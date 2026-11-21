<#
.SYNOPSIS
    Build & install a headless Tachyon multiplexer host (`tachyon.exe`) on a
    Windows PC, add `%USERPROFILE%\.local\bin` to User PATH, and optionally
    register an OnLogon Scheduled Task (with optional direct Tailscale TCP).

.EXAMPLE
    .\scripts\deploy-host.ps1 -Service
    .\scripts\deploy-host.ps1 -Service -Listen tailscale
#>
[CmdletBinding()]
param(
    [switch]$Service,
    [string]$Listen = ""
)

# PowerShell 5.1 treats native stderr output (like Cargo progress lines) as a
# terminating error when ErrorActionPreference is "Stop", so check $LASTEXITCODE
# explicitly around native commands.
$ErrorActionPreference = "Continue"
$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
Push-Location $RepoRoot

try {
    Write-Host "Building headless tachyon.exe (ConPTY server + CLI)..." -ForegroundColor Cyan
    cargo build --release --no-default-features
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }

    $Built = $null
    $JsonLines = cargo build --release --no-default-features --quiet --message-format=json 2>$null
    foreach ($Line in $JsonLines) {
        try {
            $Msg = $Line | ConvertFrom-Json
            if ($Msg.executable -and $Msg.executable.EndsWith("tachyon.exe")) {
                $Built = $Msg.executable
            }
        } catch {}
    }
    if (-not $Built) {
        $Built = Join-Path $RepoRoot "target\release\tachyon.exe"
    }
    if (-not (Test-Path $Built)) {
        throw "Expected release binary at $Built"
    }

    $BinDir = Join-Path $env:USERPROFILE ".local\bin"
    $ShellDir = Join-Path $env:USERPROFILE ".local\share\tachyon\shell"
    New-Item -ItemType Directory -Force -Path $BinDir, $ShellDir | Out-Null

    $TargetExe = Join-Path $BinDir "tachyon.exe"

    # Stop any running local server cleanly before replacing the binary.
    if (Test-Path $TargetExe) {
        & $TargetExe server stop 2>$null | Out-Null
        Start-Sleep -Milliseconds 300
    }

    Copy-Item -Force $Built $TargetExe
    if (Test-Path (Join-Path $RepoRoot "shell")) {
        Copy-Item -Recurse -Force (Join-Path $RepoRoot "shell\*") $ShellDir
    }

    # Ensure %USERPROFILE%\.local\bin is on User PATH so OpenSSH `tachyon bridge` finds it.
    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $Parts = @()
    if ($UserPath) { $Parts = $UserPath -split ';' | Where-Object { $_ -ne '' } }
    if ($Parts -notcontains $BinDir) {
        $NewPath = (@($BinDir) + $Parts) -join ';'
        [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
        $env:Path = "$BinDir;$env:Path"
        Write-Host "Added $BinDir to User PATH." -ForegroundColor Green
    }

    if ($Service) {
        Write-Host "Installing persistent Tachyon Server logon task..." -ForegroundColor Cyan
        $SvcArgs = @("server", "install-service")
        if ($Listen) { $SvcArgs += @("--listen", $Listen) }
        & $TargetExe @SvcArgs
        if ($LASTEXITCODE -ne 0) { throw "tachyon server install-service failed (exit $LASTEXITCODE)" }
        Start-Sleep -Milliseconds 400
    }

    Write-Host "Verifying local Windows named-pipe server..." -ForegroundColor Cyan
    & $TargetExe hosts test local
    if ($LASTEXITCODE -ne 0) { throw "tachyon hosts test local failed (exit $LASTEXITCODE)" }

    Write-Host ""
    Write-Host "Installed $TargetExe." -ForegroundColor Green
    Write-Host "From your MacBook (via Windows OpenSSH Server), run:"
    Write-Host "  tachyon hosts add $env:COMPUTERNAME ssh://<user>@<windows-host> --bin `"$TargetExe`""

    if ($Listen) {
        $Token = (& $TargetExe server token).Trim()
        Write-Host ""
        Write-Host "Or connect directly over Tailscale TCP (port 7501):"
        Write-Host "  tachyon hosts add $env:COMPUTERNAME tcp://<tailscale-ip>:7501 --token $Token"
    }
} finally {
    Pop-Location
}

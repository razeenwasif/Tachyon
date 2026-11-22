#!/bin/zsh
# Deploy a headless `tachyon` multiplexer server + CLI binary and shell
# integration scripts to a remote macOS or Linux SSH host (e.g. Mac mini
# `kagutsuchi`, Linux PC `amaterasu-linux`, `forge`), and optionally install &
# start its persistent user service (`launchd` on macOS, `systemd --user` on
# Linux) with optional direct Tailscale TCP (`--listen`).
#
# Usage:
#   scripts/deploy-host.sh kagutsuchi                         # auto-detect remote OS/arch
#   scripts/deploy-host.sh kagutsuchi --service               # ...and install launchd/systemd service
#   scripts/deploy-host.sh amaterasu-linux --service          # auto remote-builds on Linux + systemd
#   scripts/deploy-host.sh forge --service --listen tailscale # ...plus direct Tailscale TCP (port 7501)
set -euo pipefail

if [[ $# -lt 1 ]]; then
    echo "usage: $0 <ssh-host> [--service] [--remote-build] [--listen <addr|tailscale>]" >&2
    exit 1
fi

host=$1
shift
install_service=0
remote_build=0
listen_addr=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --service) install_service=1; shift ;;
        --remote-build) remote_build=1; shift ;;
        --listen)
            [[ $# -ge 2 ]] || { echo "--listen requires an address or 'tailscale'" >&2; exit 1; }
            listen_addr=$2
            shift 2
            ;;
        *) echo "unknown option: $1" >&2; exit 1 ;;
    esac
done

root=${0:A:h:h}
cd "$root"

local_os=$(uname -s)
local_arch=$(uname -m)
remote_info=$(ssh -o BatchMode=yes "$host" 'uname -s; uname -m' 2>/dev/null || true)
remote_os=$(echo "$remote_info" | sed -n '1p')
remote_arch=$(echo "$remote_info" | sed -n '2p')

if [[ $remote_build -eq 0 && ( "$remote_os" != "$local_os" || "$remote_arch" != "$local_arch" ) ]]; then
    echo "remote host $host is ${remote_os:-unknown}/${remote_arch:-unknown} (local is ${local_os}/${local_arch}) — switching to --remote-build"
    remote_build=1
fi

if [[ $remote_build -eq 1 ]]; then
    echo "building headless tachyon on $host (${remote_os:-remote}/${remote_arch:-unknown})..."
    build_sha=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)
    ssh "$host" 'mkdir -p ~/.local/bin ~/.local/share/tachyon/shell ~/.cache/tachyon-src'
    rsync -az --delete --exclude target --exclude dist --exclude .git ./ "$host:~/.cache/tachyon-src/"
    ssh "$host" "export PATH=\"\$HOME/.cargo/bin:\$PATH\" CARGO_TARGET_DIR=\"\$HOME/.cache/tachyon-src/target\" TACHYON_BUILD_SHA=\"$build_sha\"; cd ~/.cache/tachyon-src && cargo build --release --no-default-features && cp target/release/tachyon ~/.local/bin/tachyon.new && chmod +x ~/.local/bin/tachyon.new && mv -f ~/.local/bin/tachyon.new ~/.local/bin/tachyon && cp -R shell/* ~/.local/share/tachyon/shell/"
else
    echo "building headless tachyon (${local_os}/${local_arch})..."
    cargo build --release --no-default-features
    bin=$(cargo build --release --no-default-features --message-format=json 2>/dev/null \
        | sed -n 's/.*"executable":"\([^"]*\/tachyon\)".*/\1/p' | tail -1)
    [[ -x "$bin" ]] || { echo "release binary not found" >&2; exit 1; }
    codesign --force --sign - "$bin" >/dev/null 2>&1 || true

    echo "deploying to $host:~/.local/bin/tachyon..."
    ssh "$host" 'mkdir -p ~/.local/bin ~/.local/share/tachyon/shell'
    scp -q "$bin" "$host:~/.local/bin/tachyon.new"
    scp -qr shell/* "$host:~/.local/share/tachyon/shell/"
    ssh "$host" 'chmod +x ~/.local/bin/tachyon.new && mv -f ~/.local/bin/tachyon.new ~/.local/bin/tachyon && (codesign --force --sign - ~/.local/bin/tachyon >/dev/null 2>&1 || true)'
fi

if [[ $install_service -eq 1 ]]; then
    echo "installing persistent user service on $host..."
    svc_flags=""
    if [[ -n "$listen_addr" ]]; then
        svc_flags="--listen $listen_addr"
    fi
    ssh "$host" "~/.local/bin/tachyon server snapshot >/dev/null 2>&1 || true; ~/.local/bin/tachyon server stop >/dev/null 2>&1 || true; ~/.local/bin/tachyon server install-service $svc_flags"
fi

echo "verifying remote host over SSH bridge..."
local_bin=$(cargo build --release --no-default-features --message-format=json 2>/dev/null \
    | sed -n 's/.*"executable":"\([^"]*\/tachyon\)".*/\1/p' | tail -1)
"$local_bin" hosts add "$host" "ssh://$host" >/dev/null
"$local_bin" hosts test "$host"

if [[ -n "$listen_addr" ]]; then
    tok=$(ssh "$host" '~/.local/bin/tachyon server token' 2>/dev/null || true)
    if [[ -n "$tok" ]]; then
        echo ""
        echo "Direct TCP listener configured (--listen $listen_addr)."
        echo "To connect directly over Tailscale instead of SSH, run:"
        echo "  tachyon hosts add ${host}-tcp tcp://<tailscale-ip>:7501 --token $tok"
    fi
fi

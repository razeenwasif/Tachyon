//! Fleet health diagnostics (`tachyon doctor`) and one-click upgrades (`tachyon hosts upgrade`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};

use super::client::MuxClient;
use super::model::{HostEntry, HostInfo};
use super::proto::PROTOCOL_VERSION;
use super::{default_local_uri, resolve_target, Target};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostHealth {
    Healthy,
    UpdateAvailable,
    Degraded,
    Offline,
}

impl HostHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::UpdateAvailable => "update_available",
            Self::Degraded => "degraded",
            Self::Offline => "offline",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Healthy => "Healthy",
            Self::UpdateAvailable => "Update available",
            Self::Degraded => "Degraded",
            Self::Offline => "Offline",
        }
    }

    pub fn dot(self) -> &'static str {
        match self {
            Self::Healthy => "🟢",
            Self::UpdateAvailable => "🟠",
            Self::Degraded => "🟡",
            Self::Offline => "🔴",
        }
    }

    pub fn is_ok(self, strict: bool) -> bool {
        match self {
            Self::Healthy => true,
            Self::UpdateAvailable => !strict,
            Self::Degraded | Self::Offline => false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HostDiagnostic {
    pub alias: String,
    pub uri: String,
    pub transport: String,
    pub is_local: bool,
    pub health: HostHealth,
    pub info: Option<HostInfo>,
    pub rtt_ms: Option<f64>,
    pub sessions: usize,
    pub blocks: usize,
    pub clients: usize,
    pub auth_status: String,
    pub capabilities: Vec<String>,
    pub missing_capabilities: Vec<String>,
    pub update_available: bool,
    pub issues: Vec<String>,
    pub recommendations: Vec<String>,
}

impl HostDiagnostic {
    pub fn to_json(&self) -> Value {
        json!({
            "alias": self.alias,
            "uri": self.uri,
            "transport": self.transport,
            "is_local": self.is_local,
            "health": self.health.as_str(),
            "health_label": self.health.label(),
            "host": self.info.as_ref().map(HostInfo::to_json),
            "rtt_ms": self.rtt_ms,
            "sessions": self.sessions,
            "blocks": self.blocks,
            "clients": self.clients,
            "auth_status": self.auth_status,
            "capabilities": self.capabilities,
            "missing_capabilities": self.missing_capabilities,
            "update_available": self.update_available,
            "issues": self.issues,
            "recommendations": self.recommendations,
        })
    }
}

pub fn transport_for_uri(uri: &str, is_local: bool) -> &'static str {
    if is_local || uri.starts_with("unix://") || uri.starts_with(r"\\.\pipe\") {
        "local"
    } else if uri.starts_with("tcp://") {
        "tcp"
    } else {
        "ssh"
    }
}

/// Evaluate host health, version/build skew, RTT, auth posture, and missing capabilities.
#[allow(clippy::too_many_arguments)]
pub fn diagnose_from_status(
    alias: &str,
    uri: &str,
    is_local: bool,
    info: Option<&HostInfo>,
    rtt_ms: Option<f64>,
    sessions: usize,
    blocks: usize,
    clients: usize,
    error: Option<&str>,
) -> HostDiagnostic {
    let transport = transport_for_uri(uri, is_local).to_string();
    let Some(info) = info else {
        let err_msg = error.unwrap_or("unreachable");
        let rec = if is_local {
            "tachyon server start".to_string()
        } else if transport == "tcp" {
            format!("check Tailscale/TCP connectivity or re-pair `{alias}`")
        } else {
            format!("verify SSH access (`ssh {alias}`) or run `tachyon hosts upgrade {alias}`")
        };
        return HostDiagnostic {
            alias: alias.to_string(),
            uri: uri.to_string(),
            transport,
            is_local,
            health: HostHealth::Offline,
            info: None,
            rtt_ms: None,
            sessions: 0,
            blocks: 0,
            clients: 0,
            auth_status: "unreachable".into(),
            capabilities: Vec::new(),
            missing_capabilities: crate::LOCAL_CAPABILITIES
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            update_available: false,
            issues: vec![err_msg.to_string()],
            recommendations: vec![rec],
        };
    };

    let missing_capabilities: Vec<String> = crate::LOCAL_CAPABILITIES
        .iter()
        .filter(|cap| !info.capabilities.iter().any(|c| c == *cap))
        .map(|s| (*s).to_string())
        .collect();

    let version_mismatch = info.version != crate::APP_VERSION;
    let build_mismatch = crate::APP_BUILD_SHA != "unknown"
        && info.build.as_deref() != Some(crate::APP_BUILD_SHA)
        && (info.build.is_some() || info.capabilities.is_empty());
    let update_available = !missing_capabilities.is_empty() || version_mismatch || build_mismatch;

    let mut issues = Vec::new();
    let mut recommendations = Vec::new();
    if let Some(err) = error {
        issues.push(err.to_string());
    }
    if info.protocol != PROTOCOL_VERSION {
        issues.push(format!(
            "protocol mismatch: host reports v{}, client expects v{PROTOCOL_VERSION}",
            info.protocol
        ));
    }
    if let Some(rtt) = rtt_ms {
        if rtt > 300.0 {
            issues.push(format!("high transport latency ({rtt:.1} ms)"));
        }
    }
    if version_mismatch {
        issues.push(format!(
            "version skew: host v{}, local v{}",
            info.version, crate::APP_VERSION
        ));
    } else if build_mismatch {
        issues.push(format!(
            "build skew: host {}, local {}",
            info.build.as_deref().unwrap_or("legacy"),
            crate::APP_BUILD_SHA
        ));
    }
    if !missing_capabilities.is_empty() {
        issues.push(format!(
            "missing capabilities: {}",
            missing_capabilities.join(", ")
        ));
    }
    if transport == "tcp" && info.auth_enabled != Some(true) {
        issues.push("TCP listener without bearer auth".into());
    }

    let auth_status = match (transport.as_str(), info.auth_enabled) {
        ("local", Some(true)) => "unix + bearer token".into(),
        ("local", _) => "unix/pipe socket".into(),
        ("tcp", Some(true)) => "bearer token ✓".into(),
        ("tcp", _) => "tcp (unauthenticated)".into(),
        ("ssh", Some(true)) => "ssh + bearer token".into(),
        _ => "ssh bridge".into(),
    };

    let health = if info.protocol != PROTOCOL_VERSION
        || rtt_ms.is_some_and(|r| r > 300.0)
        || (transport == "tcp" && info.auth_enabled == Some(false))
    {
        HostHealth::Degraded
    } else if update_available {
        HostHealth::UpdateAvailable
    } else {
        HostHealth::Healthy
    };

    if update_available || info.protocol != PROTOCOL_VERSION {
        recommendations.push(format!("tachyon hosts upgrade {alias}"));
    }

    HostDiagnostic {
        alias: alias.to_string(),
        uri: uri.to_string(),
        transport,
        is_local,
        health,
        info: Some(info.clone()),
        rtt_ms,
        sessions,
        blocks,
        clients,
        auth_status,
        capabilities: info.capabilities.clone(),
        missing_capabilities,
        update_available,
        issues,
        recommendations,
    }
}

/// Probe one Mux v2 target (auto_start = false) and classify its health/capabilities.
pub fn diagnose_host(
    alias: &str,
    uri: &str,
    target: &Target,
    is_local: bool,
    timeout: Duration,
) -> HostDiagnostic {
    let client = match MuxClient::connect(target, false, "cli", "tachyon-doctor") {
        Ok(c) => c,
        Err(e) => {
            return diagnose_from_status(
                alias,
                uri,
                is_local,
                None,
                None,
                0,
                0,
                0,
                Some(&e.to_string()),
            );
        }
    };
    let rtt_ms = client
        .ping(timeout)
        .ok()
        .map(|d| d.as_secs_f64() * 1000.0);
    match client.state() {
        Ok(snap) => diagnose_from_status(
            alias,
            uri,
            is_local,
            Some(&snap.host),
            rtt_ms,
            snap.sessions.len(),
            snap.blocks.len(),
            snap.clients.len(),
            None,
        ),
        Err(e) => diagnose_from_status(
            alias,
            uri,
            is_local,
            Some(&client.host),
            rtt_ms,
            0,
            0,
            0,
            Some(&e.to_string()),
        ),
    }
}

/// Probe all configured hosts (and optionally `local`) and return ordered diagnostics.
pub fn diagnose_fleet(
    hosts: &[HostEntry],
    include_local: bool,
    timeout: Duration,
) -> Vec<HostDiagnostic> {
    let mut targets: Vec<(String, String, Target, bool)> = Vec::new();
    if include_local {
        if let Ok(t) = resolve_target(Some("local"), hosts) {
            targets.push(("local".into(), default_local_uri(), t, true));
        }
    }
    for h in hosts {
        if h.alias == "local" {
            continue;
        }
        match resolve_target(Some(&h.alias), hosts) {
            Ok(t) => targets.push((h.alias.clone(), h.uri.clone(), t, false)),
            Err(e) => {
                // Record invalid URI directly without spawning a thread.
                targets.push((
                    h.alias.clone(),
                    format!("error:{e}"),
                    Target::Tcp {
                        alias: h.alias.clone(),
                        addr: String::new(),
                        token: None,
                    },
                    false,
                ));
            }
        }
    }

    let mut handles = Vec::with_capacity(targets.len());
    for (idx, (alias, uri, target, is_local)) in targets.into_iter().enumerate() {
        handles.push(std::thread::spawn(move || {
            let diag = if let Some(err) = uri.strip_prefix("error:") {
                diagnose_from_status(&alias, "", is_local, None, None, 0, 0, 0, Some(err))
            } else {
                diagnose_host(&alias, &uri, &target, is_local, timeout)
            };
            (idx, diag)
        }));
    }

    let mut indexed = Vec::with_capacity(handles.len());
    for h in handles {
        if let Ok(item) = h.join() {
            indexed.push(item);
        }
    }
    indexed.sort_by_key(|(idx, _)| *idx);
    indexed.into_iter().map(|(_, d)| d).collect()
}

#[derive(Clone, Debug, Default)]
pub struct UpgradeOptions {
    pub dry_run: bool,
    pub remote_build: bool,
    pub install_service: bool,
    pub listen: Option<String>,
    pub ssh_override: Option<String>,
}

#[derive(Clone, Debug)]
pub struct UpgradePlan {
    pub alias: String,
    pub strategy: String,
    pub ssh_dest: Option<String>,
    pub ssh_port: Option<u16>,
    pub steps: Vec<String>,
    pub note: Option<String>,
}

impl UpgradePlan {
    pub fn to_json(&self) -> Value {
        json!({
            "alias": self.alias,
            "strategy": self.strategy,
            "ssh_dest": self.ssh_dest,
            "ssh_port": self.ssh_port,
            "steps": self.steps,
            "note": self.note,
        })
    }
}

#[derive(Clone, Debug)]
pub struct UpgradeResult {
    pub plan: UpgradePlan,
    pub executed: bool,
    pub after: Option<HostDiagnostic>,
}

impl UpgradeResult {
    pub fn to_json(&self) -> Value {
        json!({
            "executed": self.executed,
            "plan": self.plan.to_json(),
            "after": self.after.as_ref().map(HostDiagnostic::to_json),
        })
    }
}

/// Discover the Tachyon repository checkout containing `Cargo.toml` and `scripts/deploy-host.sh`.
pub fn find_repo_root() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("TACHYON_REPO").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(p);
        if p.join("Cargo.toml").exists() && p.join("scripts/deploy-host.sh").exists() {
            return Some(p);
        }
    }
    let mut candidates = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        candidates.push(exe);
    }
    for start in candidates {
        let mut cur = start.as_path();
        loop {
            if cur.join("Cargo.toml").exists() && cur.join("scripts/deploy-host.sh").exists() {
                return Some(cur.to_path_buf());
            }
            let Some(parent) = cur.parent() else {
                break;
            };
            cur = parent;
        }
    }
    None
}

fn find_ssh_partner(alias: &str, uri: &str, hosts: &[HostEntry]) -> Option<(String, Option<u16>, Option<String>)> {
    let candidates = [
        alias.strip_suffix("-tcp"),
        alias.strip_suffix("_tcp"),
    ];
    for cand in candidates.into_iter().flatten() {
        if let Some(e) = hosts.iter().find(|h| h.alias == cand) {
            if let Ok(Target::Ssh {
                destination,
                port,
                remote_bin,
                ..
            }) = resolve_target(Some(&e.alias), hosts)
            {
                return Some((destination, port, remote_bin));
            }
        }
    }
    let tcp_host = uri
        .strip_prefix("tcp://")
        .and_then(|s| s.split(':').next())
        .unwrap_or("");
    if !tcp_host.is_empty() {
        for e in hosts {
            if let Ok(Target::Ssh {
                destination,
                port,
                remote_bin,
                ..
            }) = resolve_target(Some(&e.alias), hosts)
            {
                let bare = destination.split('@').next_back().unwrap_or(&destination);
                if bare == tcp_host || e.alias == tcp_host {
                    return Some((destination, port, remote_bin));
                }
            }
        }
    }
    None
}

/// Build an upgrade plan for `alias` without executing side effects.
pub fn build_upgrade_plan(
    alias: &str,
    hosts: &[HostEntry],
    remote_hint: Option<(&str, &str)>,
    repo_root: Option<&Path>,
    current_exe: Option<&Path>,
    opts: &UpgradeOptions,
) -> Result<UpgradePlan, String> {
    let target = resolve_target(Some(alias), hosts).map_err(|e| e.to_string())?;
    let is_win_local = cfg!(target_os = "windows");

    match target {
        Target::Local { .. } => Ok(UpgradePlan {
            alias: "local".into(),
            strategy: "local-restart".into(),
            ssh_dest: None,
            ssh_port: None,
            steps: vec![
                "tachyon server snapshot".into(),
                "tachyon server stop".into(),
                "tachyon server start".into(),
            ],
            note: Some("Flushes active sessions to disk and restarts local `tachyon server` with the current binary.".into()),
        }),
        Target::Ssh {
            destination,
            port,
            remote_bin,
            ..
        } => {
            let is_win_remote = remote_hint
                .is_some_and(|(os, _)| os.eq_ignore_ascii_case("windows"))
                || remote_bin
                    .as_deref()
                    .is_some_and(|b| b.to_ascii_lowercase().ends_with(".exe") || b.contains('\\'));
            if is_win_remote {
                let mut ps = ".\\scripts\\deploy-host.ps1".to_string();
                if opts.install_service {
                    ps.push_str(" -Service");
                }
                if let Some(ref l) = opts.listen {
                    ps.push_str(&format!(" -Listen {l}"));
                }
                return Ok(UpgradePlan {
                    alias: alias.to_string(),
                    strategy: "ssh-windows-ps1".into(),
                    ssh_dest: Some(destination.clone()),
                    ssh_port: port,
                    steps: vec![
                        format!("ssh {destination} tachyon.exe server snapshot"),
                        format!("ssh {destination} powershell -NoProfile -ExecutionPolicy Bypass -Command \"cd $HOME/Tachyon; git pull --ff-only; {ps}\""),
                    ],
                    note: Some("Windows hosts build/restart via `scripts/deploy-host.ps1` (OnLogon task + ConPTY server).".into()),
                });
            }

            if !is_win_local && port.is_none() {
                if let Some(root) = repo_root {
                    let script = root.join("scripts/deploy-host.sh");
                    if script.exists() {
                        let mut cmd = format!("{} {}", script.display(), destination);
                        if opts.install_service {
                            cmd.push_str(" --service");
                        }
                        if opts.remote_build {
                            cmd.push_str(" --remote-build");
                        }
                        if let Some(ref l) = opts.listen {
                            cmd.push_str(&format!(" --listen {l}"));
                        }
                        return Ok(UpgradePlan {
                            alias: alias.to_string(),
                            strategy: "deploy-host-sh".into(),
                            ssh_dest: Some(destination),
                            ssh_port: None,
                            steps: vec![cmd],
                            note: Some("Uses `scripts/deploy-host.sh` with automatic OS/arch detection, `server snapshot`, binary swap, and service restart.".into()),
                        });
                    }
                }
            }

            let exe_str = current_exe
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "tachyon".to_string());
            Ok(UpgradePlan {
                alias: alias.to_string(),
                strategy: "ssh-scp-binary".into(),
                ssh_dest: Some(destination.clone()),
                ssh_port: port,
                steps: vec![
                    format!("ssh {destination} '~/.local/bin/tachyon server snapshot >/dev/null 2>&1 || true'"),
                    format!("scp {exe_str} {destination}:~/.local/bin/tachyon.new"),
                    format!("ssh {destination} 'chmod +x ~/.local/bin/tachyon.new && mv -f ~/.local/bin/tachyon.new ~/.local/bin/tachyon && ~/.local/bin/tachyon server stop >/dev/null 2>&1 || true && ~/.local/bin/tachyon server start'"),
                ],
                note: Some("Fallback binary-copy upgrade when running outside a Tachyon source checkout.".into()),
            })
        }
        Target::Tcp { addr, .. } => {
            let ssh_dest = if let Some(ref over) = opts.ssh_override {
                Some((over.clone(), None))
            } else {
                find_ssh_partner(alias, &format!("tcp://{addr}"), hosts)
                    .map(|(d, p, _)| (d, p))
            };
            let Some((dest, port)) = ssh_dest else {
                return Err(format!(
                    "TCP host `{alias}` (`tcp://{addr}`) needs an SSH host for upgrades — pass `--ssh <user@host>` or add a matching `ssh://` entry"
                ));
            };
            if let Some(root) = repo_root {
                let script = root.join("scripts/deploy-host.sh");
                if script.exists() && port.is_none() && !is_win_local {
                    let mut cmd = format!("{} {}", script.display(), dest);
                    if opts.install_service {
                        cmd.push_str(" --service");
                    }
                    if opts.remote_build {
                        cmd.push_str(" --remote-build");
                    }
                    let listen = opts.listen.as_deref().unwrap_or("tailscale");
                    cmd.push_str(&format!(" --listen {listen}"));
                    return Ok(UpgradePlan {
                        alias: alias.to_string(),
                        strategy: "deploy-host-sh".into(),
                        ssh_dest: Some(dest),
                        ssh_port: None,
                        steps: vec![cmd],
                        note: Some(format!("Upgrades `{alias}` over paired SSH destination while preserving `--listen {listen}`.")),
                    });
                }
            }
            let exe_str = current_exe
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "tachyon".to_string());
            Ok(UpgradePlan {
                alias: alias.to_string(),
                strategy: "ssh-scp-binary".into(),
                ssh_dest: Some(dest.clone()),
                ssh_port: port,
                steps: vec![
                    format!("ssh {dest} '~/.local/bin/tachyon server snapshot >/dev/null 2>&1 || true'"),
                    format!("scp {exe_str} {dest}:~/.local/bin/tachyon.new"),
                    format!("ssh {dest} 'chmod +x ~/.local/bin/tachyon.new && mv -f ~/.local/bin/tachyon.new ~/.local/bin/tachyon && ~/.local/bin/tachyon server stop >/dev/null 2>&1 || true && ~/.local/bin/tachyon server start'"),
                ],
                note: None,
            })
        }
    }
}

fn run_checked(cmd: &mut Command, label: &str) -> Result<(), String> {
    let out = cmd
        .output()
        .map_err(|e| format!("{label} failed to spawn: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let msg = if !stderr.is_empty() { stderr } else { stdout };
    Err(format!("{label} failed (exit {:?}): {msg}", out.status.code()))
}

/// Execute (or dry-run) an upgrade for `alias`, returning the plan and post-upgrade diagnostic.
pub fn execute_upgrade(
    alias: &str,
    hosts: &[HostEntry],
    opts: &UpgradeOptions,
) -> Result<UpgradeResult, String> {
    let target = resolve_target(Some(alias), hosts).map_err(|e| e.to_string())?;
    let pre_diag = (!opts.dry_run).then(|| {
        diagnose_host(
            alias,
            hosts.iter().find(|h| h.alias == alias).map_or("", |h| h.uri.as_str()),
            &target,
            alias == "local",
            Duration::from_secs(2),
        )
    });
    let hint = pre_diag
        .as_ref()
        .and_then(|d| d.info.as_ref())
        .map(|i| (i.os.as_str(), i.arch.as_str()));
    let repo_root = find_repo_root();
    let cur_exe = std::env::current_exe().ok();
    let plan = build_upgrade_plan(alias, hosts, hint, repo_root.as_deref(), cur_exe.as_deref(), opts)?;

    if opts.dry_run {
        return Ok(UpgradeResult {
            plan,
            executed: false,
            after: None,
        });
    }

    match plan.strategy.as_str() {
        "local-restart" => {
            let exe = cur_exe.unwrap_or_else(|| PathBuf::from("tachyon"));
            // Flush session snapshot cleanly if daemon is reachable, then wait
            // until the dying daemon releases its socket before starting anew.
            let _ = Command::new(&exe).args(["server", "snapshot"]).output();
            let stop_ok = Command::new(&exe)
                .args(["server", "stop"])
                .output()
                .is_ok_and(|o| o.status.success());
            if stop_ok {
                let sock = super::default_socket_path();
                let wait_end = std::time::Instant::now() + Duration::from_secs(3);
                while std::time::Instant::now() < wait_end {
                    let local_t = Target::Local { socket: sock.clone() };
                    if MuxClient::connect(&local_t, false, "cli", "upgrade-wait").is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
            run_checked(Command::new(&exe).args(["server", "start"]), "local `server start`")?;
        }
        "deploy-host-sh" => {
            let root = repo_root.ok_or("repository root with scripts/deploy-host.sh not found")?;
            let script = root.join("scripts/deploy-host.sh");
            let dest = plan.ssh_dest.as_deref().ok_or("missing SSH destination")?;
            let mut cmd = Command::new("zsh");
            cmd.arg(&script).arg(dest);
            if opts.install_service {
                cmd.arg("--service");
            }
            if opts.remote_build {
                cmd.arg("--remote-build");
            }
            if let Some(ref l) = opts.listen {
                cmd.args(["--listen", l]);
            }
            run_checked(&mut cmd, "scripts/deploy-host.sh")?;
        }
        "ssh-scp-binary" => {
            let dest = plan.ssh_dest.as_deref().ok_or("missing SSH destination")?;
            let exe = cur_exe.ok_or("cannot locate current tachyon binary")?;
            let mut snap_cmd = Command::new("ssh");
            if let Some(p) = plan.ssh_port {
                snap_cmd.args(["-p", &p.to_string()]);
            }
            let _ = snap_cmd
                .args([dest, "~/.local/bin/tachyon server snapshot >/dev/null 2>&1 || true"])
                .output();

            let mut scp = Command::new("scp");
            if let Some(p) = plan.ssh_port {
                scp.args(["-P", &p.to_string()]);
            }
            scp.arg("-q")
                .arg(&exe)
                .arg(format!("{dest}:~/.local/bin/tachyon.new"));
            run_checked(&mut scp, "scp tachyon")?;

            let mut finish = Command::new("ssh");
            if let Some(p) = plan.ssh_port {
                finish.args(["-p", &p.to_string()]);
            }
            let remote_cmd = "~/.local/bin/tachyon server stop >/dev/null 2>&1 || true; chmod +x ~/.local/bin/tachyon.new && mv -f ~/.local/bin/tachyon.new ~/.local/bin/tachyon && (codesign --force --sign - ~/.local/bin/tachyon >/dev/null 2>&1 || true) && ~/.local/bin/tachyon server start";
            finish.args([dest, remote_cmd]);
            run_checked(&mut finish, "remote tachyon restart")?;
        }
        "ssh-windows-ps1" => {
            let dest = plan.ssh_dest.as_deref().ok_or("missing SSH destination")?;
            let mut snap_cmd = Command::new("ssh");
            if let Some(p) = plan.ssh_port {
                snap_cmd.args(["-p", &p.to_string()]);
            }
            let _ = snap_cmd.args([dest, "tachyon.exe server snapshot"]).output();
            let mut ps_flags = String::from(".\\scripts\\deploy-host.ps1");
            if opts.install_service {
                ps_flags.push_str(" -Service");
            }
            if let Some(ref l) = opts.listen {
                ps_flags.push_str(&format!(" -Listen {l}"));
            }
            let mut ps = Command::new("ssh");
            if let Some(p) = plan.ssh_port {
                ps.args(["-p", &p.to_string()]);
            }
            ps.args([
                dest,
                &format!(
                    "powershell -NoProfile -ExecutionPolicy Bypass -Command \"cd $HOME\\Tachyon; git pull --ff-only; {ps_flags}\""
                ),
            ]);
            run_checked(&mut ps, "remote Windows deploy-host.ps1")?;
        }
        other => return Err(format!("unsupported upgrade strategy `{other}`")),
    }

    let uri = if alias == "local" {
        default_local_uri()
    } else {
        hosts
            .iter()
            .find(|h| h.alias == alias)
            .map_or_else(String::new, |h| h.uri.clone())
    };
    let after = diagnose_host(alias, &uri, &target, alias == "local", Duration::from_secs(4));
    Ok(UpgradeResult {
        plan,
        executed: true,
        after: Some(after),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_info_capabilities_and_health_classification() {
        let current = HostInfo {
            hostname: "tsukuyomi".into(),
            os: "macos".into(),
            arch: "aarch64".into(),
            version: crate::APP_VERSION.to_string(),
            protocol: PROTOCOL_VERSION,
            pid: 42,
            build: (crate::APP_BUILD_SHA != "unknown").then(|| crate::APP_BUILD_SHA.to_string()),
            capabilities: crate::LOCAL_CAPABILITIES
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            auth_enabled: Some(true),
        };
        let d_ok = diagnose_from_status(
            "local",
            "unix:///tmp/test.sock",
            true,
            Some(&current),
            Some(0.4),
            2,
            3,
            1,
            None,
        );
        assert_eq!(d_ok.health, HostHealth::Healthy);
        assert!(d_ok.missing_capabilities.is_empty());
        assert!(!d_ok.update_available);

        // Stale remote missing recent capabilities -> UpdateAvailable.
        let mut stale = current.clone();
        stale.build = Some("000dead".into());
        stale.capabilities = vec!["block.rewind".into()];
        let d_stale = diagnose_from_status(
            "kagutsuchi",
            "ssh://kagutsuchi",
            false,
            Some(&stale),
            Some(4.2),
            1,
            1,
            1,
            None,
        );
        assert_eq!(d_stale.health, HostHealth::UpdateAvailable);
        assert!(d_stale.update_available);
        assert!(d_stale.missing_capabilities.contains(&"block.receipts".to_string()));
        assert!(d_stale.missing_capabilities.contains(&"images.restore".to_string()));
        assert!(d_stale.recommendations.iter().any(|r| r.contains("hosts upgrade kagutsuchi")));

        // Protocol mismatch or unauthenticated TCP -> Degraded.
        let mut bad_proto = current.clone();
        bad_proto.protocol = PROTOCOL_VERSION + 99;
        let d_proto = diagnose_from_status(
            "kagutsuchi",
            "ssh://kagutsuchi",
            false,
            Some(&bad_proto),
            Some(5.0),
            0,
            0,
            0,
            None,
        );
        assert_eq!(d_proto.health, HostHealth::Degraded);

        // Unreachable -> Offline.
        let d_off = diagnose_from_status(
            "dead",
            "tcp://100.1.2.3:7501",
            false,
            None,
            None,
            0,
            0,
            0,
            Some("connection refused"),
        );
        assert_eq!(d_off.health, HostHealth::Offline);
        assert!(!d_off.health.is_ok(false));
    }

    #[test]
    fn upgrade_plan_selects_strategy_for_local_ssh_windows_and_tcp_hosts() {
        let hosts = vec![
            HostEntry {
                alias: "kagutsuchi".into(),
                uri: "ssh://kagutsuchi".into(),
                remote_bin: None,
                token: None,
                color: None,
            },
            HostEntry {
                alias: "amaterasu".into(),
                uri: r"ssh://razeen@amaterasu".into(),
                remote_bin: Some(r"C:\Users\Razeen\.local\bin\tachyon.exe".into()),
                token: None,
                color: None,
            },
            HostEntry {
                alias: "amaterasu-tcp".into(),
                uri: "tcp://100.109.107.16:7501".into(),
                remote_bin: None,
                token: Some("tok".into()),
                color: None,
            },
            HostEntry {
                alias: "lonely-tcp".into(),
                uri: "tcp://100.1.2.3:7501".into(),
                remote_bin: None,
                token: Some("tok".into()),
                color: None,
            },
        ];

        let p_local = build_upgrade_plan("local", &hosts, None, None, None, &UpgradeOptions::default()).unwrap();
        assert_eq!(p_local.strategy, "local-restart");
        assert!(p_local.steps.iter().any(|s| s.contains("server snapshot")));

        let p_win = build_upgrade_plan(
            "amaterasu",
            &hosts,
            Some(("windows", "x86_64")),
            None,
            None,
            &UpgradeOptions {
                install_service: true,
                listen: Some("tailscale".into()),
                ..UpgradeOptions::default()
            },
        )
        .unwrap();
        assert_eq!(p_win.strategy, "ssh-windows-ps1");
        assert!(p_win.steps.iter().any(|s| s.contains("deploy-host.ps1")));

        // Paired TCP alias falls back to sibling SSH entry (`amaterasu`) if no `--ssh` passed.
        let p_tcp = build_upgrade_plan(
            "amaterasu-tcp",
            &hosts,
            None,
            None,
            Some(Path::new("/tmp/tachyon")),
            &UpgradeOptions::default(),
        )
        .unwrap();
        assert_eq!(p_tcp.ssh_dest.as_deref(), Some("razeen@amaterasu"));

        // Unpaired TCP without `--ssh` gives clear actionable error.
        let err = build_upgrade_plan("lonely-tcp", &hosts, None, None, None, &UpgradeOptions::default())
            .unwrap_err();
        assert!(err.contains("--ssh"));

        // SSH with repo checkout uses scripts/deploy-host.sh on Unix.
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        if !cfg!(target_os = "windows") && repo.join("scripts/deploy-host.sh").exists() {
            let p_mac = build_upgrade_plan(
                "kagutsuchi",
                &hosts,
                Some(("macos", "aarch64")),
                Some(&repo),
                None,
                &UpgradeOptions {
                    install_service: true,
                    ..UpgradeOptions::default()
                },
            )
            .unwrap();
            assert_eq!(p_mac.strategy, "deploy-host-sh");
            assert!(p_mac.steps[0].contains("--service"));
        }
    }
}

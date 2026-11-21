//! Tachyon automation and server CLI (`tachyon ls`, `split`, `run`, `send`,
//! `capture`, `events`, `hosts`, `server`, `bridge`, …).
//!
//! Works identically in the full macOS/Windows desktop binary and in headless
//! `--no-default-features` server builds on Mac mini, Linux, and Windows.

use std::io::{self, Read};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

use crate::mux::client::MuxClient;
use crate::mux::model::{HostColor, HostEntry, SplitDir};
use crate::mux::{
    default_local_uri, default_socket_path, default_token_path, generate_auth_token, load_hosts,
    load_or_create_token, resolve_target, save_hosts, transport, write_token_file, Target,
};
use crate::server::{self, RunOptions, ServiceOptions};

/// Global CLI flags accepted before or after a subcommand.
#[derive(Default)]
struct GlobalOpts {
    server: Option<String>,
    json: bool,
}

/// Returns `true` if `args` (excluding argv[0]) should be handled by the CLI
/// rather than launching the desktop GUI window.
pub fn is_cli_invocation(args: &[String]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-S" | "--server" => {
                return true;
            }
            "--json" => {
                i += 1;
                continue;
            }
            "server" | "bridge" | "hosts" | "workspace" | "ws" | "agents" | "agent" | "ls"
            | "new" | "session" | "window" | "split" | "run" | "send" | "capture" | "events"
            | "skill" | "status" | "hook" | "help" | "--help" | "-h" => {
                return true;
            }
            _ => return false,
        }
    }
    false
}

/// Entry point for CLI subcommands. Returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    match run_inner(args) {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("tachyon: {msg}");
            1
        }
    }
}

fn run_inner(raw: &[String]) -> Result<i32, String> {
    let mut opts = GlobalOpts::default();
    let mut rest = Vec::with_capacity(raw.len());
    let mut i = 0;
    let mut after_dashdash = false;
    while i < raw.len() {
        let a = &raw[i];
        if after_dashdash {
            rest.push(a.clone());
            i += 1;
            continue;
        }
        if a == "--" {
            after_dashdash = true;
            rest.push(a.clone());
            i += 1;
            continue;
        }
        match a.as_str() {
            "-S" | "--server" => {
                let val = raw.get(i + 1).ok_or("`-S/--server` requires a host")?;
                opts.server = Some(val.clone());
                i += 2;
            }
            "--json" => {
                opts.json = true;
                i += 1;
            }
            _ => {
                rest.push(a.clone());
                i += 1;
            }
        }
    }

    let Some((cmd, tail)) = rest.split_first() else {
        print!("{HELP}");
        return Ok(0);
    };

    match cmd.as_str() {
        "help" | "--help" | "-h" => {
            print!("{HELP}");
            Ok(0)
        }
        "bridge" => cmd_bridge(tail),
        "server" => cmd_server(&opts, tail),
        "hosts" => cmd_hosts(&opts, tail),
        "workspace" | "ws" => cmd_workspace(&opts, tail),
        "agents" | "agent" => cmd_agents(&opts, tail),
        "ls" => cmd_ls(&opts),
        "new" => cmd_session_new(&opts, tail),
        "session" => cmd_session(&opts, tail),
        "window" => cmd_window(&opts, tail),
        "split" => cmd_split(&opts, tail),
        "run" => cmd_run(&opts, tail),
        "send" => cmd_send(&opts, tail),
        "capture" => cmd_capture(&opts, tail),
        "events" => cmd_events(&opts, tail),
        "skill" => cmd_skill(&opts, tail),
        "status" => cmd_status(tail),
        "hook" => cmd_hook(tail),
        other => Err(format!("unknown command `{other}` (try `tachyon help`)")),
    }
}

fn target_for(opts: &GlobalOpts) -> Result<Target, String> {
    let hosts = load_hosts();
    resolve_target(opts.server.as_deref(), &hosts).map_err(|e| e.to_string())
}

fn connect(opts: &GlobalOpts, auto_start: bool) -> Result<(Target, std::sync::Arc<MuxClient>), String> {
    let target = target_for(opts)?;
    let client = MuxClient::connect(&target, auto_start, "cli", "tachyon-cli")
        .map_err(|e| format!("could not connect to {}: {e}", target.display_name()))?;
    Ok((target, client))
}

fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()));
}

// ---------------------------------------------------------------------------
// bridge & server
// ---------------------------------------------------------------------------

fn cmd_bridge(args: &[String]) -> Result<i32, String> {
    let mut socket = default_socket_path();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--socket" {
            let p = args.get(i + 1).ok_or("--socket requires a path")?;
            socket = PathBuf::from(p);
            i += 2;
        } else {
            return Err(format!("unknown bridge option `{}`", args[i]));
        }
    }
    transport::run_bridge(&socket).map_err(|e| e.to_string())?;
    Ok(0)
}

fn cmd_server(opts: &GlobalOpts, args: &[String]) -> Result<i32, String> {
    let sub = args.first().map(String::as_str).unwrap_or("status");
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };

    let mut socket = default_socket_path();
    let mut listen: Option<String> = None;
    let mut auth_token: Option<String> = None;
    let mut token_file: Option<PathBuf> = None;
    let mut regenerate = false;

    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--socket" => {
                let p = rest.get(i + 1).ok_or("--socket requires a path")?;
                socket = PathBuf::from(p);
                i += 2;
            }
            "--listen" => {
                let addr = rest
                    .get(i + 1)
                    .ok_or("--listen requires <ip:port>, <ip>, or `tailscale`")?;
                listen = Some(addr.clone());
                i += 2;
            }
            "--token" => {
                let t = rest.get(i + 1).ok_or("--token requires a bearer token")?;
                auth_token = Some(t.clone());
                i += 2;
            }
            "--token-file" => {
                let p = rest.get(i + 1).ok_or("--token-file requires a path")?;
                token_file = Some(PathBuf::from(p));
                i += 2;
            }
            "--regenerate" => {
                regenerate = true;
                i += 1;
            }
            other => return Err(format!("unknown server option `{other}`")),
        }
    }

    match sub {
        "run" => {
            let run_opts = RunOptions { listen, auth_token, token_file };
            server::run_with_options(&socket, run_opts).map_err(|e| e.to_string())?;
            Ok(0)
        }
        "start" => {
            let target = if opts.server.is_some() {
                target_for(opts)?
            } else {
                Target::Local { socket: socket.clone() }
            };
            let c = MuxClient::connect(&target, true, "cli", "server-start")
                .map_err(|e| e.to_string())?;
            if opts.json {
                print_json(&json!({
                    "running": true,
                    "target": target.display_name(),
                    "host": c.host.to_json(),
                }));
            } else {
                println!(
                    "tachyon server running on {} (pid {}, {}/{}, v{})",
                    c.host.hostname, c.host.pid, c.host.os, c.host.arch, c.host.version
                );
            }
            Ok(0)
        }
        "stop" => {
            let target = if opts.server.is_some() {
                target_for(opts)?
            } else {
                Target::Local { socket }
            };
            let c = MuxClient::connect(&target, false, "cli", "server-stop")
                .map_err(|e| format!("server not running on {}: {e}", target.display_name()))?;
            c.shutdown_server().map_err(|e| e.to_string())?;
            if opts.json {
                print_json(&json!({ "stopped": true, "target": target.display_name() }));
            } else {
                println!("stopped tachyon server on {}", target.display_name());
            }
            Ok(0)
        }
        "status" => {
            let target = if opts.server.is_some() {
                target_for(opts)?
            } else {
                Target::Local { socket }
            };
            match MuxClient::connect(&target, false, "cli", "server-status") {
                Ok(c) => {
                    let rtt = c.ping(Duration::from_secs(3)).ok();
                    let snap = c.state().map_err(|e| e.to_string())?;
                    if opts.json {
                        print_json(&json!({
                            "running": true,
                            "target": target.display_name(),
                            "rtt_ms": rtt.map(|d| d.as_secs_f64() * 1000.0),
                            "host": snap.host.to_json(),
                            "sessions": snap.sessions.len(),
                            "windows": snap.windows.len(),
                            "blocks": snap.blocks.len(),
                            "clients": snap.clients.len(),
                        }));
                    } else {
                        let rtt_s = rtt
                            .map(|d| format!(", rtt {:.1} ms", d.as_secs_f64() * 1000.0))
                            .unwrap_or_default();
                        println!(
                            "running on {} ({}/{}, pid {}, v{}{}) — {} sessions, {} windows, {} blocks, {} clients",
                            snap.host.hostname,
                            snap.host.os,
                            snap.host.arch,
                            snap.host.pid,
                            snap.host.version,
                            rtt_s,
                            snap.sessions.len(),
                            snap.windows.len(),
                            snap.blocks.len(),
                            snap.clients.len(),
                        );
                    }
                    Ok(0)
                }
                Err(_) => {
                    if opts.json {
                        print_json(&json!({ "running": false, "target": target.display_name() }));
                    } else {
                        println!("stopped ({})", target.display_name());
                    }
                    Ok(1)
                }
            }
        }
        "token" => {
            let path = token_file.unwrap_or_else(default_token_path);
            let (token, created) = if regenerate {
                let tok = generate_auth_token().map_err(|e| e.to_string())?;
                write_token_file(&path, &tok).map_err(|e| e.to_string())?;
                (tok, true)
            } else {
                load_or_create_token(&path).map_err(|e| e.to_string())?
            };
            if opts.json {
                print_json(&json!({
                    "token": token,
                    "path": path.to_string_lossy(),
                    "created": created,
                }));
            } else {
                println!("{token}");
            }
            Ok(0)
        }
        "install-service" => {
            let svc_opts = ServiceOptions { listen, token_file };
            let path = server::install_service_with_options(&socket, &svc_opts)
                .map_err(|e| e.to_string())?;
            if opts.json {
                print_json(&json!({ "installed": path.to_string_lossy() }));
            } else {
                println!("installed user service: {}", path.display());
            }
            Ok(0)
        }
        "uninstall-service" => {
            server::uninstall_service().map_err(|e| e.to_string())?;
            if opts.json {
                print_json(&json!({ "uninstalled": true }));
            } else {
                println!("removed tachyon server user service");
            }
            Ok(0)
        }
        other => Err(format!("unknown server subcommand `{other}`")),
    }
}

// ---------------------------------------------------------------------------
// hosts
// ---------------------------------------------------------------------------

fn cmd_hosts(opts: &GlobalOpts, args: &[String]) -> Result<i32, String> {
    let sub = args.first().map(String::as_str).unwrap_or("ls");
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };

    match sub {
        "ls" | "list" => {
            let hosts = load_hosts();
            let local_uri = default_local_uri();
            if opts.json {
                let mut entries = vec![json!({
                    "alias": "local",
                    "uri": local_uri,
                })];
                entries.extend(hosts.iter().map(HostEntry::to_json));
                print_json(&json!({ "hosts": entries }));
            } else {
                println!("{:<18} {}", "local", local_uri);
                for h in &hosts {
                    let mut notes = Vec::new();
                    if let Some(b) = &h.remote_bin {
                        notes.push(format!("bin: {b}"));
                    }
                    if h.token.is_some() {
                        notes.push("token ✓".into());
                    }
                    if let Some(c) = &h.color {
                        notes.push(format!("color: {c}"));
                    }
                    if notes.is_empty() {
                        println!("{:<18} {}", h.alias, h.uri);
                    } else {
                        println!("{:<18} {} ({})", h.alias, h.uri, notes.join(", "));
                    }
                }
            }
            Ok(0)
        }
        "add" => {
            let usage = "usage: tachyon hosts add <alias> <uri> [--bin PATH] [--token TOKEN] [--color COLOR]";
            let alias = rest.first().ok_or(usage)?;
            let uri = rest.get(1).ok_or(usage)?;
            let mut remote_bin = None;
            let mut token = None;
            let mut color = None;
            let mut i = 2;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--bin" => {
                        remote_bin = Some(rest.get(i + 1).ok_or("--bin requires a path")?.clone());
                        i += 2;
                    }
                    "--token" => {
                        token = Some(rest.get(i + 1).ok_or("--token requires a bearer token")?.clone());
                        i += 2;
                    }
                    "--color" => {
                        let c = rest.get(i + 1).ok_or("--color requires a colour preset or #RRGGBB")?;
                        if !HostColor::is_valid_spec(c) {
                            return Err(format!("invalid host colour `{c}` (expected blue|purple|teal|green|amber|orange|red|danger|pink|none|auto or #RRGGBB)"));
                        }
                        color = (!c.eq_ignore_ascii_case("auto")).then(|| c.clone());
                        i += 2;
                    }
                    other => return Err(format!("unknown hosts add flag `{other}`")),
                }
            }
            let mut hosts = load_hosts();
            // Preserve existing colour if updating an existing host without `--color`.
            let preserved_color = hosts
                .iter()
                .find(|h| h.alias == *alias)
                .and_then(|h| h.color.clone());
            // Validate URI syntax up front.
            let entry = HostEntry {
                alias: alias.clone(),
                uri: uri.clone(),
                remote_bin,
                token,
                color: color.or(preserved_color),
            };
            let _ = resolve_target(Some(&entry.alias), std::slice::from_ref(&entry))
                .map_err(|e| e.to_string())?;
            if let Some(existing) = hosts.iter_mut().find(|h| h.alias == *alias) {
                *existing = entry.clone();
            } else {
                hosts.push(entry.clone());
            }
            save_hosts(&hosts).map_err(|e| e.to_string())?;
            if opts.json {
                print_json(&entry.to_json());
            } else {
                println!("saved host `{}` -> {}", entry.alias, entry.uri);
            }
            Ok(0)
        }
        "color" | "colour" => {
            let usage = "usage: tachyon hosts color <alias> <blue|purple|teal|green|amber|orange|red|danger|pink|none|auto|#RRGGBB>";
            let alias = rest.first().ok_or(usage)?;
            let spec = rest.get(1).ok_or(usage)?;
            if !HostColor::is_valid_spec(spec) {
                return Err(format!(
                    "invalid host colour `{spec}` (expected blue|purple|teal|green|amber|orange|red|danger|pink|none|auto or #RRGGBB)"
                ));
            }
            let mut hosts = load_hosts();
            let Some(entry) = hosts.iter_mut().find(|h| h.alias == *alias) else {
                return Err(format!("no configured host `{alias}`"));
            };
            entry.color = if spec.eq_ignore_ascii_case("auto") {
                None
            } else {
                Some(spec.trim().to_string())
            };
            let out_json = entry.to_json();
            save_hosts(&hosts).map_err(|e| e.to_string())?;
            if opts.json {
                print_json(&out_json);
            } else {
                println!("set colour for `{alias}` -> {}", spec.trim());
            }
            Ok(0)
        }
        "remove" | "rm" => {
            let alias = rest.first().ok_or("usage: tachyon hosts remove <alias>")?;
            let mut hosts = load_hosts();
            let before = hosts.len();
            hosts.retain(|h| h.alias != *alias);
            if hosts.len() == before {
                return Err(format!("no configured host `{alias}`"));
            }
            save_hosts(&hosts).map_err(|e| e.to_string())?;
            if opts.json {
                print_json(&json!({ "removed": alias }));
            } else {
                println!("removed host `{alias}`");
            }
            Ok(0)
        }
        "test" => {
            let alias = rest.first().map(String::as_str).or(opts.server.as_deref()).unwrap_or("local");
            let hosts = load_hosts();
            let target = resolve_target(Some(alias), &hosts).map_err(|e| e.to_string())?;
            let c = MuxClient::connect(&target, true, "cli", "hosts-test")
                .map_err(|e| format!("failed to reach `{alias}`: {e}"))?;
            let rtt = c.ping(Duration::from_secs(5)).map_err(|e| e.to_string())?;
            let snap = c.state().map_err(|e| e.to_string())?;
            if opts.json {
                print_json(&json!({
                    "ok": true,
                    "alias": alias,
                    "rtt_ms": rtt.as_secs_f64() * 1000.0,
                    "host": snap.host.to_json(),
                    "sessions": snap.sessions.len(),
                    "blocks": snap.blocks.len(),
                }));
            } else {
                println!(
                    "ok: {} ({}/{}, pid {}, v{}) — rtt {:.1} ms, {} sessions, {} blocks",
                    snap.host.hostname,
                    snap.host.os,
                    snap.host.arch,
                    snap.host.pid,
                    snap.host.version,
                    rtt.as_secs_f64() * 1000.0,
                    snap.sessions.len(),
                    snap.blocks.len(),
                );
            }
            Ok(0)
        }
        other => Err(format!("unknown hosts subcommand `{other}`")),
    }
}

// ---------------------------------------------------------------------------
// ls / session / window
// ---------------------------------------------------------------------------

fn cmd_ls(opts: &GlobalOpts) -> Result<i32, String> {
    let (target, c) = connect(opts, true)?;
    let snap = c.state().map_err(|e| e.to_string())?;
    if opts.json {
        print_json(&snap.to_json());
        return Ok(0);
    }
    println!(
        "{} ({}, {}/{}, pid {})",
        target.display_name(),
        snap.host.hostname,
        snap.host.os,
        snap.host.arch,
        snap.host.pid
    );
    if snap.sessions.is_empty() {
        println!("  (no active sessions)");
        return Ok(0);
    }
    for s in &snap.sessions {
        println!("  {}  [{}]", s.name, s.id);
        for wid in &s.windows {
            if let Some(w) = snap.windows.iter().find(|w| &w.id == wid) {
                let active = if s.active_window.as_deref() == Some(wid.as_str()) { "*" } else { " " };
                let wname = w.name.as_deref().unwrap_or("");
                let leaves = w.layout.leaves();
                if wname.is_empty() {
                    println!("   {active} window {} ({} pane{})", w.id, leaves.len(), if leaves.len() == 1 { "" } else { "s" });
                } else {
                    println!("   {active} window {} `{}` ({} pane{})", w.id, wname, leaves.len(), if leaves.len() == 1 { "" } else { "s" });
                }
                for bid in &leaves {
                    if let Some(b) = snap.blocks.iter().find(|b| &b.id == bid) {
                        let focus = if w.focused_block == b.id { ">" } else { " " };
                        let status = if b.alive {
                            format!("{}x{} pid {}", b.cols, b.rows, b.pid)
                        } else {
                            format!("exited({})", b.exit_code.unwrap_or(0))
                        };
                        println!("      {focus} {}  {}  ({})", b.id, b.display_name(), status);
                    }
                }
            }
        }
    }
    Ok(0)
}

fn cmd_session(opts: &GlobalOpts, args: &[String]) -> Result<i32, String> {
    let sub = args.first().map(String::as_str).unwrap_or("ls");
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };
    match sub {
        "ls" | "list" => cmd_ls(opts),
        "new" | "create" => cmd_session_new(opts, rest),
        "rename" => {
            let id = rest
                .first()
                .map(String::as_str)
                .ok_or("usage: tachyon session rename <session-id|name> <new-name>")?;
            let name = rest
                .get(1)
                .map(String::as_str)
                .ok_or("usage: tachyon session rename <session-id|name> <new-name>")?;
            let (_, c) = connect(opts, false)?;
            let sess = c.rename_session(id, name).map_err(|e| e.to_string())?;
            if opts.json {
                print_json(&sess.to_json());
            } else {
                println!("renamed {} -> {}", sess.id, sess.name);
            }
            Ok(0)
        }
        "kill" | "rm" => {
            let id = rest
                .first()
                .cloned()
                .or_else(|| std::env::var("TACHYON_SESSION").ok())
                .ok_or("usage: tachyon session kill <session-id|name>")?;
            let (_, c) = connect(opts, false)?;
            c.kill_session(&id).map_err(|e| e.to_string())?;
            if opts.json {
                print_json(&json!({ "killed": id }));
            } else {
                println!("killed session {id}");
            }
            Ok(0)
        }
        "inspect" => {
            let id = rest
                .first()
                .cloned()
                .or_else(|| std::env::var("TACHYON_SESSION").ok())
                .ok_or("specify a session id/name or run inside Tachyon")?;
            let (_, c) = connect(opts, false)?;
            let data = c
                .rpc(json!({ "op": "session.inspect", "session_id": id }))
                .map_err(|e| e.to_string())?;
            print_json(&data);
            Ok(0)
        }
        other => Err(format!("unknown session subcommand `{other}`")),
    }
}

fn cmd_session_new(opts: &GlobalOpts, args: &[String]) -> Result<i32, String> {
    let mut name: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut label: Option<String> = None;
    let mut cols: u16 = 120;
    let mut rows: u16 = 32;
    let mut cmd: Option<Vec<String>> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                cmd = Some(args[i + 1..].to_vec());
                break;
            }
            "--cwd" => {
                cwd = Some(args.get(i + 1).ok_or("--cwd requires a path")?.clone());
                i += 2;
            }
            "--label" => {
                label = Some(args.get(i + 1).ok_or("--label requires a value")?.clone());
                i += 2;
            }
            "--cols" => {
                cols = args.get(i + 1).ok_or("--cols requires a number")?.parse().map_err(|_| "bad --cols")?;
                i += 2;
            }
            "--rows" => {
                rows = args.get(i + 1).ok_or("--rows requires a number")?.parse().map_err(|_| "bad --rows")?;
                i += 2;
            }
            flag if flag.starts_with('-') => return Err(format!("unknown flag `{flag}`")),
            pos if name.is_none() => {
                name = Some(pos.to_string());
                i += 1;
            }
            extra => return Err(format!("unexpected argument `{extra}` (use `--` before a command)")),
        }
    }

    let (_, c) = connect(opts, true)?;
    let (sess, win, blk) = c
        .create_session(name.as_deref(), cwd.as_deref(), cmd.as_deref(), label.as_deref(), cols, rows)
        .map_err(|e| e.to_string())?;
    if opts.json {
        print_json(&json!({
            "session": sess.to_json(),
            "window": win.to_json(),
            "block": blk.to_json(),
        }));
    } else {
        println!("{} ({}) window={} block={}", sess.name, sess.id, win.id, blk.id);
    }
    Ok(0)
}

fn cmd_window(opts: &GlobalOpts, args: &[String]) -> Result<i32, String> {
    let sub = args.first().map(String::as_str).unwrap_or("ls");
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };
    match sub {
        "ls" | "list" => {
            let filter_session = rest.first().cloned().or_else(|| std::env::var("TACHYON_SESSION").ok());
            let (_, c) = connect(opts, true)?;
            let snap = c.state().map_err(|e| e.to_string())?;
            let wins: Vec<_> = snap
                .windows
                .iter()
                .filter(|w| filter_session.as_deref().is_none_or(|s| w.session_id == s))
                .collect();
            if opts.json {
                print_json(&json!({ "windows": wins.iter().map(|w| w.to_json()).collect::<Vec<_>>() }));
            } else {
                for w in wins {
                    println!(
                        "{}  session={}  focus={}  panes={}  {}",
                        w.id,
                        w.session_id,
                        w.focused_block,
                        w.layout.leaves().len(),
                        w.name.as_deref().unwrap_or("")
                    );
                }
            }
            Ok(0)
        }
        "new" | "create" => {
            let mut session: Option<String> = std::env::var("TACHYON_SESSION").ok();
            let mut name: Option<String> = None;
            let mut cwd: Option<String> = None;
            let mut label: Option<String> = None;
            let mut cols: u16 = 120;
            let mut rows: u16 = 32;
            let mut cmd: Option<Vec<String>> = None;

            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--" => {
                        cmd = Some(rest[i + 1..].to_vec());
                        break;
                    }
                    "-s" | "--session" => {
                        session = Some(rest.get(i + 1).ok_or("--session requires an id/name")?.clone());
                        i += 2;
                    }
                    "--name" => {
                        name = Some(rest.get(i + 1).ok_or("--name requires a value")?.clone());
                        i += 2;
                    }
                    "--cwd" => {
                        cwd = Some(rest.get(i + 1).ok_or("--cwd requires a path")?.clone());
                        i += 2;
                    }
                    "--label" => {
                        label = Some(rest.get(i + 1).ok_or("--label requires a value")?.clone());
                        i += 2;
                    }
                    "--cols" => {
                        cols = rest.get(i + 1).ok_or("--cols requires a number")?.parse().map_err(|_| "bad --cols")?;
                        i += 2;
                    }
                    "--rows" => {
                        rows = rest.get(i + 1).ok_or("--rows requires a number")?.parse().map_err(|_| "bad --rows")?;
                        i += 2;
                    }
                    other => return Err(format!("unknown window new option `{other}`")),
                }
            }
            let (_, c) = connect(opts, true)?;
            let sid = match session {
                Some(s) => s,
                None => {
                    let snap = c.state().map_err(|e| e.to_string())?;
                    snap.sessions
                        .last()
                        .map(|s| s.id.clone())
                        .ok_or("no active sessions; create one with `tachyon new`")?
                }
            };
            let (win, blk) = c
                .create_window(&sid, name.as_deref(), cwd.as_deref(), cmd.as_deref(), label.as_deref(), cols, rows)
                .map_err(|e| e.to_string())?;
            if opts.json {
                print_json(&json!({ "window": win.to_json(), "block": blk.to_json() }));
            } else {
                println!("window={} block={}", win.id, blk.id);
            }
            Ok(0)
        }
        "rename" => {
            let wid = rest.first().ok_or("usage: tachyon window rename <window-id> <name>")?;
            let name = rest.get(1).ok_or("usage: tachyon window rename <window-id> <name>")?;
            let (_, c) = connect(opts, false)?;
            let win = c.rename_window(wid, name).map_err(|e| e.to_string())?;
            if opts.json {
                print_json(&win.to_json());
            } else {
                println!("renamed {} -> {}", win.id, win.name.as_deref().unwrap_or(""));
            }
            Ok(0)
        }
        "kill" | "rm" => {
            let wid = rest
                .first()
                .cloned()
                .or_else(|| std::env::var("TACHYON_WINDOW").ok())
                .ok_or("usage: tachyon window kill <window-id>")?;
            let (_, c) = connect(opts, false)?;
            c.kill_window(&wid).map_err(|e| e.to_string())?;
            if opts.json {
                print_json(&json!({ "killed": wid }));
            } else {
                println!("killed window {wid}");
            }
            Ok(0)
        }
        "focus" => {
            let wid = rest
                .first()
                .cloned()
                .or_else(|| std::env::var("TACHYON_WINDOW").ok())
                .ok_or("usage: tachyon window focus <window-id> [--block <id>]")?;
            let mut block = None;
            if rest.get(1).map(String::as_str) == Some("--block") {
                block = rest.get(2).cloned();
            }
            let (_, c) = connect(opts, false)?;
            c.focus_window(&wid, block.as_deref()).map_err(|e| e.to_string())?;
            if opts.json {
                print_json(&json!({ "focused": wid, "block": block }));
            }
            Ok(0)
        }
        other => Err(format!("unknown window subcommand `{other}`")),
    }
}

// ---------------------------------------------------------------------------
// split / run / send / capture / events
// ---------------------------------------------------------------------------

fn resolve_block_id(explicit: Option<String>, c: &MuxClient) -> Result<String, String> {
    if let Some(b) = explicit {
        return Ok(b);
    }
    if let Ok(b) = std::env::var("TACHYON_BLOCK") {
        if !b.is_empty() {
            return Ok(b);
        }
    }
    let snap = c.state().map_err(|e| e.to_string())?;
    snap.windows
        .last()
        .map(|w| w.focused_block.clone())
        .or_else(|| snap.blocks.last().map(|b| b.id.clone()))
        .ok_or_else(|| "no active block (pass `-b <block-id>` or create a session first)".to_string())
}

fn cmd_split(opts: &GlobalOpts, args: &[String]) -> Result<i32, String> {
    let mut dir = SplitDir::Right;
    let mut block: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut label: Option<String> = None;
    let mut cols: u16 = 80;
    let mut rows: u16 = 24;
    let mut cmd: Option<Vec<String>> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                cmd = Some(args[i + 1..].to_vec());
                break;
            }
            "--right" | "-r" => {
                dir = SplitDir::Right;
                i += 1;
            }
            "--down" | "-d" => {
                dir = SplitDir::Down;
                i += 1;
            }
            "--dir" => {
                let d = args.get(i + 1).ok_or("--dir requires right|down")?;
                dir = SplitDir::parse(d).ok_or_else(|| format!("invalid split direction `{d}`"))?;
                i += 2;
            }
            "-b" | "--block" => {
                block = Some(args.get(i + 1).ok_or("-b/--block requires an id")?.clone());
                i += 2;
            }
            "--cwd" => {
                cwd = Some(args.get(i + 1).ok_or("--cwd requires a path")?.clone());
                i += 2;
            }
            "--label" => {
                label = Some(args.get(i + 1).ok_or("--label requires a value")?.clone());
                i += 2;
            }
            "--cols" => {
                cols = args.get(i + 1).ok_or("--cols requires a number")?.parse().map_err(|_| "bad --cols")?;
                i += 2;
            }
            "--rows" => {
                rows = args.get(i + 1).ok_or("--rows requires a number")?.parse().map_err(|_| "bad --rows")?;
                i += 2;
            }
            other => return Err(format!("unknown split option `{other}`")),
        }
    }

    let (_, c) = connect(opts, true)?;
    let target_block = resolve_block_id(block, &c)?;
    let (win, blk) = c
        .split_block(&target_block, dir, cwd.as_deref(), cmd.as_deref(), label.as_deref(), cols, rows)
        .map_err(|e| e.to_string())?;
    if opts.json {
        print_json(&json!({ "window": win.to_json(), "block": blk.to_json() }));
    } else {
        println!("block={} window={}", blk.id, win.id);
    }
    Ok(0)
}

fn cmd_run(opts: &GlobalOpts, args: &[String]) -> Result<i32, String> {
    let mut session: Option<String> = None;
    let mut split: Option<SplitDir> = None;
    let mut block: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut label: Option<String> = None;
    let mut wait = false;
    let mut capture = false;
    let mut timeout_ms: Option<u64> = None;
    let mut cols: u16 = 120;
    let mut rows: u16 = 32;
    let mut cmd: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                cmd = args[i + 1..].to_vec();
                break;
            }
            "-s" | "--session" => {
                session = Some(args.get(i + 1).ok_or("--session requires a value")?.clone());
                i += 2;
            }
            "--split" => {
                let d = args.get(i + 1).ok_or("--split requires right|down")?;
                split = Some(SplitDir::parse(d).ok_or_else(|| format!("invalid split `{d}`"))?);
                i += 2;
            }
            "-b" | "--block" => {
                block = Some(args.get(i + 1).ok_or("-b/--block requires an id")?.clone());
                i += 2;
            }
            "--wait" | "-w" => {
                wait = true;
                i += 1;
            }
            "--capture" => {
                capture = true;
                i += 1;
            }
            "--timeout" => {
                timeout_ms = Some(
                    args.get(i + 1)
                        .ok_or("--timeout requires milliseconds")?
                        .parse()
                        .map_err(|_| "bad --timeout")?,
                );
                i += 2;
            }
            "--cwd" => {
                cwd = Some(args.get(i + 1).ok_or("--cwd requires a path")?.clone());
                i += 2;
            }
            "--label" => {
                label = Some(args.get(i + 1).ok_or("--label requires a value")?.clone());
                i += 2;
            }
            "--cols" => {
                cols = args.get(i + 1).ok_or("--cols requires a number")?.parse().map_err(|_| "bad --cols")?;
                i += 2;
            }
            "--rows" => {
                rows = args.get(i + 1).ok_or("--rows requires a number")?.parse().map_err(|_| "bad --rows")?;
                i += 2;
            }
            other if other.starts_with('-') => return Err(format!("unknown run option `{other}`")),
            _ => {
                cmd = args[i..].to_vec();
                break;
            }
        }
    }

    if cmd.is_empty() {
        return Err("usage: tachyon run [--wait] [--split right|down] -- <command...>".into());
    }

    let (_, c) = connect(opts, true)?;

    let blk = if let Some(dir) = split {
        let target_block = resolve_block_id(block, &c)?;
        let (_, blk) = c
            .split_block(&target_block, dir, cwd.as_deref(), Some(&cmd), label.as_deref(), cols, rows)
            .map_err(|e| e.to_string())?;
        blk
    } else {
        let env_sess = std::env::var("TACHYON_SESSION").ok().filter(|s| !s.is_empty());
        let chosen = session.or(env_sess);
        match chosen.as_deref() {
            Some("new") | None => {
                let (_, _, blk) = c
                    .create_session(None, cwd.as_deref(), Some(&cmd), label.as_deref(), cols, rows)
                    .map_err(|e| e.to_string())?;
                blk
            }
            Some(sid) => {
                let (_, blk) = c
                    .create_window(sid, label.as_deref(), cwd.as_deref(), Some(&cmd), label.as_deref(), cols, rows)
                    .map_err(|e| e.to_string())?;
                blk
            }
        }
    };

    if wait {
        let code = c.wait_block(&blk.id, timeout_ms).map_err(|e| e.to_string())?;
        let text = if capture || opts.json {
            c.capture(&blk.id, Some(200), false).ok()
        } else {
            None
        };
        if opts.json {
            print_json(&json!({
                "block": blk.to_json(),
                "exit_code": code,
                "output": text,
            }));
        } else if let Some(out) = text {
            if !out.is_empty() {
                println!("{out}");
            }
        }
        return Ok(code);
    }

    if opts.json {
        print_json(&json!({ "block": blk.to_json() }));
    } else {
        println!("block={} session={} window={}", blk.id, blk.session_id, blk.window_id);
    }
    Ok(0)
}

fn cmd_send(opts: &GlobalOpts, args: &[String]) -> Result<i32, String> {
    let mut block: Option<String> = None;
    let mut enter = false;
    let mut parts: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                parts.extend_from_slice(&args[i + 1..]);
                break;
            }
            "-b" | "--block" => {
                block = Some(args.get(i + 1).ok_or("-b/--block requires an id")?.clone());
                i += 2;
            }
            "--enter" | "-e" => {
                enter = true;
                i += 1;
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown send option `{other}`"));
            }
            _ => {
                parts.push(args[i].clone());
                i += 1;
            }
        }
    }

    let mut payload = if parts.len() == 1 && parts[0] == "-" {
        let mut buf = Vec::new();
        io::stdin().read_to_end(&mut buf).map_err(|e| e.to_string())?;
        buf
    } else if !parts.is_empty() {
        parts.join(" ").into_bytes()
    } else if enter {
        Vec::new()
    } else {
        return Err("usage: tachyon send [-b BLOCK] [--enter] <text | ->".into());
    };

    if enter {
        payload.push(b'\r');
    }

    let (_, c) = connect(opts, false)?;
    let bid = resolve_block_id(block, &c)?;
    c.send_input(&bid, &payload).map_err(|e| e.to_string())?;
    if opts.json {
        print_json(&json!({ "block_id": bid, "bytes": payload.len() }));
    }
    Ok(0)
}

fn cmd_capture(opts: &GlobalOpts, args: &[String]) -> Result<i32, String> {
    let mut block: Option<String> = None;
    let mut lines: Option<usize> = None;
    let mut ansi = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-b" | "--block" => {
                block = Some(args.get(i + 1).ok_or("-b/--block requires an id")?.clone());
                i += 2;
            }
            "-n" | "--lines" => {
                lines = Some(
                    args.get(i + 1)
                        .ok_or("--lines requires a count")?
                        .parse()
                        .map_err(|_| "bad --lines")?,
                );
                i += 2;
            }
            "--ansi" => {
                ansi = true;
                i += 1;
            }
            other => return Err(format!("unknown capture option `{other}`")),
        }
    }

    let (_, c) = connect(opts, false)?;
    let bid = resolve_block_id(block, &c)?;
    let text = c.capture(&bid, lines, ansi).map_err(|e| e.to_string())?;
    if opts.json {
        print_json(&json!({ "block_id": bid, "text": text }));
    } else {
        println!("{text}");
    }
    Ok(0)
}

fn cmd_events(opts: &GlobalOpts, args: &[String]) -> Result<i32, String> {
    let mut session_filter: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-s" | "--session" => {
                session_filter = Some(args.get(i + 1).ok_or("--session requires an id")?.clone());
                i += 2;
            }
            other => return Err(format!("unknown events option `{other}`")),
        }
    }

    let (_, c) = connect(opts, true)?;
    let rx = c.subscribe_events().map_err(|e| e.to_string())?;
    while let Ok(ev) = rx.recv() {
        if let Some(ref want) = session_filter {
            let sid = ev
                .get("session_id")
                .and_then(Value::as_str)
                .or_else(|| ev.get("session").and_then(|s| s.get("id")).and_then(Value::as_str))
                .or_else(|| ev.get("block").and_then(|b| b.get("session_id")).and_then(Value::as_str));
            if sid != Some(want.as_str()) {
                continue;
            }
        }
        if opts.json {
            println!("{}", serde_json::to_string(&ev).unwrap_or_default());
        } else {
            let kind = ev.get("event").and_then(Value::as_str).unwrap_or("event");
            println!("{kind}\t{}", serde_json::to_string(&ev).unwrap_or_default());
        }
    }
    Ok(0)
}

fn cmd_skill(opts: &GlobalOpts, args: &[String]) -> Result<i32, String> {
    use crate::integrations::{self, AgentKind};
    let sub = args.first().map(String::as_str).unwrap_or("status");
    let home = integrations::user_home();
    let cfg = crate::config::Config::load_default();

    match sub {
        "status" | "ls" => {
            let statuses = integrations::inspect_all(&home);
            if opts.json {
                print_json(&json!({
                    "auto_name_sessions": cfg.agents.auto_name_sessions,
                    "agents": statuses.iter().map(|s| s.to_json()).collect::<Vec<_>>(),
                }));
            } else {
                println!(
                    "Auto-name sessions: {}",
                    if cfg.agents.auto_name_sessions { "on" } else { "off" }
                );
                for s in statuses {
                    let det = if s.detected { "detected" } else { "not found" };
                    let inst = if s.skill_installed { "skill ✓" } else { "skill —" };
                    let hooks = if s.settings_path.is_some() {
                        if s.hooks_installed { ", hooks ✓" } else { ", hooks —" }
                    } else {
                        ""
                    };
                    println!("{:<12} {:<10} {inst}{hooks}", s.name, det);
                }
            }
            Ok(0)
        }
        "setup" | "install" => {
            let mut agent: Option<AgentKind> = None;
            let mut all = false;
            let mut auto_name = cfg.agents.auto_name_sessions;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--agent" | "-a" => {
                        let name = args.get(i + 1).ok_or("--agent requires claude|antigravity|codex|opencode|pi|all")?;
                        if name.eq_ignore_ascii_case("all") {
                            all = true;
                        } else {
                            agent = Some(
                                AgentKind::parse(name)
                                    .ok_or_else(|| format!("unknown agent `{name}`"))?,
                            );
                        }
                        i += 2;
                    }
                    "--auto-name" => {
                        auto_name = true;
                        i += 1;
                    }
                    "--no-auto-name" => {
                        auto_name = false;
                        i += 1;
                    }
                    other => {
                        if other.eq_ignore_ascii_case("all") {
                            all = true;
                            i += 1;
                        } else if let Some(k) = AgentKind::parse(other) {
                            agent = Some(k);
                            i += 1;
                        } else {
                            return Err(format!("unknown skill setup option `{other}`"));
                        }
                    }
                }
            }
            let _ = crate::config::Config::update_default_key(
                "agents.auto_name_sessions",
                if auto_name { "true" } else { "false" },
            );
            let statuses = if all {
                for &k in &AgentKind::ALL {
                    integrations::setup_agent(&home, k, auto_name).map_err(|e| e.to_string())?;
                }
                integrations::inspect_all(&home)
            } else if let Some(k) = agent {
                integrations::setup_agent(&home, k, auto_name).map_err(|e| e.to_string())?;
                integrations::inspect_all(&home)
            } else {
                integrations::setup_all_detected(&home, auto_name).map_err(|e| e.to_string())?
            };
            if opts.json {
                print_json(&json!({
                    "auto_name_sessions": auto_name,
                    "agents": statuses.iter().map(|s| s.to_json()).collect::<Vec<_>>(),
                }));
            } else {
                for s in statuses.iter().filter(|s| s.skill_installed) {
                    println!("installed {} skill -> {}", s.name, s.skill_path.display());
                    if s.hooks_installed {
                        if let Some(p) = &s.settings_path {
                            println!("installed {} hooks -> {}", s.name, p.display());
                        }
                    }
                }
            }
            Ok(0)
        }
        "remove" | "uninstall" => {
            let mut agent: Option<AgentKind> = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--agent" | "-a" => {
                        let name = args.get(i + 1).ok_or("--agent requires claude|antigravity|codex|opencode|pi")?;
                        agent = Some(
                            AgentKind::parse(name)
                                .ok_or_else(|| format!("unknown agent `{name}`"))?,
                        );
                        i += 2;
                    }
                    other => {
                        if let Some(k) = AgentKind::parse(other) {
                            agent = Some(k);
                            i += 1;
                        } else {
                            return Err(format!("unknown skill remove option `{other}`"));
                        }
                    }
                }
            }
            let statuses = if let Some(k) = agent {
                integrations::remove_agent(&home, k).map_err(|e| e.to_string())?;
                integrations::inspect_all(&home)
            } else {
                integrations::remove_all(&home).map_err(|e| e.to_string())?
            };
            if opts.json {
                print_json(&json!({
                    "agents": statuses.iter().map(|s| s.to_json()).collect::<Vec<_>>(),
                }));
            } else {
                println!("removed Tachyon agent skills and hooks");
            }
            Ok(0)
        }
        other => Err(format!("unknown skill subcommand `{other}` (use status, setup, or remove)")),
    }
}

fn cmd_status(args: &[String]) -> Result<i32, String> {
    use crate::term::{ProgramState, ProgramStatus};
    let mut state: Option<ProgramState> = None;
    let mut clear = false;
    let mut app: Option<String> = None;
    let mut kind: Option<String> = None;
    let mut msg: Option<String> = None;
    let mut label: Option<String> = None;
    let mut use_tty = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "clear" => {
                clear = true;
                i += 1;
            }
            "--state" | "-s" => {
                let v = args.get(i + 1).ok_or("--state requires idle|working|blocked|done|error|clear")?;
                if v.eq_ignore_ascii_case("clear") {
                    clear = true;
                } else {
                    state = Some(
                        ProgramState::parse(v)
                            .ok_or_else(|| format!("unknown status state `{v}`"))?,
                    );
                }
                i += 2;
            }
            "--app" => {
                app = Some(args.get(i + 1).ok_or("--app requires a name")?.clone());
                i += 2;
            }
            "--kind" => {
                kind = Some(args.get(i + 1).ok_or("--kind requires a value")?.clone());
                i += 2;
            }
            "--msg" | "-m" => {
                msg = Some(args.get(i + 1).ok_or("--msg requires text")?.clone());
                i += 2;
            }
            "--label" => {
                label = Some(args.get(i + 1).ok_or("--label requires text")?.clone());
                i += 2;
            }
            "--tty" => {
                use_tty = true;
                i += 1;
            }
            other => {
                if let Some(st) = ProgramState::parse(other) {
                    state = Some(st);
                    i += 1;
                } else {
                    return Err(format!("unknown status option `{other}`"));
                }
            }
        }
    }

    if clear {
        crate::integrations::emit_osc_7501(None, use_tty).map_err(|e| e.to_string())?;
        return Ok(0);
    }
    let state = state.ok_or("specify --state <idle|working|blocked|done|error|clear>")?;
    let st = ProgramStatus { state, app, kind, msg, label };
    crate::integrations::emit_osc_7501(Some(&st), use_tty).map_err(|e| e.to_string())?;
    Ok(0)
}

fn cmd_hook(args: &[String]) -> Result<i32, String> {
    let agent = args.first().map(String::as_str).unwrap_or("agent");
    let event = args.get(1).map(String::as_str).unwrap_or("working");
    Ok(crate::integrations::run_hook(agent, event))
}

// ---------------------------------------------------------------------------
// workspace (multi-host session templates)
// ---------------------------------------------------------------------------

fn connect_host_alias(alias: &str, auto_start: bool) -> Result<std::sync::Arc<MuxClient>, String> {
    let hosts = load_hosts();
    let selector = if alias == "local" { None } else { Some(alias) };
    let target = resolve_target(selector, &hosts).map_err(|e| e.to_string())?;
    MuxClient::connect(&target, auto_start, "cli", "tachyon-cli")
        .map_err(|e| format!("could not connect to `{alias}` ({}): {e}", target.display_name()))
}

fn cmd_workspace(opts: &GlobalOpts, args: &[String]) -> Result<i32, String> {
    use crate::mux::workspace::{
        display_path, list_workspaces, load_workspace, remove_workspace, save_workspace,
        snapshot_workspace, starter_workspace, workspace_down, workspace_up, workspaces_dir,
        WorkspaceUpOptions,
    };

    let sub = args.first().map(String::as_str).unwrap_or("ls");
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };

    match sub {
        "ls" | "list" => {
            let list = list_workspaces();
            if opts.json {
                print_json(&json!({
                    "dir": workspaces_dir().to_string_lossy(),
                    "workspaces": list.iter().map(|w| w.to_json()).collect::<Vec<_>>(),
                }));
            } else if list.is_empty() {
                println!(
                    "No workspaces saved in {}. Run `tachyon workspace init <name>` or `tachyon workspace save <name>`.",
                    display_path(&workspaces_dir())
                );
            } else {
                for w in &list {
                    let hosts = w.hosts.join(",");
                    let desc = w
                        .description
                        .as_deref()
                        .map(|d| format!("  — {d}"))
                        .unwrap_or_default();
                    println!(
                        "{:<18} [{}]  {}s/{}w/{}p{}",
                        w.name, hosts, w.session_count, w.window_count, w.pane_count, desc
                    );
                }
            }
            Ok(0)
        }
        "show" | "cat" => {
            let target = rest
                .first()
                .map(String::as_str)
                .ok_or("usage: tachyon workspace show <name|path>")?;
            let (path, spec) = load_workspace(target)?;
            if opts.json {
                print_json(&json!({
                    "path": path.to_string_lossy(),
                    "workspace": spec.to_json(),
                }));
            } else {
                print_json(&spec.to_json());
            }
            Ok(0)
        }
        "init" => {
            let mut name: Option<String> = None;
            let mut force = false;
            for a in rest {
                match a.as_str() {
                    "--force" | "-f" => force = true,
                    other if other.starts_with('-') => {
                        return Err(format!("unknown workspace init option `{other}`"));
                    }
                    other => {
                        if name.is_none() {
                            name = Some(other.to_string());
                        } else {
                            return Err(format!("unexpected argument `{other}`"));
                        }
                    }
                }
            }
            let name = name.ok_or("usage: tachyon workspace init <name> [--force]")?;
            let hosts = load_hosts();
            let spec = starter_workspace(&name, &hosts)?;
            let path = save_workspace(&spec, force)?;
            if opts.json {
                print_json(&json!({
                    "name": spec.name,
                    "path": path.to_string_lossy(),
                    "hosts": spec.hosts(),
                }));
            } else {
                println!(
                    "Created workspace `{}` at {} ({} host(s): {})",
                    spec.name,
                    display_path(&path),
                    spec.hosts().len(),
                    spec.hosts().join(", ")
                );
            }
            Ok(0)
        }
        "rm" | "remove" | "delete" => {
            let name = rest
                .first()
                .map(String::as_str)
                .ok_or("usage: tachyon workspace rm <name>")?;
            let path = remove_workspace(name)?;
            if opts.json {
                print_json(&json!({ "removed": name, "path": path.to_string_lossy() }));
            } else {
                println!("Removed workspace `{name}` ({})", display_path(&path));
            }
            Ok(0)
        }
        "up" | "start" | "launch" => {
            let mut target: Option<String> = None;
            let mut up_opts = WorkspaceUpOptions::default();
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--recreate" | "--force" => {
                        up_opts.recreate = true;
                        i += 1;
                    }
                    "--dry-run" => {
                        up_opts.dry_run = true;
                        i += 1;
                    }
                    "--cols" => {
                        let v = rest.get(i + 1).ok_or("--cols requires a number")?;
                        up_opts.cols = v.parse::<u16>().map_err(|_| format!("invalid --cols `{v}`"))?;
                        i += 2;
                    }
                    "--rows" => {
                        let v = rest.get(i + 1).ok_or("--rows requires a number")?;
                        up_opts.rows = v.parse::<u16>().map_err(|_| format!("invalid --rows `{v}`"))?;
                        i += 2;
                    }
                    other if other.starts_with('-') => {
                        return Err(format!("unknown workspace up option `{other}`"));
                    }
                    other => {
                        if target.is_none() {
                            target = Some(other.to_string());
                        } else {
                            return Err(format!("unexpected argument `{other}`"));
                        }
                        i += 1;
                    }
                }
            }
            let target = target.ok_or(
                "usage: tachyon workspace up <name|path> [--recreate] [--dry-run]",
            )?;
            let (_path, spec) = load_workspace(&target)?;
            let report = workspace_up(&spec, &up_opts, |alias| connect_host_alias(alias, true));
            if opts.json {
                print_json(&report.to_json());
            } else {
                let verb = if report.dry_run { "Plan" } else { "Workspace" };
                println!("{verb} `{}`:", report.name);
                for s in &report.sessions {
                    let state = if report.dry_run {
                        "would create"
                    } else if s.reused {
                        "reused"
                    } else {
                        "created"
                    };
                    let panes: usize = s.windows.iter().map(|w| w.blocks.len()).sum();
                    println!(
                        "  {:14} session `{}` ({}) — {} ({} window(s), {} pane(s))",
                        s.host,
                        s.session_name,
                        s.session_id,
                        state,
                        s.windows.len(),
                        panes
                    );
                }
                for e in &report.errors {
                    eprintln!("  {:14} ERROR: {}", e.host, e.error);
                }
            }
            Ok(if report.errors.is_empty() { 0 } else { 1 })
        }
        "down" | "stop" => {
            let target = rest
                .first()
                .map(String::as_str)
                .ok_or("usage: tachyon workspace down <name|path>")?;
            let (_path, spec) = load_workspace(target)?;
            let report = workspace_down(&spec, |alias| connect_host_alias(alias, false));
            if opts.json {
                print_json(&report.to_json());
            } else {
                println!("Workspace `{}` down:", report.name);
                for s in &report.stopped {
                    println!(
                        "  {:14} stopped session `{}` ({})",
                        s.host,
                        s.session_name,
                        s.session_id.as_deref().unwrap_or("")
                    );
                }
                for s in &report.not_found {
                    println!("  {:14} session `{}` not running", s.host, s.session_name);
                }
                for e in &report.errors {
                    eprintln!("  {:14} ERROR: {}", e.host, e.error);
                }
            }
            Ok(if report.errors.is_empty() { 0 } else { 1 })
        }
        "save" | "snapshot" => {
            let mut name: Option<String> = None;
            let mut hosts_arg: Option<Vec<String>> = None;
            let mut all_hosts = false;
            let mut session_filter: Option<String> = None;
            let mut description: Option<String> = None;
            let mut force = false;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--hosts" => {
                        let v = rest.get(i + 1).ok_or("--hosts requires comma-separated aliases")?;
                        hosts_arg = Some(
                            v.split(',')
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                                .collect(),
                        );
                        i += 2;
                    }
                    "--all-hosts" => {
                        all_hosts = true;
                        i += 1;
                    }
                    "-s" | "--session" => {
                        session_filter = Some(
                            rest.get(i + 1).ok_or("--session requires a name or id")?.clone(),
                        );
                        i += 2;
                    }
                    "-d" | "--description" => {
                        description =
                            Some(rest.get(i + 1).ok_or("--description requires text")?.clone());
                        i += 2;
                    }
                    "--force" | "-f" => {
                        force = true;
                        i += 1;
                    }
                    other if other.starts_with('-') => {
                        return Err(format!("unknown workspace save option `{other}`"));
                    }
                    other => {
                        if name.is_none() {
                            name = Some(other.to_string());
                        } else {
                            return Err(format!("unexpected argument `{other}`"));
                        }
                        i += 1;
                    }
                }
            }
            let name = name.ok_or(
                "usage: tachyon workspace save <name> [--hosts local,kagutsuchi | --all-hosts] [--session S] [--description TEXT] [--force]",
            )?;

            let target_hosts: Vec<String> = if let Some(list) = hosts_arg {
                list
            } else if all_hosts {
                let mut list = vec!["local".to_string()];
                for h in load_hosts() {
                    if h.alias != "local" {
                        list.push(h.alias);
                    }
                }
                list
            } else if let Some(s) = &opts.server {
                vec![s.clone()]
            } else {
                vec!["local".to_string()]
            };

            let mut snapshots = Vec::new();
            let mut errs = Vec::new();
            for alias in &target_hosts {
                match connect_host_alias(alias, false).and_then(|c| {
                    c.state().map_err(|e| format!("state failed on `{alias}`: {e}"))
                }) {
                    Ok(snap) => snapshots.push((alias.clone(), snap)),
                    Err(e) => errs.push((alias.clone(), e)),
                }
            }
            if snapshots.is_empty() {
                let reason = errs
                    .into_iter()
                    .map(|(h, e)| format!("{h}: {e}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(format!("could not read any host state ({reason})"));
            }

            let spec = snapshot_workspace(
                &name,
                description,
                &snapshots,
                session_filter.as_deref(),
                None,
            )?;
            let path = save_workspace(&spec, force)?;
            if opts.json {
                print_json(&json!({
                    "name": spec.name,
                    "path": path.to_string_lossy(),
                    "hosts": spec.hosts(),
                    "sessions": spec.sessions.len(),
                    "windows": spec.window_count(),
                    "panes": spec.pane_count(),
                    "skipped_hosts": errs.into_iter().map(|(h, e)| json!({ "host": h, "error": e })).collect::<Vec<_>>(),
                }));
            } else {
                println!(
                    "Saved workspace `{}` to {} ({} session(s), {} window(s), {} pane(s))",
                    spec.name,
                    display_path(&path),
                    spec.sessions.len(),
                    spec.window_count(),
                    spec.pane_count()
                );
                for (h, e) in errs {
                    eprintln!("  skipped `{h}`: {e}");
                }
            }
            Ok(0)
        }
        other => Err(format!(
            "unknown workspace subcommand `{other}` (expected ls|up|down|save|init|show|rm)"
        )),
    }
}

// ---------------------------------------------------------------------------
// agents (cross-host Agent Mission Control CLI)
// ---------------------------------------------------------------------------

fn cmd_agents(opts: &GlobalOpts, args: &[String]) -> Result<i32, String> {
    use crate::mux::model::{collect_agent_entries, AgentCounts, AgentFilter};

    let mut hosts_arg: Option<Vec<String>> = None;
    let mut all_hosts = false;
    let mut filter = AgentFilter::All;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--all-hosts" | "-a" => {
                all_hosts = true;
                i += 1;
            }
            "--hosts" => {
                let v = args.get(i + 1).ok_or("--hosts requires comma-separated aliases")?;
                hosts_arg = Some(
                    v.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect(),
                );
                i += 2;
            }
            "--state" | "--filter" => {
                let v = args.get(i + 1).ok_or("--state requires all|attention|working|done")?;
                filter = AgentFilter::parse(v)
                    .ok_or_else(|| format!("invalid agent filter `{v}` (expected all|attention|working|done)"))?;
                i += 2;
            }
            other => {
                if let Some(f) = AgentFilter::parse(other) {
                    filter = f;
                    i += 1;
                } else {
                    return Err(format!("unknown agents option `{other}`"));
                }
            }
        }
    }

    let target_hosts: Vec<String> = if let Some(list) = hosts_arg {
        list
    } else if all_hosts {
        let mut list = vec!["local".to_string()];
        for h in load_hosts() {
            if h.alias != "local" {
                list.push(h.alias);
            }
        }
        list
    } else if let Some(s) = &opts.server {
        vec![s.clone()]
    } else {
        vec!["local".to_string()]
    };

    let mut snaps = Vec::new();
    let mut errors = Vec::new();
    for alias in &target_hosts {
        match connect_host_alias(alias, false)
            .and_then(|c| c.state().map_err(|e| format!("state failed: {e}")))
        {
            Ok(snap) => {
                let display = if alias == "local" {
                    "This Mac".to_string()
                } else {
                    alias.clone()
                };
                snaps.push((alias.clone(), display, snap));
            }
            Err(e) => errors.push((alias.clone(), e)),
        }
    }

    let refs: Vec<_> = snaps
        .iter()
        .map(|(a, d, s)| (a.clone(), d.clone(), None, s))
        .collect();
    let all_entries = collect_agent_entries(&refs);
    let counts = AgentCounts::from_entries(&all_entries);
    let entries: Vec<_> = all_entries
        .into_iter()
        .filter(|e| filter.matches(e.state))
        .collect();

    if opts.json {
        print_json(&json!({
            "counts": {
                "total": counts.total,
                "blocked": counts.blocked,
                "error": counts.error,
                "working": counts.working,
                "done": counts.done,
                "idle": counts.idle,
            },
            "agents": entries.iter().map(|e| e.to_json()).collect::<Vec<_>>(),
            "errors": errors.iter().map(|(h, e)| json!({ "host": h, "error": e })).collect::<Vec<_>>(),
        }));
    } else if entries.is_empty() {
        println!("No matching coding agents active.");
        for (h, e) in &errors {
            eprintln!("  skipped `{h}`: {e}");
        }
    } else {
        for e in &entries {
            let msg = e
                .message
                .as_deref()
                .map(|m| format!("  — {m}"))
                .unwrap_or_default();
            println!(
                "{:<9} {} {:<12} {:<12} {}/{} ({}){}",
                e.state.as_str().to_uppercase(),
                e.glyph,
                e.agent_name,
                e.host,
                e.session_name,
                e.window_name,
                e.block_id,
                msg
            );
        }
        for (h, e) in &errors {
            eprintln!("  skipped `{h}`: {e}");
        }
    }
    Ok(0)
}

const HELP: &str = "\
Tachyon — GPU-accelerated multi-host multiplexer terminal

Usage:
  tachyon                                   launch desktop app (GUI builds)
  tachyon [-S HOST] [--json] <command> ...  control local or remote host server

Server & Hosts:
  tachyon server run|start|stop|status      manage the persistent local server
    [--socket PATH] [--listen ADDR|tailscale] [--token TOK | --token-file PATH]
  tachyon server token [--regenerate]       show or rotate the TCP bearer auth token
  tachyon server install-service            install launchd (macOS), systemd (Linux), or Task (Windows)
  tachyon server uninstall-service          remove the user service
  tachyon bridge                            stdio <-> local socket/pipe bridge (invoked over SSH)
  tachyon hosts ls                          list local & configured remote hosts
  tachyon hosts add <alias> <uri> [--bin P] [--token T] [--color C]
                                            save an ssh:// or direct tcp:// remote host
  tachyon hosts color <alias> <color|auto|none>
                                            set accent or danger colour for a saved host
  tachyon hosts remove <alias>              remove a saved host
  tachyon hosts test <alias>                verify SSH/TCP transport, server version & RTT

Workspaces (Multi-host Templates):
  tachyon workspace ls                      list saved templates in ~/.config/Tachyon/workspaces/
  tachyon workspace up <name|path> [--recreate] [--dry-run]
                                            launch (or idempotently reuse) sessions & splits across hosts
  tachyon workspace down <name|path>        stop sessions belonging to a workspace
  tachyon workspace save <name> [--hosts H1,H2 | --all-hosts] [--session S] [--force]
                                            snapshot live sessions & split trees into a template
  tachyon workspace init <name> [--force]   create a starter multi-host template from saved hosts
  tachyon workspace show|rm <name>          inspect or remove a saved template

Sessions, Windows & Automation:
  tachyon ls                                list sessions, windows & blocks
  tachyon new [NAME] [--cwd D] [-- CMD...]  create a new persistent session
  tachyon session ls|new|rename|kill|inspect
  tachyon window  ls|new|rename|kill|focus
  tachyon split [--right|--down] [-b B] [-- CMD...]
  tachyon run [--session S] [--split right|down] [--wait] [--capture] -- CMD...
  tachyon send [-b B] [--enter] <TEXT | ->
  tachyon capture [-b B] [--lines N] [--ansi]
  tachyon events [-s SESSION] [--json]

Coding Agents & Live Status (OSC 7501):
  tachyon agents [--all-hosts] [--state S]  list live & blocked coding agents across hosts
  tachyon skill status|setup|remove         manage Claude Code, Antigravity, Codex, OpenCode & Pi skills/hooks
  tachyon status --state S [--app A] [--msg M] [--tty]
  tachyon hook <agent> <event>              internal hook entrypoint for coding agents

Global flags:
  -S, --server <host>   target host alias or ssh://... URI (default: $TACHYON_SERVER or local)
  --json                machine-readable JSON output
";

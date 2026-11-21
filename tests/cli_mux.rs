//! End-to-end integration tests exercising the compiled `tachyon` CLI binary
//! against an isolated local Mux v2 server (`TACHYON_SOCKET`).

#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::Value;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tachyon")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tachyon-cli-test-{tag}-{}-{}",
        std::process::id(),
        tachyon::mux::mono_now_ns()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_cli(sock: &PathBuf, hosts: &PathBuf, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(bin())
        .args(args)
        .env("TACHYON_SOCKET", sock)
        .env("TACHYON_HOSTS_FILE", hosts)
        .env_remove("TACHYON_SERVER")
        .env_remove("TACHYON_SESSION")
        .env_remove("TACHYON_WINDOW")
        .env_remove("TACHYON_BLOCK")
        .output()
        .expect("spawn tachyon cli");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn cli_end_to_end_server_sessions_splits_run_capture_and_bridge() {
    let dir = temp_dir("e2e");
    let sock = dir.join("server.sock");
    let hosts = dir.join("hosts.json");

    // 1. Start daemon and inspect status as JSON.
    let (code, out, err) = run_cli(&sock, &hosts, &["server", "start", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["running"], true);

    let (code, out, err) = run_cli(&sock, &hosts, &["server", "status", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["running"], true);
    assert_eq!(v["sessions"], 0);

    // 2. Create a session running an interactive shell script.
    let (code, out, err) = run_cli(
        &sock,
        &hosts,
        &[
            "new",
            "alpha",
            "--json",
            "--",
            "/bin/sh",
            "-c",
            "echo READY; read x; echo GOT:$x; sleep 15",
        ],
    );
    assert_eq!(code, 0, "stderr: {err}");
    let created: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(created["session"]["name"], "alpha");
    let block_id = created["block"]["id"].as_str().unwrap().to_string();
    let sess_id = created["session"]["id"].as_str().unwrap().to_string();

    // 3. Send input and capture output.
    std::thread::sleep(Duration::from_millis(120));
    let (code, _, err) = run_cli(&sock, &hosts, &["send", "-b", &block_id, "--enter", "hello-cli"]);
    assert_eq!(code, 0, "stderr: {err}");
    std::thread::sleep(Duration::from_millis(120));
    let (code, out, err) = run_cli(&sock, &hosts, &["capture", "-b", &block_id, "--lines", "10"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("READY") && out.contains("GOT:hello-cli"), "capture: {out:?}");

    // 4. `tachyon run --wait --capture` mirrors exact exit code and captures stdout.
    let (code, out, err) = run_cli(
        &sock,
        &hosts,
        &[
            "run",
            "--session",
            &sess_id,
            "--wait",
            "--capture",
            "--",
            "/bin/sh",
            "-c",
            "echo RUN_PAYLOAD; exit 7",
        ],
    );
    assert_eq!(code, 7, "stderr: {err}, out: {out}");
    assert!(out.contains("RUN_PAYLOAD"), "out: {out:?}");

    // 5. Rename session and verify via `tachyon ls --json`.
    let (code, _, err) = run_cli(&sock, &hosts, &["session", "rename", "alpha", "renamed-work"]);
    assert_eq!(code, 0, "stderr: {err}");
    let (code, out, err) = run_cli(&sock, &hosts, &["ls", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let snap: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(snap["sessions"][0]["name"], "renamed-work");

    // 6. Verify `tachyon bridge` relays live Mux v2 frames cleanly over stdio.
    let mut bridge = Command::new(bin())
        .args(["bridge", "--socket"])
        .arg(&sock)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tachyon bridge");
    let transport = tachyon::mux::transport::Transport {
        reader: Box::new(bridge.stdout.take().unwrap()),
        writer: Box::new(bridge.stdin.take().unwrap()),
        diag: std::sync::Arc::new(parking_lot::Mutex::new(String::new())),
        _guard: None,
    };
    let bridged = tachyon::mux::client::MuxClient::from_transport(transport, "cli", "bridge-test").unwrap();
    let bridged_snap = bridged.state().unwrap();
    assert_eq!(bridged_snap.sessions.len(), 1);
    assert_eq!(bridged_snap.sessions[0].name, "renamed-work");
    drop(bridged);
    let _ = bridge.kill();
    let _ = bridge.wait();

    // 7. Cleanly kill session and stop server.
    let (code, _, err) = run_cli(&sock, &hosts, &["session", "kill", "renamed-work"]);
    assert_eq!(code, 0, "stderr: {err}");
    let (code, _, err) = run_cli(&sock, &hosts, &["server", "stop"]);
    assert_eq!(code, 0, "stderr: {err}");

    let _ = std::fs::remove_dir_all(&dir);
}

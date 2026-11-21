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

    // 5.5. Emit OSC 7501 status inside a managed block and verify `ls --json` reports it.
    let status_cmd = format!(
        "'{}' status --state blocked --app claude --msg 'Approve tool' && sleep 5",
        bin()
    );
    let (code, out, err) = run_cli(
        &sock,
        &hosts,
        &["new", "agent-test", "--json", "--", "/bin/sh", "-c", &status_cmd],
    );
    assert_eq!(code, 0, "stderr: {err}");
    let agent_created: Value = serde_json::from_str(&out).unwrap();
    let agent_bid = agent_created["block"]["id"].as_str().unwrap().to_string();
    let mut saw_status = false;
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(60));
        let (c, ls_out, _) = run_cli(&sock, &hosts, &["ls", "--json"]);
        if c == 0 {
            let ls_json: Value = serde_json::from_str(&ls_out).unwrap();
            if let Some(blk) = ls_json["blocks"]
                .as_array()
                .and_then(|arr| arr.iter().find(|b| b["id"] == agent_bid))
            {
                if blk["status"]["state"] == "blocked"
                    && blk["status"]["app"] == "claude"
                    && blk["status"]["msg"] == "Approve tool"
                {
                    saw_status = true;
                    break;
                }
            }
        }
    }
    assert!(saw_status, "expected OSC 7501 blocked status in tachyon ls --json");
    let (code, _, err) = run_cli(&sock, &hosts, &["session", "kill", "agent-test"]);
    assert_eq!(code, 0, "stderr: {err}");

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

#[test]
fn cli_end_to_end_tcp_listener_with_bearer_token_auth() {
    let dir = temp_dir("tcp-e2e");
    let sock = dir.join("server.sock");
    let hosts = dir.join("hosts.json");
    let tok_file = dir.join("server.token");

    // Pick a free loopback port by briefly binding port 0.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let listen = format!("127.0.0.1:{port}");

    // 1. Pre-generate bearer token via `tachyon server token`.
    let (code, tok_out, err) = run_cli(
        &sock,
        &hosts,
        &["server", "token", "--token-file", tok_file.to_str().unwrap()],
    );
    assert_eq!(code, 0, "stderr: {err}");
    let token = tok_out.trim().to_string();
    assert_eq!(token.len(), 64, "token: {token:?}");

    // 2. Launch server with both local Unix socket and TCP listener.
    let mut srv = Command::new(bin())
        .args([
            "server",
            "run",
            "--socket",
            sock.to_str().unwrap(),
            "--listen",
            &listen,
            "--token-file",
            tok_file.to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tcp tachyon server");

    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    while !sock.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }

    // 3. Register a `tcp://` host with a WRONG token -> `hosts test` must fail.
    let uri = format!("tcp://{listen}");
    let (code, _, err) = run_cli(&sock, &hosts, &["hosts", "add", "bad-tcp", &uri, "--token", "nope"]);
    assert_eq!(code, 0, "stderr: {err}");
    let (code, _, err) = run_cli(&sock, &hosts, &["hosts", "test", "bad-tcp"]);
    assert_ne!(code, 0, "expected unauthorized failure for bad token");
    assert!(err.contains("unauthorized"), "stderr: {err}");

    // 4. Register a `tcp://` host with the REAL token -> `hosts test` & `run --wait` succeed!
    let (code, _, err) = run_cli(&sock, &hosts, &["hosts", "add", "ok-tcp", &uri, "--token", &token]);
    assert_eq!(code, 0, "stderr: {err}");
    let (code, out, err) = run_cli(&sock, &hosts, &["hosts", "test", "ok-tcp", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], true);

    let (code, out, err) = run_cli(
        &sock,
        &hosts,
        &[
            "-S",
            "ok-tcp",
            "run",
            "--wait",
            "--capture",
            "--",
            "/bin/sh",
            "-c",
            "echo OVER_TCP_OK",
        ],
    );
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("OVER_TCP_OK"), "out: {out:?}");

    // 5. Stop server cleanly over TCP!
    let (code, _, err) = run_cli(&sock, &hosts, &["-S", "ok-tcp", "server", "stop"]);
    assert_eq!(code, 0, "stderr: {err}");
    let _ = srv.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cli_hosts_color_sets_lists_and_resets_color() {
    let dir = temp_dir("hosts-color");
    let sock = dir.join("server.sock");
    let hosts = dir.join("hosts.json");

    // 1. Add a host with an explicit danger color.
    let (code, _, err) = run_cli(
        &sock,
        &hosts,
        &["hosts", "add", "prod-api", "ssh://prod-api", "--color", "red"],
    );
    assert_eq!(code, 0, "stderr: {err}");

    let (code, out, err) = run_cli(&sock, &hosts, &["hosts", "ls", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let list: Value = serde_json::from_str(&out).unwrap();
    let prod = list["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["alias"] == "prod-api")
        .expect("prod-api listed");
    assert_eq!(prod["color"], "red");

    // 2. Update color via `tachyon hosts color` to a custom danger hex.
    let (code, _, err) = run_cli(&sock, &hosts, &["hosts", "color", "prod-api", "danger:#ef4444"]);
    assert_eq!(code, 0, "stderr: {err}");
    let (code, out, err) = run_cli(&sock, &hosts, &["hosts", "ls"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("danger:#ef4444"), "ls output: {out}");

    // 3. Invalid color is rejected cleanly.
    let (code, _, err) = run_cli(&sock, &hosts, &["hosts", "color", "prod-api", "not-a-color"]);
    assert_ne!(code, 0);
    assert!(err.contains("invalid host colour"), "stderr: {err}");

    // 4. Reset to `auto` clears explicit color back to null/omitted.
    let (code, _, err) = run_cli(&sock, &hosts, &["hosts", "color", "prod-api", "auto"]);
    assert_eq!(code, 0, "stderr: {err}");
    let (code, out, err) = run_cli(&sock, &hosts, &["hosts", "ls", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let list: Value = serde_json::from_str(&out).unwrap();
    let prod = list["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["alias"] == "prod-api")
        .expect("prod-api listed");
    assert!(prod.get("color").is_none() || prod["color"].is_null(), "prod: {prod}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cli_workspace_init_up_idempotent_save_recreate_down_and_rm() {
    let dir = temp_dir("workspace-e2e");
    let sock = dir.join("server.sock");
    let hosts = dir.join("hosts.json");
    let ws_dir = dir.join("workspaces");
    std::fs::create_dir_all(&ws_dir).unwrap();

    let run_ws = |args: &[&str]| -> (i32, String, String) {
        let out = Command::new(bin())
            .args(args)
            .env("TACHYON_SOCKET", &sock)
            .env("TACHYON_HOSTS_FILE", &hosts)
            .env("TACHYON_WORKSPACES_DIR", &ws_dir)
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
    };

    // 1. Start local isolated server.
    let (code, _, err) = run_ws(&["server", "start"]);
    assert_eq!(code, 0, "stderr: {err}");

    // 2. Write a 3-pane recursive layout + 2nd window panes-shorthand template.
    let tpl_path = ws_dir.join("dev-stack.json");
    let tpl = r#"{
      // End-to-end workspace fixture
      "name": "dev-stack",
      "description": "3-pane nested layout + 2nd window",
      "focus": { "host": "local", "session": "dev-stack", "window": "code" },
      "sessions": [
        {
          "host": "local",
          "name": "dev-stack",
          "cwd": "~",
          "windows": [
            {
              "name": "code",
              "layout": {
                "split": "right",
                "ratio": 0.65,
                "a": { "label": "left-main", "cmd": ["/bin/sh", "-c", "echo LEFT_OK; sleep 20"] },
                "b": {
                  "split": "down",
                  "ratio": 0.4,
                  "a": { "label": "top-right", "focus": true, "cmd": ["/bin/sh", "-c", "echo TOPRIGHT_OK; sleep 20"] },
                  "b": { "label": "bot-right", "cmd": ["/bin/sh", "-c", "echo BOTRIGHT_OK; sleep 20"] }
                }
              }
            },
            {
              "name": "ops",
              "panes": [
                { "label": "ops-1", "cmd": ["/bin/sh", "-c", "echo OPS1; sleep 20"] },
                { "label": "ops-2", "split": "right", "cmd": ["/bin/sh", "-c", "echo OPS2; sleep 20"] }
              ]
            }
          ]
        }
      ]
    }"#;
    std::fs::write(&tpl_path, tpl).unwrap();

    // 3. `workspace ls --json` lists it with accurate counts (1s/2w/5p).
    let (code, out, err) = run_ws(&["workspace", "ls", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let ls_v: Value = serde_json::from_str(&out).unwrap();
    let arr = ls_v["workspaces"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "dev-stack");
    assert_eq!(arr[0]["windows"], 2);
    assert_eq!(arr[0]["panes"], 5);

    // 4. `workspace up dev-stack --json` creates the exact 3-pane nested tree & ratios.
    let (code, out, err) = run_ws(&["workspace", "up", "dev-stack", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let up_v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(up_v["sessions"][0]["reused"], false);
    assert_eq!(up_v["sessions"][0]["windows"].as_array().unwrap().len(), 2);
    assert_eq!(up_v["sessions"][0]["windows"][0]["blocks"].as_array().unwrap().len(), 3);
    assert_eq!(up_v["sessions"][0]["windows"][1]["blocks"].as_array().unwrap().len(), 2);
    let focus_wid = up_v["focus"]["window_id"].as_str().unwrap().to_string();
    assert_eq!(focus_wid, up_v["sessions"][0]["windows"][0]["window_id"]);

    // Verify exact nested layout ratios & focus on server state.
    let (code, state_out, err) = run_ws(&["ls", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let snap: Value = serde_json::from_str(&state_out).unwrap();
    let code_win = snap["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == focus_wid)
        .expect("code window present");
    assert_eq!(code_win["name"], "code");
    assert_eq!(code_win["layout"]["dir"], "right");
    assert!((code_win["layout"]["ratio"].as_f64().unwrap() - 0.65).abs() < 0.01);
    assert_eq!(code_win["layout"]["b"]["dir"], "down");
    assert!((code_win["layout"]["b"]["ratio"].as_f64().unwrap() - 0.4).abs() < 0.01);

    // 5. Second `workspace up dev-stack --json` is idempotent (reused = true).
    let (code, out, err) = run_ws(&["workspace", "up", "dev-stack", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let up2: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(up2["sessions"][0]["reused"], true);
    assert_eq!(up2["sessions"][0]["session_id"], up_v["sessions"][0]["session_id"]);

    // 6. `workspace save captured` snapshots the live multi-window split tree cleanly.
    let (code, out, err) = run_ws(&["workspace", "save", "captured", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let saved: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(saved["name"], "captured");
    assert_eq!(saved["windows"], 2);
    assert_eq!(saved["panes"], 5);

    // 7. `workspace down dev-stack` stops the session; `workspace rm` deletes templates.
    let (code, out, err) = run_ws(&["workspace", "down", "dev-stack", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let down_v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(down_v["stopped"].as_array().unwrap().len(), 1);

    let (code, _, err) = run_ws(&["workspace", "init", "starter-demo"]);
    assert_eq!(code, 0, "stderr: {err}");
    let (code, _, err) = run_ws(&["workspace", "rm", "starter-demo"]);
    assert_eq!(code, 0, "stderr: {err}");

    let (code, _, err) = run_ws(&["server", "stop"]);
    assert_eq!(code, 0, "stderr: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}

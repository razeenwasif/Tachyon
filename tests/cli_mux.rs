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
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    // Keep Unix socket paths well under macOS SUN_LEN (104 bytes).
    let base = if cfg!(unix) { PathBuf::from("/tmp") } else { std::env::temp_dir() };
    let dir = base.join(format!("ty-{tag}-{}-{n}", std::process::id()));
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

#[test]
fn cli_agents_lists_and_filters_live_osc7501_agents_by_urgency() {
    let dir = temp_dir("agents-e2e");
    let sock = dir.join("server.sock");
    let hosts = dir.join("hosts.json");

    let (code, _, err) = run_cli(&sock, &hosts, &["server", "start"]);
    assert_eq!(code, 0, "stderr: {err}");

    let cmd_done = format!(
        "'{}' status --state done --app claude --msg 'Build green' && sleep 15",
        bin()
    );
    let cmd_blocked = format!(
        "'{}' status --state blocked --app antigravity --msg 'Approve push' && sleep 15",
        bin()
    );

    let (code, _, err) = run_cli(
        &sock,
        &hosts,
        &["new", "done-sess", "--", "/bin/sh", "-c", &cmd_done],
    );
    assert_eq!(code, 0, "stderr: {err}");
    let (code, _, err) = run_cli(
        &sock,
        &hosts,
        &["new", "blocked-sess", "--", "/bin/sh", "-c", &cmd_blocked],
    );
    assert_eq!(code, 0, "stderr: {err}");

    // Wait briefly for both OSC 7501 sequences to be parsed by the server.
    let mut ready = false;
    for _ in 0..25 {
        std::thread::sleep(Duration::from_millis(60));
        let (c, out, _) = run_cli(&sock, &hosts, &["agents", "--json"]);
        if c == 0 {
            let v: Value = serde_json::from_str(&out).unwrap();
            if v["counts"]["total"] == 2
                && v["counts"]["blocked"] == 1
                && v["counts"]["done"] == 1
            {
                let agents = v["agents"].as_array().unwrap();
                // Blocked agent must rank ahead of Done agent.
                assert_eq!(agents[0]["state"], "blocked");
                assert_eq!(agents[0]["agent"], "Antigravity");
                assert_eq!(agents[0]["message"], "Approve push");
                assert_eq!(agents[1]["state"], "done");
                assert_eq!(agents[1]["agent"], "Claude Code");
                ready = true;
                break;
            }
        }
    }
    assert!(ready, "expected both live OSC 7501 agents in `tachyon agents --json`");

    // `--state attention` filters down to only the blocked agent.
    let (code, out, err) = run_cli(&sock, &hosts, &["agents", "--state", "attention", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let attn: Value = serde_json::from_str(&out).unwrap();
    let arr = attn["agents"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["state"], "blocked");

    let (code, _, err) = run_cli(&sock, &hosts, &["server", "stop"]);
    assert_eq!(code, 0, "stderr: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cli_send_all_broadcasts_input_to_all_split_panes_in_window() {
    let dir = temp_dir("broadcast-e2e");
    let sock = dir.join("server.sock");
    let hosts = dir.join("hosts.json");

    let (code, _, err) = run_cli(&sock, &hosts, &["server", "start"]);
    assert_eq!(code, 0, "stderr: {err}");

    let pane_script = "read line; echo BCAST:$line; sleep 15";
    let (code, out, err) = run_cli(
        &sock,
        &hosts,
        &["new", "bcast", "--json", "--", "/bin/sh", "-c", pane_script],
    );
    assert_eq!(code, 0, "stderr: {err}");
    let created: Value = serde_json::from_str(&out).unwrap();
    let wid = created["window"]["id"].as_str().unwrap().to_string();
    let b1 = created["block"]["id"].as_str().unwrap().to_string();

    let (code, out, err) = run_cli(
        &sock,
        &hosts,
        &["split", "--right", "-b", &b1, "--json", "--", "/bin/sh", "-c", pane_script],
    );
    assert_eq!(code, 0, "stderr: {err}");
    let s2: Value = serde_json::from_str(&out).unwrap();
    let b2 = s2["block"]["id"].as_str().unwrap().to_string();

    let (code, out, err) = run_cli(
        &sock,
        &hosts,
        &["split", "--down", "-b", &b2, "--json", "--", "/bin/sh", "-c", pane_script],
    );
    assert_eq!(code, 0, "stderr: {err}");
    let s3: Value = serde_json::from_str(&out).unwrap();
    let b3 = s3["block"]["id"].as_str().unwrap().to_string();

    std::thread::sleep(Duration::from_millis(120));

    // Broadcast once to all 3 panes in the window.
    let (code, out, err) = run_cli(
        &sock,
        &hosts,
        &["send", "--all", "-w", &wid, "--enter", "--json", "hello-all"],
    );
    assert_eq!(code, 0, "stderr: {err}");
    let sent: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(sent["window_id"], wid);
    assert_eq!(sent["blocks"].as_array().unwrap().len(), 3);

    std::thread::sleep(Duration::from_millis(150));

    for bid in [&b1, &b2, &b3] {
        let (code, cap, err) = run_cli(&sock, &hosts, &["capture", "-b", bid, "--lines", "10"]);
        assert_eq!(code, 0, "stderr: {err}");
        assert!(
            cap.contains("BCAST:hello-all"),
            "expected broadcast line in pane {bid}, got: {cap:?}"
        );
    }

    let (code, _, err) = run_cli(&sock, &hosts, &["server", "stop"]);
    assert_eq!(code, 0, "stderr: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cli_server_rewind_preserves_alt_screen_frames_and_provenance_without_gui() {
    let dir = temp_dir("rewind-e2e");
    let sock = dir.join("server.sock");
    let hosts = dir.join("hosts.json");

    let (code, _, err) = run_cli(&sock, &hosts, &["server", "start"]);
    assert_eq!(code, 0, "stderr: {err}");

    // Emit distinct frames separated by >60ms (> FRAME_GAP_MS = 40ms), including
    // an alternate-screen TUI screen that is exited before we inspect it!
    let script = r#"printf 'FRAME_ONE\r\n'; sleep 0.08; printf '\033[?1049h\033[HALT_SCREEN_HUD\r\n'; sleep 0.08; printf '\033[?1049lBACK_TO_MAIN\r\n'; sleep 15"#;
    let (code, out, err) = run_cli(
        &sock,
        &hosts,
        &["new", "rw-sess", "--json", "--", "/bin/sh", "-c", script],
    );
    assert_eq!(code, 0, "stderr: {err}");
    let created: Value = serde_json::from_str(&out).unwrap();
    let bid = created["block"]["id"].as_str().unwrap().to_string();

    // Wait for all 3 bursts to land on the server while NO GUI is attached.
    let mut frames_val = Value::Null;
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(60));
        let (c, cap, _) = run_cli(&sock, &hosts, &["capture", "-b", &bid]);
        if c == 0 && cap.contains("BACK_TO_MAIN") {
            let (lc, lout, lerr) = run_cli(&sock, &hosts, &["rewind", "-b", &bid, "--list", "--json"]);
            assert_eq!(lc, 0, "stderr: {lerr}");
            frames_val = serde_json::from_str(&lout).unwrap();
            if frames_val["frames"].as_array().is_some_and(|a| a.len() >= 3) {
                break;
            }
        }
    }
    let frames = frames_val["frames"]
        .as_array()
        .expect("server rewind frames array");
    assert!(frames.len() >= 3, "expected >=3 frames, got: {frames_val}");

    // Find the historical alternate-screen frame (which normal capture no longer has!).
    let mut alt_seq = None;
    for f in frames {
        let seq = f["seq"].as_u64().unwrap();
        let (fc, fout, ferr) = run_cli(
            &sock,
            &hosts,
            &["rewind", "-b", &bid, "--seq", &seq.to_string(), "--json"],
        );
        assert_eq!(fc, 0, "stderr: {ferr}");
        let fv: Value = serde_json::from_str(&fout).unwrap();
        if fv["text"].as_str().unwrap_or("").contains("ALT_SCREEN_HUD") {
            alt_seq = Some(seq);
            break;
        }
    }
    let alt_seq = alt_seq.expect("server-side Rewind should recover exited alt-screen frame");

    // Verify server-side cell provenance on cell (1,1) ('A' in ALT_SCREEN_HUD).
    let (pc, pout, perr) = run_cli(
        &sock,
        &hosts,
        &[
            "rewind",
            "-b",
            &bid,
            "--seq",
            &alt_seq.to_string(),
            "--provenance",
            "1,1",
            "--json",
        ],
    );
    assert_eq!(pc, 0, "stderr: {perr}");
    let pv: Value = serde_json::from_str(&pout).unwrap();
    assert!(
        pv["provenance"]["context"]
            .as_str()
            .unwrap_or("")
            .contains('A'),
        "provenance: {pv}"
    );

    let (code, _, err) = run_cli(&sock, &hosts, &["server", "stop"]);
    assert_eq!(code, 0, "stderr: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}

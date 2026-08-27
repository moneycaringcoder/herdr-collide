//! Process-level bounds for repository filesystem reads in one refresh.

#[path = "fixtures.rs"]
mod fixtures;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use fixtures::Fixture;

#[test]
fn a_slow_repository_degrades_when_the_cycle_budget_expires() {
    let fixture = Fixture::new("bounded-cycle");
    let worktree = fixture.worktree("slow-worktree", "slow-worktree");
    fixture.leaking_fsmonitor(&worktree, 90);

    let root = fixture.root();
    let socket = root.join("bounded.sock");
    let listener = UnixListener::bind(&socket).expect("bind fake herdr");
    let config_dir = root.join("config");
    let state_dir = root.join("state");
    std::fs::create_dir_all(&config_dir).expect("create config");
    std::fs::create_dir_all(&state_dir).expect("create state");
    std::fs::write(
        config_dir.join("config.json"),
        r#"{"git_timeout_seconds":10,"cycle_timeout_seconds":1,"predict_conflicts":false,"base_ref":"main"}"#,
    )
    .expect("write config");

    let workspace = serde_json::json!({
        "workspace_id": "slow",
        "number": 1,
        "label": "slow",
        "focused": true,
        "pane_count": 0,
        "tab_count": 0,
        "active_tab_id": "slow:t1",
        "agent_status": "idle",
        "worktree": {
            "repo_key": fixture.repo.join(".git"),
            "repo_name": "repo",
            "repo_root": fixture.repo,
            "checkout_path": worktree,
            "is_linked_worktree": true
        }
    });
    let server = std::thread::spawn(move || loop {
        let (stream, _) = listener.accept().expect("accept collide");
        let mut request = String::new();
        BufReader::new(&stream)
            .read_line(&mut request)
            .expect("read request");
        if request.is_empty() {
            continue;
        }
        let reply = serde_json::json!({
            "id": "collide:1",
            "result": {
                "type": "session_snapshot",
                "snapshot": {
                    "protocol": 19,
                    "version": "0.8.0",
                    "layouts": [],
                    "tabs": [],
                    "panes": [],
                    "agents": [],
                    "workspaces": [workspace]
                }
            }
        });
        writeln!(&stream, "{reply}").expect("write reply");
        break;
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_collide"))
        .args(["--once", "--base-ref", "main"])
        .env("HERDR_SOCKET_PATH", &socket)
        .env("HERDR_PLUGIN_ID", "test.collide.bounded")
        .env("HERDR_WORKSPACE_ID", "slow")
        .env("HERDR_PLUGIN_CONFIG_DIR", &config_dir)
        .env("HERDR_PLUGIN_STATE_DIR", &state_dir)
        .env("HOME", root.join("home"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn collide");
    server.join().expect("fake herdr server");

    let deadline = Instant::now() + Duration::from_secs(4);
    while child.try_wait().expect("poll collide").is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if child.try_wait().expect("final poll").is_none() {
        let _ = child.kill();
    }
    let output = child.wait_with_output().expect("collect collide");

    assert!(
        output.status.success(),
        "refresh exceeded its bound or failed: {output:?}"
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 output");
    assert!(stdout.contains("cycle-timeout"), "{stdout}");
}

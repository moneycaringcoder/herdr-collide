//! Process-level shutdown coverage for the redraw-in-place detail pane.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
fn sighup_restores_the_cursor_before_the_watch_pane_exits() {
    let root = std::env::temp_dir().join(format!("collide-watch-sighup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("state")).expect("create state");
    std::fs::create_dir_all(root.join("config")).expect("create config");
    let socket = root.join("herdr.sock");
    let listener = UnixListener::bind(&socket).expect("bind fake herdr");

    let server = std::thread::spawn(move || loop {
        let (stream, _) = listener.accept().expect("accept collide");
        let mut request = String::new();
        BufReader::new(&stream)
            .read_line(&mut request)
            .expect("read request");
        if request.is_empty() {
            continue;
        }
        assert!(request.contains("session.snapshot"), "{request}");
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
                    "workspaces": []
                }
            }
        });
        let mut stream = &stream;
        writeln!(stream, "{reply}").expect("write reply");
        break;
    });

    let child = Command::new(env!("CARGO_BIN_EXE_collide"))
        .args(["--watch", "--interval", "3600"])
        .env("HERDR_SOCKET_PATH", &socket)
        .env("HERDR_PLUGIN_ID", "test.collide.watch")
        .env("HERDR_PLUGIN_STATE_DIR", root.join("state"))
        .env("HERDR_PLUGIN_CONFIG_DIR", root.join("config"))
        .env("HOME", root.join("home"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn watch pane");

    server.join().expect("fake herdr server");
    std::thread::sleep(Duration::from_millis(100));
    // SAFETY: `child.id()` is the live process spawned above.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGHUP) }, 0);
    let output = child.wait_with_output().expect("collect watch output");
    let _ = std::fs::remove_dir_all(&root);

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.starts_with(b"\x1b[?25l"), "{output:?}");
    assert!(output.stdout.ends_with(b"\x1b[?25h\x1b[0m"), "{output:?}");
}

//! Process-level shutdown coverage for the interactive detail pane.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
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

    let mut master_fd = -1;
    let mut slave_fd = -1;
    // SAFETY: openpty initialises both owned file descriptors; null termios and
    // winsize pointers request the platform defaults.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    // SAFETY: openpty returned two fresh owned descriptors.
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let probe = slave.try_clone().expect("clone slave for termios probe");
    let mut child = Command::new(env!("CARGO_BIN_EXE_collide"))
        .args(["--watch", "--interval", "3600"])
        .env("HERDR_SOCKET_PATH", &socket)
        .env("HERDR_PLUGIN_ID", "test.collide.watch")
        .env("HERDR_PLUGIN_STATE_DIR", root.join("state"))
        .env("HERDR_PLUGIN_CONFIG_DIR", root.join("config"))
        .env("HOME", root.join("home"))
        .stdin(Stdio::from(slave.try_clone().expect("clone slave stdin")))
        .stdout(Stdio::from(slave.try_clone().expect("clone slave stdout")))
        .stderr(Stdio::from(slave))
        .spawn()
        .expect("spawn watch pane");

    server.join().expect("fake herdr server");
    std::thread::sleep(Duration::from_millis(100));
    // SAFETY: `child.id()` is the live process spawned above.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGHUP) }, 0);
    let status = child.wait().expect("wait for watch pane");

    let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: probe is a live PTY slave and termios points to writable storage.
    assert_eq!(
        unsafe { libc::tcgetattr(probe.as_raw_fd(), termios.as_mut_ptr()) },
        0
    );
    // SAFETY: tcgetattr succeeded and initialised termios.
    let termios = unsafe { termios.assume_init() };
    assert_ne!(termios.c_lflag & libc::ICANON, 0, "canonical mode restored");
    assert_ne!(termios.c_lflag & libc::ECHO, 0, "echo restored");
    drop(probe);

    let mut output = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match master.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => output.extend_from_slice(&chunk[..read]),
            // Linux PTY masters report EIO after the last slave closes; it is
            // the terminal equivalent of EOF, not a failed read.
            Err(err) if err.raw_os_error() == Some(libc::EIO) => break,
            Err(err) => panic!("read PTY output: {err}"),
        }
    }
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        status.success(),
        "{status:?}: {}",
        String::from_utf8_lossy(&output)
    );
    for sequence in [
        b"\x1b[?1049h".as_slice(),
        b"\x1b[?25l".as_slice(),
        b"\x1b[?1049l".as_slice(),
        b"\x1b[?25h".as_slice(),
    ] {
        assert!(
            output
                .windows(sequence.len())
                .any(|window| window == sequence),
            "missing {sequence:?}: {}",
            String::from_utf8_lossy(&output)
        );
    }
}

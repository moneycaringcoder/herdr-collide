//! Wire-level tests for the socket client.
//!
//! Every test stands up a real Unix socket server in a temp directory and
//! asserts the bytes the client puts on the wire, because the parts of this
//! protocol that bite (mandatory `{}` params, one request per connection, the
//! merge-patch clear with no TTL) are invisible from the Rust API alone.
//!
//! No running herdr is required, and nothing here touches the user's state.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use collide::herdr::{error_code, Herdr};
use serde_json::{json, Value};

const SOURCE: &str = "test.collide";

/// `HERDR_SOCKET_PATH` and `HERDR_PLUGIN_ID` are process-global, so the tests
/// that set them have to run one at a time even though cargo runs them on
/// separate threads.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What the server does with one connection.
#[derive(Clone)]
enum Reply {
    /// Answer, then close — the real server's behaviour.
    Line(String),
    /// Read the request and close without answering, which is what a client
    /// sees when it lands on a socket the server is tearing down.
    Eof,
}

struct TestServer {
    path: PathBuf,
    dir: PathBuf,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TestServer {
    fn start(replies: Vec<Reply>) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "collide-wire-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        // Kept short: a Unix socket path is capped at ~108 bytes.
        let path = dir.join("s.sock");

        let listener = UnixListener::bind(&path).expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut replies = replies.into_iter();
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream.set_nonblocking(false).expect("blocking");
                            let mut line = String::new();
                            let mut reader = BufReader::new(&stream);
                            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                                continue;
                            }
                            requests.lock().expect("requests").push(line);
                            match replies.next() {
                                Some(Reply::Line(reply)) => {
                                    let mut stream = &stream;
                                    let _ = stream.write_all(reply.as_bytes());
                                    let _ = stream.write_all(b"\n");
                                    let _ = stream.flush();
                                }
                                // Exhausted or an explicit EOF: just close, the
                                // way herdr closes after answering.
                                Some(Reply::Eof) | None => {}
                            }
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            })
        };

        Self {
            path,
            dir,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn client(&self) -> Herdr {
        std::env::set_var("HERDR_SOCKET_PATH", &self.path);
        std::env::set_var("HERDR_PLUGIN_ID", SOURCE);
        Herdr::connect().expect("connect")
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests").clone()
    }

    /// The single request, parsed, with its raw framing already asserted.
    fn only_request(&self) -> Value {
        let requests = self.requests();
        assert_eq!(requests.len(), 1, "expected one request, got {requests:?}");
        parse_framed(&requests[0])
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// One line, newline-terminated, with no trailing framing of its own.
fn parse_framed(raw: &str) -> Value {
    assert!(raw.ends_with('\n'), "request must be newline-terminated");
    assert_eq!(
        raw.matches('\n').count(),
        1,
        "one request per line, got {raw:?}"
    );
    serde_json::from_str(raw.trim_end()).expect("request is JSON")
}

/// Every mutation answers with this, verified live against herdr 0.8.0 for both
/// a set and a clear of `workspace.report_metadata`.
fn ok_reply() -> Reply {
    Reply::Line(json!({"id": "collide:1", "result": {"type": "ok"}}).to_string())
}

/// `notification.show` does **not** answer `ok`: it reports whether the toast
/// was actually shown, and why not when it was not.
fn notification_reply() -> Reply {
    Reply::Line(
        json!({
            "id": "collide:1",
            "result": {"type": "notification_show", "shown": true, "reason": "shown"}
        })
        .to_string(),
    )
}

/// A `session.snapshot` reply in the real envelope: the arrays live under
/// `snapshot`, one level below `result`, alongside a `type` discriminator.
/// Reading them off `result` yields zero workspaces and looks exactly like an
/// idle session, which is why every reply here mirrors the wire shape rather
/// than the shape the client would find convenient.
fn snapshot_reply(snapshot: Value) -> Reply {
    Reply::Line(
        json!({
            "id": "collide:1",
            "result": {"type": "session_snapshot", "snapshot": snapshot}
        })
        .to_string(),
    )
}

/// Structure copied from `herdr api snapshot` on a live 0.8.0 server, paths
/// redacted. Fields the client ignores are kept deliberately: a reply that
/// carries only what the client reads cannot catch the client reading the wrong
/// thing.
fn snapshot_with_one_repo() -> Value {
    json!({
        "focused_pane_id": "w6:p1",
        "focused_tab_id": "w6:t1",
        "focused_workspace_id": "w6",
        "protocol": 19,
        "version": "0.8.0",
        "layouts": [],
        "tabs": [{"tab_id": "w6:t1", "workspace_id": "w6", "focused": true}],
        "workspaces": [{
            "workspace_id": "w6",
            "number": 4,
            "label": "feature",
            "focused": false,
            "pane_count": 1,
            "tab_count": 1,
            "active_tab_id": "w6:t1",
            "agent_status": "done",
            "worktree": {
                "repo_key": "/repo/.git",
                "repo_name": "repo",
                "repo_root": "/repo",
                "checkout_path": "/repo",
                "is_linked_worktree": false
            }
        }],
        "panes": [{
            "pane_id": "w6:p1",
            "terminal_id": "term_6591127b6323a6",
            "workspace_id": "w6",
            "tab_id": "w6:t1",
            "focused": false,
            "agent": "claude",
            "agent_status": "done",
            "cwd": "/repo",
            "revision": 1345
        }],
        "agents": [{
            "pane_id": "w6:p1",
            "tab_id": "w6:t1",
            "workspace_id": "w6",
            "agent": "claude",
            // An object on the wire, not a string: anything treating it as a
            // display name silently gets nothing.
            "agent_session": {
                "agent": "claude",
                "kind": "id",
                "source": "herdr:claude",
                "value": "f413fb98-8457-405d-9a9d-d3f86fa9a252"
            },
            "agent_status": "done"
        }]
    })
}

#[test]
fn request_framing_is_a_single_json_line_with_object_params() {
    let _guard = env_lock();
    let server = TestServer::start(vec![snapshot_reply(snapshot_with_one_repo())]);
    let mut client = server.client();

    client.checkouts().expect("snapshot");

    let request = server.only_request();
    assert_eq!(request["method"], "session.snapshot");
    assert!(request["id"].is_string(), "id must be a string");
    // Mandatory and an object even when empty — never null, never absent.
    assert_eq!(request["params"], json!({}));
    assert!(request["params"].is_object());
    assert!(
        request.get("jsonrpc").is_none(),
        "this protocol has no jsonrpc field"
    );
}

#[test]
fn one_request_per_connection_is_survived_by_reconnecting() {
    let _guard = env_lock();
    // The first connection is read and closed without an answer, exactly as a
    // server that has just handed off behaves. The retry must land the call.
    let server = TestServer::start(vec![Reply::Eof, snapshot_reply(snapshot_with_one_repo())]);
    let mut client = server.client();

    let checkouts = client.checkouts().expect("retry should succeed");

    assert_eq!(checkouts.len(), 1);
    assert_eq!(
        server.requests().len(),
        2,
        "the dropped connection must be retried on a fresh one"
    );
}

#[test]
fn set_badge_sends_source_tokens_and_ttl() {
    let _guard = env_lock();
    let server = TestServer::start(vec![ok_reply()]);
    let mut client = server.client();

    client
        .set_badge("w6", "collide_conflict", "✘ 2", 15_000)
        .expect("set");

    let params = server.only_request()["params"].clone();
    assert_eq!(
        params,
        json!({
            "workspace_id": "w6",
            "source": SOURCE,
            "tokens": {"collide_conflict": "✘ 2"},
            "ttl_ms": 15_000
        })
    );
    // No `$` prefix on the wire: that syntax belongs to herdr's config.toml.
    assert!(!params["tokens"]
        .as_object()
        .unwrap()
        .keys()
        .any(|key| key.starts_with('$')));
}

#[test]
fn set_badge_clamps_ttl_into_the_protocol_range() {
    let _guard = env_lock();
    let server = TestServer::start(vec![ok_reply(), ok_reply()]);
    let mut client = server.client();

    client
        .set_badge("w6", "collide_clean", "ok", 0)
        .expect("low");
    client
        .set_badge("w6", "collide_clean", "ok", u64::MAX)
        .expect("high");

    let requests = server.requests();
    assert_eq!(parse_framed(&requests[0])["params"]["ttl_ms"], 1);
    assert_eq!(
        parse_framed(&requests[1])["params"]["ttl_ms"],
        86_400_000u64
    );
}

#[test]
fn clear_badge_sends_a_null_token_and_no_ttl() {
    let _guard = env_lock();
    let server = TestServer::start(vec![ok_reply()]);
    let mut client = server.client();

    client.clear_badge("w6", "collide_overlap").expect("clear");

    let params = server.only_request()["params"].clone();
    // Tokens are a merge patch: null deletes the name, and a TTL alongside a
    // delete is rejected.
    assert!(params["tokens"]["collide_overlap"].is_null());
    assert!(
        params.get("ttl_ms").is_none(),
        "a clear must omit ttl_ms entirely, got {params}"
    );
    assert_eq!(params["source"], SOURCE);
}

#[test]
fn error_envelopes_surface_as_a_typed_error() {
    let _guard = env_lock();
    let server = TestServer::start(vec![Reply::Line(
        json!({
            "id": "collide:1",
            "error": {"code": "workspace_not_found", "message": "no such workspace"}
        })
        .to_string(),
    )]);
    let mut client = server.client();

    let err = client
        .set_badge("gone", "collide_clean", "ok", 15_000)
        .expect_err("an error envelope is a failure");

    assert_eq!(error_code(&*err), Some("workspace_not_found"));
    assert!(err.to_string().contains("no such workspace"));
    // A rejected request is not a transport failure, so it must not be retried.
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn transport_failure_after_the_retry_is_not_a_herdr_error_code() {
    let _guard = env_lock();
    let server = TestServer::start(vec![Reply::Eof, Reply::Eof]);
    let mut client = server.client();

    let err = client.checkouts().expect_err("both attempts fail");

    assert_eq!(
        error_code(&*err),
        None,
        "callers must be able to tell blindness from rejection"
    );
}

#[test]
fn non_git_workspaces_are_skipped_rather_than_failing() {
    let _guard = env_lock();
    let server = TestServer::start(vec![snapshot_reply(json!({
        "protocol": 19,
        "version": "0.8.0",
        "workspaces": [
            {"workspace_id": "w1", "label": "notes", "number": 1, "agent_status": "idle"},
            {
                "workspace_id": "w2",
                "label": "main",
                "number": 2,
                "worktree": {
                    "repo_key": "/repo/.git",
                    "repo_name": "repo",
                    "repo_root": "/repo",
                    "checkout_path": "/repo",
                    "is_linked_worktree": false
                }
            },
            {
                "workspace_id": "w3",
                "label": "fix",
                "number": 3,
                "worktree": {
                    "repo_key": "/repo/.git",
                    "repo_name": "repo",
                    "repo_root": "/repo",
                    "checkout_path": "/wt/fix",
                    "is_linked_worktree": true
                }
            }
        ],
        "panes": [{"pane_id": "w3:p1", "workspace_id": "w3", "tab_id": "w3:t1", "agent": "codex"}],
        "agents": [{
            "pane_id": "w2:p1",
            "tab_id": "w2:t1",
            "workspace_id": "w2",
            "agent": "claude",
            "agent_session": {"agent": "claude", "kind": "id", "source": "herdr:claude", "value": "s1"},
            "name": "gitsmith"
        }]
    }))]);
    let mut client = server.client();

    let checkouts = client.checkouts().expect("snapshot");

    assert_eq!(
        checkouts
            .iter()
            .map(|c| c.workspace_id.as_str())
            .collect::<Vec<_>>(),
        ["w2", "w3"],
        "a workspace with no worktree key is not a repo, not an error"
    );

    let main = &checkouts[0];
    assert_eq!(main.workspace_label, "main");
    assert_eq!(main.repo_key.0, "/repo/.git");
    assert_eq!(main.repo_root, PathBuf::from("/repo"));
    assert_eq!(main.checkout_path, PathBuf::from("/repo"));
    assert!(!main.is_linked_worktree);
    assert_eq!(
        main.agent.as_deref(),
        Some("gitsmith"),
        "the user's own name for the agent wins over the program name"
    );
    // Branches come from `worktree.list` in a later pass.
    assert_eq!(main.branch, None);

    let linked = &checkouts[1];
    assert!(linked.is_linked_worktree);
    assert_eq!(
        linked.agent.as_deref(),
        Some("codex"),
        "panes[] carries the agent when agents[] has no row"
    );
}

#[test]
fn notify_sends_title_and_body() {
    let _guard = env_lock();
    // Not an `ok` envelope: this method reports whether the toast was shown.
    let server = TestServer::start(vec![notification_reply()]);
    let mut client = server.client();

    client.notify("collide", "2 conflicts").expect("notify");

    let request = server.only_request();
    assert_eq!(request["method"], "notification.show");
    assert_eq!(request["params"]["title"], "collide");
    assert_eq!(request["params"]["body"], "2 conflicts");
}

/// The regression test for the shape bug: the arrays live under `snapshot`, and
/// a client reading them off `result` finds nothing while reporting success.
#[test]
fn checkouts_are_read_from_the_nested_snapshot_object() {
    let _guard = env_lock();
    let server = TestServer::start(vec![snapshot_reply(snapshot_with_one_repo())]);
    let mut client = server.client();

    let checkouts = client.checkouts().expect("snapshot");

    assert_eq!(checkouts.len(), 1, "a live session must not read as idle");
    assert_eq!(checkouts[0].workspace_id, "w6");
    assert_eq!(checkouts[0].repo_key.0, "/repo/.git");
    assert_eq!(checkouts[0].agent.as_deref(), Some("claude"));
}

/// The other half of the same bug: if the payload ever stops carrying
/// `snapshot`, that must be loud. An empty checkout list is indistinguishable
/// from an idle session, so silently returning one would hide the breakage
/// exactly the way the original bug did.
#[test]
fn a_reply_without_the_snapshot_key_is_an_error_not_an_empty_session() {
    let _guard = env_lock();
    // The arrays are present, but at the level the buggy client read them from.
    let flattened = {
        let mut result = snapshot_with_one_repo();
        result["type"] = json!("session_snapshot");
        result
    };
    let server = TestServer::start(vec![Reply::Line(
        json!({"id": "collide:1", "result": flattened}).to_string(),
    )]);
    let mut client = server.client();

    let err = client
        .checkouts()
        .expect_err("a missing `snapshot` object must not read as an idle session");

    assert!(
        err.to_string().contains("snapshot"),
        "the message must name what is missing: {err}"
    );
}

#[test]
fn connect_reports_the_socket_path_when_there_is_no_server() {
    let _guard = env_lock();
    std::env::set_var("HERDR_SOCKET_PATH", "/nonexistent/collide-test.sock");

    let err = Herdr::connect().expect_err("no server listening");

    assert!(
        err.to_string().contains("/nonexistent/collide-test.sock"),
        "the message must name the path: {err}"
    );
}

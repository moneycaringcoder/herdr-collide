//! Wire-level tests for the socket client.
//!
//! Every test stands up a real Unix socket server in a temp directory and
//! asserts the bytes the client puts on the wire, because the parts of this
//! protocol that bite (mandatory `{}` params, one request per connection, the
//! merge-patch clear with no TTL) are invisible from the Rust API alone.
//!
//! The fixtures below are shaped from `herdr api snapshot` captured against a
//! live 0.8.0 server and from the bundled schema (`herdr api schema --json`),
//! including the fields the client never reads: a reply that carries only what
//! the client reads cannot catch the client reading the wrong thing.
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

fn scratch_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "collide-wire-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
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
        let dir = scratch_dir("srv");
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
        point_at(&self.path);
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

fn point_at(path: &std::path::Path) {
    std::env::set_var("HERDR_SOCKET_PATH", path);
    std::env::set_var("HERDR_PLUGIN_ID", SOURCE);
}

/// A server that goes away and comes back, the way `herdr update --handoff`
/// does: the old server unlinks the socket, and a moment later a new one binds
/// the same path. Nothing is listening in between, so a dial made during the
/// window fails outright.
///
/// `TestServer`'s `Reply::Eof` is a *different* failure — there the socket
/// exists and the connection is accepted, then closed. Only this one exercises
/// the case the protocol notes actually describe.
struct HandoffServer {
    path: PathBuf,
    dir: PathBuf,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// How long the rebound listener stays up, waiting for connections that may
/// never come. It has to be bounded: `Drop` joins this thread while the test
/// holds `env_lock`, so a blocking `accept()` here would wedge every other test
/// in the file the moment this one failed. (It did, the first time.)
const HANDOFF_LIFETIME: Duration = Duration::from_millis(1_500);

impl HandoffServer {
    /// Unlinks `path`, waits `gap`, rebinds, then answers `replies` in order.
    fn start(dir: PathBuf, path: PathBuf, gap: Duration, replies: Vec<String>) -> Self {
        let _ = std::fs::remove_file(&path);
        let thread = {
            let path = path.clone();
            std::thread::spawn(move || {
                std::thread::sleep(gap);
                let listener = UnixListener::bind(&path).expect("rebind");
                listener.set_nonblocking(true).expect("nonblocking");
                let deadline = std::time::Instant::now() + HANDOFF_LIFETIME;
                let mut replies = replies.into_iter();
                while std::time::Instant::now() < deadline {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream.set_nonblocking(false).expect("blocking");
                            let mut line = String::new();
                            let mut reader = BufReader::new(&stream);
                            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                                continue;
                            }
                            if let Some(reply) = replies.next() {
                                let mut stream = &stream;
                                let _ = stream.write_all(reply.as_bytes());
                                let _ = stream.write_all(b"\n");
                                let _ = stream.flush();
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
            thread: Some(thread),
        }
    }
}

impl Drop for HandoffServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
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

/// A `session.snapshot` reply in the real envelope: the arrays live under
/// `snapshot`, one level below `result`, alongside a `type` discriminator.
/// Reading them off `result` yields zero workspaces and looks exactly like an
/// idle session, which is why every reply here mirrors the wire shape rather
/// than the shape the client would find convenient.
fn snapshot_reply(snapshot: Value) -> Reply {
    Reply::Line(snapshot_line(snapshot))
}

fn snapshot_line(snapshot: Value) -> String {
    json!({
        "id": "collide:1",
        "result": {"type": "session_snapshot", "snapshot": snapshot}
    })
    .to_string()
}

/// Structure copied from `herdr api snapshot` on a live 0.8.0 server, paths
/// redacted. Fields the client ignores are kept deliberately.
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
            // A readback of what plugins have set on this workspace. Real
            // workspaces carry it, and the protocol notes call it out as the
            // way to verify our own writes, so a fixture without it is not a
            // snapshot herdr could send.
            "tokens": {"git_dirty": "~55 ?45"},
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
    // server that has just answered behaves. The retry must land the call.
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

/// The case the protocol notes describe and the EOF test above does *not*
/// cover: the socket file is gone entirely for a moment, so the first dial
/// fails before any connection exists.
///
/// This is what fails without a pause before the retry. Measured back to back,
/// the two attempts were 0.05 ms apart — one attempt, as far as a rebind is
/// concerned.
#[test]
fn a_call_survives_the_socket_being_unlinked_and_rebound() {
    let _guard = env_lock();
    let dir = scratch_dir("handoff");
    let path = dir.join("s.sock");

    // Bind first so `connect` succeeds, the way a daemon that has been running
    // since before the handoff has an open client.
    let listener = UnixListener::bind(&path).expect("bind");
    point_at(&path);
    let mut client = Herdr::connect().expect("connect before the handoff");
    drop(listener);

    // The old server goes away; a new one binds the same path 60 ms later,
    // comfortably inside the client's retry pause and far outside a retry with
    // no pause at all.
    let server = HandoffServer::start(
        dir,
        path,
        Duration::from_millis(60),
        vec![snapshot_line(snapshot_with_one_repo())],
    );

    let checkouts = client
        .checkouts()
        .expect("the retry must land on the new server");
    assert_eq!(checkouts.len(), 1);
    drop(server);
}

/// `connect` is a call like any other — `--disable`'s sweep, `--once` and the
/// daemon's shutdown clear all go through it — so it retries too.
#[test]
fn connect_survives_the_socket_being_unlinked_and_rebound() {
    let _guard = env_lock();
    let dir = scratch_dir("handoff-connect");
    let path = dir.join("s.sock");
    point_at(&path);

    let server = HandoffServer::start(dir, path, Duration::from_millis(60), vec![]);

    Herdr::connect().expect("connect must retry across a rebind");
    drop(server);
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
        "focused_pane_id": "w2:p1",
        "focused_tab_id": "w2:t1",
        "focused_workspace_id": "w2",
        "layouts": [],
        "tabs": [{"tab_id": "w2:t1", "workspace_id": "w2", "focused": true}],
        "workspaces": [
            // A live server omits `worktree` entirely for a workspace that is
            // not a repo — verified against `herdr api snapshot`, where seven
            // of ten workspaces had no such key at all.
            {"workspace_id": "w1", "label": "notes", "number": 1, "agent_status": "idle",
             "focused": false, "pane_count": 1, "tab_count": 1, "active_tab_id": "w1:t1"},
            // The schema types it `anyOf [WorkspaceWorktreeInfo, null]`, so an
            // explicit null is legal even though the server does not send one.
            // It means the same thing: not a repo.
            {"workspace_id": "wn", "label": "null", "number": 4, "agent_status": "idle",
             "focused": false, "pane_count": 1, "tab_count": 1, "active_tab_id": "wn:t1",
             "worktree": null},
            {
                "workspace_id": "w2",
                "label": "main",
                "number": 2,
                "focused": true,
                "pane_count": 1,
                "tab_count": 1,
                "active_tab_id": "w2:t1",
                "agent_status": "idle",
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
                "focused": false,
                "pane_count": 1,
                "tab_count": 1,
                "active_tab_id": "w3:t1",
                "agent_status": "idle",
                "worktree": {
                    "repo_key": "/repo/.git",
                    "repo_name": "repo",
                    "repo_root": "/repo",
                    "checkout_path": "/wt/fix",
                    "is_linked_worktree": true
                }
            }
        ],
        "panes": [{"pane_id": "w3:p1", "workspace_id": "w3", "tab_id": "w3:t1",
                   "terminal_id": "term_1", "focused": false, "revision": 1,
                   "agent_status": "idle", "agent": "codex"}],
        "agents": [{
            "pane_id": "w2:p1",
            "tab_id": "w2:t1",
            "terminal_id": "term_0",
            "workspace_id": "w2",
            "focused": true,
            "revision": 1,
            "agent_status": "idle",
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
        "a workspace with no worktree key, or a null one, is not a repo"
    );
    assert_eq!(
        client.skipped_worktrees(),
        0,
        "neither shape is a worktree we failed to read"
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
    // Branches come from git, not from the snapshot and not from
    // `worktree.list` — this client never calls that method.
    assert_eq!(main.branch, None);

    let linked = &checkouts[1];
    assert!(linked.is_linked_worktree);
    assert_eq!(
        linked.agent.as_deref(),
        Some("codex"),
        "panes[] carries the agent when agents[] has no row"
    );
}

/// A workspace herdr says is a repo but whose worktree object we cannot address
/// is dropped — silently, before this, which made the session look smaller than
/// it is. The count is what turns that into a note the daemon can print.
#[test]
fn an_unreadable_worktree_object_is_counted_rather_than_swallowed() {
    let _guard = env_lock();
    let server = TestServer::start(vec![snapshot_reply(json!({
        "protocol": 19,
        "version": "0.8.0",
        "layouts": [],
        "tabs": [],
        "panes": [],
        "agents": [],
        "workspaces": [
            // No checkout_path: a repo we can see but cannot address.
            {"workspace_id": "w1", "label": "half", "number": 1, "agent_status": "idle",
             "focused": false, "pane_count": 1, "tab_count": 1, "active_tab_id": "w1:t1",
             "worktree": {"repo_key": "/repo/.git", "repo_name": "repo", "repo_root": "/repo",
                          "is_linked_worktree": false}},
            // Present but not an object at all.
            {"workspace_id": "w2", "label": "odd", "number": 2, "agent_status": "idle",
             "focused": false, "pane_count": 1, "tab_count": 1, "active_tab_id": "w2:t1",
             "worktree": "/repo"},
            {"workspace_id": "w3", "label": "fine", "number": 3, "agent_status": "idle",
             "focused": false, "pane_count": 1, "tab_count": 1, "active_tab_id": "w3:t1",
             "worktree": {"repo_key": "/repo/.git", "repo_name": "repo", "repo_root": "/repo",
                          "checkout_path": "/repo", "is_linked_worktree": false}}
        ]
    }))]);
    let mut client = server.client();

    let checkouts = client.checkouts().expect("snapshot");

    assert_eq!(checkouts.len(), 1);
    assert_eq!(
        client.skipped_worktrees(),
        2,
        "both unreadable worktree objects must be counted"
    );
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

/// The same argument one level down. `workspaces` is a required field of
/// `SessionSnapshot`, so its absence is a protocol break — and an absent array
/// read as an empty one is the identical invisible failure, just deeper.
#[test]
fn a_snapshot_without_a_workspaces_array_is_an_error_not_an_idle_session() {
    let _guard = env_lock();
    let server = TestServer::start(vec![snapshot_reply(json!({
        "protocol": 19,
        "version": "0.8.0",
        "layouts": [],
        "tabs": [],
        "panes": [],
        "agents": []
    }))]);
    let mut client = server.client();

    let err = client
        .checkouts()
        .expect_err("no workspaces array must not read as an idle session");

    assert!(
        err.to_string().contains("workspaces"),
        "the message must name what is missing: {err}"
    );
    // And the keys that did arrive, so the reader can see what changed.
    assert!(err.to_string().contains("agents"), "{err}");
}

#[test]
fn an_empty_workspaces_array_really_is_an_idle_session() {
    let _guard = env_lock();
    let server = TestServer::start(vec![snapshot_reply(json!({
        "protocol": 19,
        "version": "0.8.0",
        "layouts": [],
        "tabs": [],
        "panes": [],
        "agents": [],
        "workspaces": []
    }))]);
    let mut client = server.client();

    assert!(client
        .checkouts()
        .expect("an idle session is fine")
        .is_empty());
}

/// `server.reload_config` does not answer `{"type":"ok"}`. These three payloads
/// were captured from a live 0.8.0 server driven against a scratch config file.
#[test]
fn a_config_reload_is_only_a_success_when_it_was_applied() {
    let _guard = env_lock();
    let server = TestServer::start(vec![
        Reply::Line(
            json!({"id": "collide:1", "result":
                {"type": "config_reload", "status": "applied", "diagnostics": []}})
            .to_string(),
        ),
        Reply::Line(
            json!({"id": "collide:2", "result": {
                "type": "config_reload",
                "status": "failed",
                "diagnostics": ["config parse error: TOML parse error at line 1, column 19; keeping current config"]
            }})
            .to_string(),
        ),
        Reply::Line(
            json!({"id": "collide:3", "result": {
                "type": "config_reload",
                "status": "partial",
                "diagnostics": ["invalid theme config: invalid type: integer `42`, expected a string"]
            }})
            .to_string(),
        ),
    ]);
    let mut client = server.client();

    client.reload_config().expect("applied is a success");

    let failed = client.reload_config().expect_err("failed is not a success");
    assert!(failed.to_string().contains("failed"), "{failed}");
    assert!(
        failed.to_string().contains("TOML parse error"),
        "the diagnostics must reach the user: {failed}"
    );

    let partial = client
        .reload_config()
        .expect_err("partial is not a success either");
    assert!(partial.to_string().contains("partial"), "{partial}");
    assert!(
        partial.to_string().contains("invalid theme config"),
        "{partial}"
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(parse_framed(&requests[0])["method"], "server.reload_config");
    assert_eq!(parse_framed(&requests[0])["params"], json!({}));
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

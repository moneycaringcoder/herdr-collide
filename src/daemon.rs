//! Badge updater lifecycle: detached daemon, pid/enabled markers, TTL badge
//! pushes, and cleanup that survives being killed. See docs/herdr-protocol.md
//! for the lifecycle contract these verbs implement.

use std::collections::HashMap;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{self, Config};
use crate::herdr::{self, Herdr};
use crate::model::Severity;
use crate::Result;

/// The stop request only posts a signal; the daemon still has to clear its
/// badges. Bounded so `--disable` can never hang on a wedged daemon.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
const STOP_POLL: Duration = Duration::from_millis(25);

/// The main loop wakes at least this often so a stop request is noticed
/// promptly even with a long refresh interval.
const LOOP_TICK: Duration = Duration::from_millis(250);

/// Arguments the detached child is given a copy of. It re-reads the config file
/// but never sees the user's command line, so `collide --enable --interval 30`
/// would otherwise run at the config file's interval.
const FORWARDED: [&str; 2] = ["--interval", "--base-ref"];

pub fn enable(args: &[String]) -> Result<()> {
    // Parse before touching any state: a typo'd value must fail here, where the
    // user can see it, and not inside a detached child whose stderr is
    // /dev/null.
    let forwarded = forwarded_args(args)?;
    config::load_with_args(args)?;

    // Mark next. If the spawn fails, or the server hands off before we finish,
    // `--restore` still knows the user wants a daemon.
    mark_enabled(true);
    if live_pid().is_some() {
        return Ok(());
    }
    spawn_detached(&forwarded)
}

pub fn disable() -> Result<()> {
    // Mark first, so nothing that observes the markers mid-teardown concludes
    // the daemon is still wanted.
    mark_enabled(false);

    if let Some(pid) = live_pid() {
        request_stop(pid);
        // Load-bearing: the stop request only posts, and the pid file lives
        // until the daemon has finished clearing. An `--enable` landing in that
        // window would see a live pid, spawn nothing, and the badge would never
        // come back.
        if !await_exit(pid, STOP_TIMEOUT) {
            eprintln!("collide: updater {pid} did not exit within {STOP_TIMEOUT:?}");
        }
    }
    clear_pid_file();

    // Fresh connection, and every current workspace: the daemon may have died
    // without clearing, and it only ever tracked the workspaces it had seen.
    let mut client = Herdr::connect()?;
    sweep(&mut client)
}

pub fn toggle(args: &[String]) -> Result<()> {
    if live_pid().is_some() {
        disable()
    } else {
        enable(args)
    }
}

/// herdr startup hook. Silent no-op unless the enabled marker is set and no
/// daemon is currently live.
pub fn restore() -> Result<()> {
    if !is_enabled() || live_pid().is_some() {
        return Ok(());
    }
    // A startup hook has no user command line to forward; the child falls back
    // to the config file, which is the only durable record of the user's
    // choices anyway.
    spawn_detached(&[])
}

/// The refresh loop itself, running in the foreground.
pub fn run(config: &Config) -> Result<()> {
    write_pid(std::process::id());

    // Which token name is currently lit per workspace. A severity flip has to
    // clear the old name before setting the new one, or herdr renders two
    // badges at once — the merge patch only touches names we mention.
    let active: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let stopping = Arc::new(AtomicBool::new(false));
    spawn_signal_thread(Arc::clone(&active), Arc::clone(&stopping))?;

    // A daemon that is SIGKILLed never runs `Predictor::drop`, and unlike a
    // one-shot run it gets killed often enough for the leftovers to add up.
    crate::git::sweep_scratch();

    // gather_for derives its own integration ref (collide.rs calls
    // git::integration_ref per checkout), so a configured base_ref does not
    // reach git yet. Say so once rather than letting the setting look effective.
    if config.base_ref != config::DEFAULT_BASE_REF {
        eprintln!(
            "collide: base_ref `{}` is not honoured yet — the analysis pass derives its own \
             integration ref",
            config.base_ref
        );
    }

    let mut client: Option<Herdr> = None;
    // Notes repeat every cycle for as long as their cause lasts, so only the
    // ones that are new since the last cycle are worth printing.
    let mut reported_notes: Vec<String> = Vec::new();
    loop {
        if stopping.load(Ordering::SeqCst) {
            // The signal thread owns shutdown from here: it clears state over
            // its own connection and exits the process. Park rather than
            // return, so this thread can never push a badge back on top of the
            // clear it is racing.
            loop {
                std::thread::park();
            }
        }

        if client.is_none() {
            match Herdr::connect() {
                Ok(connected) => client = Some(connected),
                Err(err) => eprintln!("collide: cannot reach herdr: {err}"),
            }
        }
        if let Some(connected) = client.as_mut() {
            if let Err(err) = refresh(connected, config, &active, &mut reported_notes) {
                eprintln!("collide: refresh failed: {err}");
                // Only a transport failure is worth redialling for; an error
                // envelope means the server is fine and answered us.
                if herdr::error_code(&*err).is_none() {
                    client = None;
                }
            }
        }

        nap(config.interval, &stopping);
    }
}

/// One cycle: snapshot, gather, push.
///
/// The gathering is `collide::gather_for` rather than a hand-rolled
/// `change_set` + `analyse` pass. `analyse` alone deliberately leaves every
/// shared file `Unknown` when `predict_conflicts` is set and expects the caller
/// to run predictions and fold them back through `apply_predictions`; a caller
/// that skips that step can escalate a badge to overlap but never to conflict,
/// which is the entire headline feature. `gather_for` also re-derives repo
/// identity from git, so two checkouts are only compared when their
/// `--git-common-dir` really matches.
///
/// `gather_for` rather than `gather` because `gather` opens its own socket
/// connection: this way the daemon keeps one client, its retry, and its ability
/// to tell a transport failure from an error envelope.
fn refresh(
    client: &mut Herdr,
    config: &Config,
    active: &Mutex<HashMap<String, String>>,
    reported_notes: &mut Vec<String>,
) -> Result<()> {
    let checkouts = client.checkouts()?;
    let cycle = crate::collide::gather_for(checkouts, config)?;

    for note in new_notes(reported_notes, &cycle.notes) {
        eprintln!("collide: {note}");
    }
    reported_notes.clone_from(&cycle.notes);

    push(client, config, &cycle.report.statuses, active);
    Ok(())
}

/// Notes that were not already reported on the previous cycle. A note repeats
/// for as long as its cause lasts — a workspace that is not a repo produces one
/// every 5 seconds — so only the new ones are worth printing.
pub fn new_notes(previous: &[String], current: &[String]) -> Vec<String> {
    current
        .iter()
        .filter(|note| !previous.contains(note))
        .cloned()
        .collect()
}

/// One badge call to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BadgeOp {
    Clear {
        workspace_id: String,
        token: String,
    },
    Set {
        workspace_id: String,
        token: &'static str,
        text: String,
    },
}

/// Turns "what is lit now" plus "what this cycle found" into the calls that
/// close the gap. Pure, so the ordering rules below are testable without a
/// socket:
///
/// * A severity flip clears the old token name *before* setting the new one.
///   Tokens are a merge patch, so an unmentioned name stays lit and herdr would
///   render two badges for one workspace.
/// * `render::badge` is the single author of badge text, and it renders a clean
///   workspace as the empty string. An empty value is a clear, never a draw:
///   setting it would occupy the row with nothing.
/// * A workspace that dropped out of the report — closed, or no longer a repo —
///   is cleared rather than left to expire.
pub fn badge_plan(
    active: &HashMap<String, String>,
    statuses: &[crate::model::WorkspaceStatus],
) -> Vec<BadgeOp> {
    let mut ops = Vec::new();
    let mut wanted: HashMap<&str, &'static str> = HashMap::new();

    for status in statuses {
        let text = crate::render::badge(status);
        let token = status.severity.token_name();
        let previous = active.get(&status.workspace_id).map(String::as_str);
        let next = if text.is_empty() { None } else { Some(token) };

        if let Some(previous) = previous {
            if Some(previous) != next {
                ops.push(BadgeOp::Clear {
                    workspace_id: status.workspace_id.clone(),
                    token: previous.to_string(),
                });
            }
        }
        if let Some(token) = next {
            wanted.insert(status.workspace_id.as_str(), token);
            ops.push(BadgeOp::Set {
                workspace_id: status.workspace_id.clone(),
                token,
                // Re-sent every cycle even when unchanged: the TTL is what makes
                // the badge self-heal, and it only refreshes on a write.
                text,
            });
        }
    }

    let mut stale: Vec<(&String, &String)> = active
        .iter()
        .filter(|(workspace_id, _)| !wanted.contains_key(workspace_id.as_str()))
        .filter(|(workspace_id, _)| {
            // Already cleared above by the severity-flip branch.
            !statuses.iter().any(|s| &s.workspace_id == *workspace_id)
        })
        .collect();
    // A HashMap iterates in an arbitrary order; sorting keeps the plan
    // reproducible for both tests and logs.
    stale.sort();
    for (workspace_id, token) in stale {
        ops.push(BadgeOp::Clear {
            workspace_id: workspace_id.clone(),
            token: token.clone(),
        });
    }

    ops
}

/// Executes a badge plan. Errors are reported per call and the cycle continues:
/// a swallowed push failure renders as a blank badge with nothing to debug, and
/// one bad workspace must not cost every other one its badge.
fn push(
    client: &mut Herdr,
    config: &Config,
    statuses: &[crate::model::WorkspaceStatus],
    active: &Mutex<HashMap<String, String>>,
) {
    let ttl_ms = config.ttl_ms();
    let plan = badge_plan(&lock(active).clone(), statuses);
    let mut lit: HashMap<String, String> = HashMap::new();

    for op in plan {
        match op {
            BadgeOp::Clear {
                workspace_id,
                token,
            } => {
                // A failed clear is forgotten rather than retried next cycle:
                // the TTL expires it within three cycles anyway, and retrying
                // forever would hammer a workspace that no longer exists.
                report_error(
                    client.clear_badge(&workspace_id, &token),
                    &workspace_id,
                    &token,
                    "clear",
                );
            }
            BadgeOp::Set {
                workspace_id,
                token,
                text,
            } => {
                if report_error(
                    client.set_badge(&workspace_id, token, &text, ttl_ms),
                    &workspace_id,
                    token,
                    "set",
                ) {
                    lit.insert(workspace_id, token.to_string());
                }
            }
        }
    }

    *lock(active) = lit;
}

/// Logs a failed push. Returns whether the call succeeded. A workspace that
/// closed under us is expected churn, not something to shout about.
fn report_error(result: Result<()>, workspace_id: &str, token: &str, what: &str) -> bool {
    match result {
        Ok(()) => true,
        Err(err) => {
            if herdr::error_code(&*err) != Some("workspace_not_found") {
                eprintln!("collide: {what} {token} on {workspace_id} failed: {err}");
            }
            false
        }
    }
}

/// Clears every token this plugin owns on every current workspace.
fn sweep(client: &mut Herdr) -> Result<()> {
    let checkouts = client.checkouts()?;
    let mut failures = 0usize;
    for checkout in &checkouts {
        for token in Severity::ALL_TOKENS {
            if !report_error(
                client.clear_badge(&checkout.workspace_id, token),
                &checkout.workspace_id,
                token,
                "clear",
            ) {
                failures += 1;
            }
        }
    }
    if failures > 0 {
        return Err(format!("{failures} badge clears failed; see the messages above").into());
    }
    Ok(())
}

fn spawn_signal_thread(
    active: Arc<Mutex<HashMap<String, String>>>,
    stopping: Arc<AtomicBool>,
) -> Result<()> {
    let mut signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
    ])?;
    std::thread::spawn(move || {
        if signals.forever().next().is_some() {
            stopping.store(true, Ordering::SeqCst);
            shutdown(&active);
            std::process::exit(0);
        }
    });
    Ok(())
}

/// Clears everything this daemon lit, over its **own** connection so it never
/// waits on the main loop's sleep or its in-flight round trip.
fn shutdown(active: &Mutex<HashMap<String, String>>) {
    let tracked: Vec<(String, String)> = lock(active)
        .iter()
        .map(|(workspace_id, token)| (workspace_id.clone(), token.clone()))
        .collect();
    match Herdr::connect() {
        Ok(mut client) => {
            for (workspace_id, token) in tracked {
                report_error(
                    client.clear_badge(&workspace_id, &token),
                    &workspace_id,
                    &token,
                    "clear",
                );
            }
        }
        // Not silent: without this line a killed daemon looks like it cleaned
        // up, and the badge lingers until its TTL expires.
        Err(err) => eprintln!("collide: shutdown could not reach herdr: {err}"),
    }
    clear_pid_file();
}

/// Sleeps in slices so a stop request is noticed without waiting out a whole
/// refresh interval.
fn nap(interval: Duration, stopping: &AtomicBool) {
    let deadline = Instant::now() + interval;
    while Instant::now() < deadline {
        if stopping.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(LOOP_TICK.min(deadline.saturating_duration_since(Instant::now())));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A panicking push must not take the badge state down with it; the data is
    // a plain map and stays consistent.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Process control
// ---------------------------------------------------------------------------

/// The arguments worth handing to the detached child, normalised to the
/// `--name value` spelling. Anything else on the command line (the verb itself,
/// flags the daemon does not read) is dropped.
pub fn forwarded_args(args: &[String]) -> Result<Vec<String>> {
    let mut forwarded = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let Some(name) = FORWARDED.into_iter().find(|name| {
            arg == name
                || arg
                    .strip_prefix(*name)
                    .is_some_and(|tail| tail.starts_with('='))
        }) else {
            continue;
        };
        let value = match arg.split_once('=') {
            Some((_, value)) => value.to_string(),
            None => rest.next().ok_or(format!("{name} needs a value"))?.clone(),
        };
        forwarded.push(name.to_string());
        forwarded.push(value);
    }
    Ok(forwarded)
}

fn spawn_detached(forwarded: &[String]) -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command
        .arg("--daemon")
        .args(forwarded)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // A daemon herdr spawned as a child dies with herdr. `setsid` puts it in
    // its own session so it survives; a double fork is not needed, and the
    // extra process would only make the pid we record harder to track.
    unsafe {
        command.pre_exec(|| {
            // EPERM here just means we are already a session leader.
            libc::setsid();
            Ok(())
        });
    }
    let child = command.spawn()?;
    write_pid(child.id());
    Ok(())
}

fn request_stop(pid: i32) {
    // SIGTERM, not SIGKILL: the daemon's handler is what clears the badges.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
}

fn await_exit(pid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !is_alive(pid) {
            return true;
        }
        std::thread::sleep(STOP_POLL);
    }
    !is_alive(pid)
}

fn is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // Signal 0 checks for existence without delivering anything. EPERM means
    // the process exists but belongs to someone else.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Guards against pid reuse. The state dir outlives reboots, so a recorded pid
/// can easily belong to something else entirely by the time we read it.
#[cfg(target_os = "linux")]
fn same_program(pid: i32) -> bool {
    let ours = fs::read_to_string("/proc/self/comm");
    let theirs = fs::read_to_string(format!("/proc/{pid}/comm"));
    match (ours, theirs) {
        (Ok(ours), Ok(theirs)) => ours.trim() == theirs.trim(),
        // /proc unreadable (hidepid, a stripped container): fall back to
        // trusting the liveness probe rather than killing a live daemon's
        // marker.
        _ => true,
    }
}

#[cfg(not(target_os = "linux"))]
fn same_program(_pid: i32) -> bool {
    // No portable equivalent of /proc/<pid>/comm; liveness is all we have.
    true
}

// ---------------------------------------------------------------------------
// Markers
// ---------------------------------------------------------------------------

/// The pid of a daemon that is live *right now*, or `None`. A stale or reused
/// pid file is swept as a side effect so the next verb starts from a clean
/// state.
pub fn live_pid() -> Option<i32> {
    let recorded = read_pid()?;
    if is_alive(recorded) && same_program(recorded) {
        return Some(recorded);
    }
    clear_pid_file();
    None
}

pub fn read_pid() -> Option<i32> {
    fs::read_to_string(config::pid_file())
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|pid| *pid > 0)
}

pub fn write_pid(pid: u32) {
    // Best effort: an unwritable state dir must not fail the user's action,
    // but it must not be silent either — without the marker, `--enable` will
    // happily start a second daemon.
    let path = config::pid_file();
    if let Err(err) = write_marker(&path, &pid.to_string()) {
        eprintln!("collide: could not record pid in {}: {err}", path.display());
    }
}

/// Removes the pid file, but only if it still names this process or a dead one,
/// so a successor daemon's marker is never deleted.
pub fn clear_pid_file() {
    match read_pid() {
        Some(pid) if pid != std::process::id() as i32 && is_alive(pid) && same_program(pid) => {}
        _ => {
            let _ = fs::remove_file(config::pid_file());
        }
    }
}

/// Did the user ever ask for a daemon? Consulted by `--restore`.
pub fn is_enabled() -> bool {
    config::enabled_flag().exists()
}

pub fn mark_enabled(enabled: bool) {
    let path = config::enabled_flag();
    let outcome = if enabled {
        write_marker(&path, "1")
    } else {
        match fs::remove_file(&path) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    };
    if let Err(err) = outcome {
        eprintln!("collide: could not update {}: {err}", path.display());
    }
}

fn write_marker(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)
}

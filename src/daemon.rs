//! Badge updater lifecycle: detached daemon, pid/enabled markers, TTL badge
//! pushes, and cleanup that survives being killed. See docs/herdr-protocol.md
//! for the lifecycle contract these verbs implement.

use std::collections::HashMap;
use std::fs;
use std::os::unix::io::AsRawFd;
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
///
/// Raised from 3s because 3s was measured to expire on a loaded machine while a
/// perfectly healthy daemon was still on its way out — and the old code then
/// left the pid file in place, so the next `--enable` saw a live pid and did
/// nothing. The bound is now a step on the way to `SIGKILL` rather than a
/// verdict, so being generous with it costs only patience.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// After `SIGKILL` there is nothing left to wait for but the kernel.
const KILL_TIMEOUT: Duration = Duration::from_secs(2);
const STOP_POLL: Duration = Duration::from_millis(25);

/// Cap on `updater.log`. The daemon truncates its own stderr when it grows past
/// this, so a daemon that fails every cycle for a week cannot fill the disk.
const MAX_LOG_BYTES: u64 = 1 << 20;

/// The main loop wakes at least this often so a stop request is noticed
/// promptly even with a long refresh interval.
const LOOP_TICK: Duration = Duration::from_millis(250);

/// Arguments the detached child is given a copy of. It re-reads the config file
/// but never sees the user's command line, so `collide --enable --interval 30`
/// would otherwise run at the config file's interval.
const FORWARDED: [&str; 2] = ["--interval", "--base-ref"];

pub fn enable(args: &[String]) -> Result<()> {
    // Parse before touching any state: a typo'd value must fail here, where the
    // user can see it, and not inside a detached child whose output goes to a
    // log nobody has been told about yet.
    let forwarded = forwarded_args(args)?;
    config::load_with_args(args)?;

    // Held across the check-and-spawn. Without it, two `--enable` invocations —
    // a keypress and a `--restore` startup hook during a handoff is enough —
    // both see no live pid and both spawn, and only one of the two daemons ends
    // up in the pid file. The other can never be stopped.
    //
    // Taking the lock also proves the state dir is writable, which is the
    // condition that used to let `--enable` spawn an unstoppable daemon on
    // every invocation.
    let _lock = SpawnLock::acquire()?;

    // Mark next. If the spawn fails, or the server hands off before we finish,
    // `--restore` still knows the user wants a daemon.
    mark_enabled(true);
    if live_pid().is_some() {
        return Ok(());
    }
    spawn_detached(&forwarded)
}

pub fn disable() -> Result<()> {
    // The same lock `--enable` takes, so an `--enable` cannot land in the
    // middle of the teardown, see the doomed daemon's pid, and decline to
    // spawn. Released before the sweep, which is slow and does not need it.
    let stopped = {
        let _lock = SpawnLock::acquire()?;

        // Mark first, so nothing that observes the markers mid-teardown
        // concludes the daemon is still wanted.
        mark_enabled(false);
        stop_daemon()
    };
    stopped?;

    // Fresh connection, and every current workspace: the daemon may have died
    // without clearing, and it only ever tracked the workspaces it had seen.
    let mut client = Herdr::connect()?;
    sweep(&mut client)
}

/// Stops a running daemon and clears its marker, escalating to `SIGKILL`.
///
/// The escalation matters: a daemon that ignored `SIGTERM` has already failed
/// to clear its own badges, so there is nothing left to be polite about — and
/// the sweep that follows clears every token on every workspace anyway. What
/// must not happen is the old behaviour, where a surviving daemon left its pid
/// file in place (`clear_pid_file` refuses to delete a live daemon's marker)
/// and the next `--enable` quietly did nothing.
fn stop_daemon() -> Result<()> {
    let Some(pid) = live_pid() else {
        clear_pid_file();
        return Ok(());
    };

    request_stop(pid);
    if !await_exit(pid, STOP_TIMEOUT) {
        eprintln!("collide: updater {pid} ignored SIGTERM for {STOP_TIMEOUT:?}; sending SIGKILL");
        force_stop(pid);
        if !await_exit(pid, KILL_TIMEOUT) {
            return Err(format!(
                "updater {pid} survived SIGKILL; stop it by hand before enabling again"
            )
            .into());
        }
    }
    clear_pid_file();
    Ok(())
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
    if !is_enabled() {
        return Ok(());
    }
    // Startup hooks run on a live handoff too, so this can fire at the same
    // moment a user presses the enable action. Same lock, same reason.
    let _lock = SpawnLock::acquire()?;
    if live_pid().is_some() {
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

        cap_log();
        nap(config.interval, &stopping);
    }
}

/// Truncates our own stderr when it grows past the cap.
///
/// Done on the descriptor rather than on the path, so it works regardless of
/// which file `--enable` handed us, and does nothing at all when stderr is a
/// terminal or a pipe — a foreground `--daemon` must keep printing to whatever
/// the user pointed it at.
fn cap_log() {
    let fd = std::io::stderr().as_raw_fd();
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd, &mut stat) } != 0 {
        return;
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        return;
    }
    if u64::try_from(stat.st_size).unwrap_or(0) <= MAX_LOG_BYTES {
        return;
    }
    unsafe {
        libc::ftruncate(fd, 0);
        libc::lseek(fd, 0, libc::SEEK_SET);
    }
    eprintln!("collide: updater log passed {MAX_LOG_BYTES} bytes and was truncated");
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
    let skipped = client.skipped_worktrees();
    let cycle = crate::collide::gather_for(checkouts, config)?;

    // A workspace herdr says is a repo but whose worktree object this client
    // could not read is dropped silently, which makes the session look smaller
    // than it is. Folded in with the analysis notes so it repeats no more often
    // than they do.
    let mut notes = cycle.notes.clone();
    if skipped > 0 {
        notes.push(format!(
            "{skipped} workspace(s) carried a worktree object this client could not read \
             (no repo_key or checkout_path); they are missing from the report"
        ));
    }

    for note in new_notes(reported_notes, &notes) {
        eprintln!("collide: {note}");
    }
    reported_notes.clone_from(&notes);

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
    let previous = lock(active).clone();
    let plan = badge_plan(&previous, statuses);

    let mut results = Vec::with_capacity(plan.len());
    for op in plan {
        let outcome = match &op {
            BadgeOp::Clear {
                workspace_id,
                token,
            } => report_error(
                client.clear_badge(workspace_id, token),
                workspace_id,
                token,
                "clear",
            ),
            BadgeOp::Set {
                workspace_id,
                token,
                text,
            } => report_error(
                client.set_badge(workspace_id, token, text, ttl_ms),
                workspace_id,
                token,
                "set",
            ),
        };
        results.push((op, outcome));
    }

    *lock(active) = next_active(&previous, &results);
}

/// What is lit after a cycle's calls have been made.
///
/// Pure, because the rule it encodes is the one that was wrong. The old code
/// rebuilt this map from the successful sets alone, so a set that failed erased
/// the record of a token herdr was still rendering under its TTL — and the next
/// severity flip then emitted no clear for it, lighting two collide tokens on
/// one workspace, which is exactly what the one-token-per-workspace design
/// exists to prevent.
///
/// The rule now: an entry leaves the map only when herdr confirms a clear, when
/// the workspace has gone away and taken the badge with it, or when a
/// successful set replaces it. A call that merely failed changes nothing, so
/// the next cycle still knows what is lit.
pub fn next_active(
    previous: &HashMap<String, String>,
    results: &[(BadgeOp, PushOutcome)],
) -> HashMap<String, String> {
    let mut lit = previous.clone();
    for (op, outcome) in results {
        match op {
            BadgeOp::Clear {
                workspace_id,
                token,
            } => {
                if *outcome != PushOutcome::Failed
                    && lit
                        .get(workspace_id)
                        .is_some_and(|current| current == token)
                {
                    lit.remove(workspace_id);
                }
            }
            BadgeOp::Set {
                workspace_id,
                token,
                ..
            } => match outcome {
                PushOutcome::Done => {
                    lit.insert(workspace_id.clone(), (*token).to_string());
                }
                // Nothing is lit on a workspace that no longer exists.
                PushOutcome::Gone => {
                    lit.remove(workspace_id);
                }
                // Leave whatever was already lit in place: herdr is still
                // rendering it.
                PushOutcome::Failed => {}
            },
        }
    }
    lit
}

/// What one badge call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// herdr accepted it.
    Done,
    /// The workspace is gone, so whatever it was rendering is gone with it.
    Gone,
    /// It failed and the token may well still be lit.
    Failed,
}

/// Logs a failed push. A workspace that closed under us is expected churn, not
/// something to shout about — but it is still distinguished from a real
/// failure, because "the badge is gone" and "the badge may still be lit" call
/// for different bookkeeping.
fn report_error(result: Result<()>, workspace_id: &str, token: &str, what: &str) -> PushOutcome {
    match result {
        Ok(()) => PushOutcome::Done,
        Err(err) => {
            if herdr::error_code(&*err) == Some("workspace_not_found") {
                return PushOutcome::Gone;
            }
            eprintln!("collide: {what} {token} on {workspace_id} failed: {err}");
            PushOutcome::Failed
        }
    }
}

/// Clears every token this plugin owns on every current workspace.
fn sweep(client: &mut Herdr) -> Result<()> {
    let checkouts = client.checkouts()?;
    let mut failures = 0usize;
    for checkout in &checkouts {
        for token in Severity::ALL_TOKENS {
            if report_error(
                client.clear_badge(&checkout.workspace_id, token),
                &checkout.workspace_id,
                token,
                "clear",
            ) == PushOutcome::Failed
            {
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
                let _ = report_error(
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

/// An exclusive `flock` on `updater.lock`, held for as long as the value lives.
///
/// Creating it is also the writability check for the state dir: if the lock
/// cannot be created, `--enable` refuses rather than spawning a daemon whose pid
/// it will not be able to record. A daemon nobody can stop is worse than no
/// daemon, and a permission problem is something the user can fix once told.
struct SpawnLock {
    _file: fs::File,
}

impl SpawnLock {
    fn acquire() -> Result<Self> {
        let path = config::lock_file();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                format!(
                    "cannot create the plugin state directory {}: {err}",
                    parent.display()
                )
            })?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|err| {
                format!(
                    "cannot write the updater lock at {}: {err}. \
                     Refusing to start an updater whose pid could not be recorded — \
                     it could never be stopped. Fix the permissions on {} and try again.",
                    path.display(),
                    path.parent().unwrap_or(&path).display()
                )
            })?;
        // Blocking: `--disable` holds this only across the stop, so the wait is
        // bounded by STOP_TIMEOUT + KILL_TIMEOUT.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!(
                "cannot lock {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )
            .into());
        }
        Ok(Self { _file: file })
    }
}

impl Drop for SpawnLock {
    fn drop(&mut self) {
        // Closing the descriptor releases the lock; being explicit costs
        // nothing and documents the intent.
        unsafe {
            libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// The daemon's stdout and stderr, truncated for this run.
///
/// Without this the child inherits `/dev/null` and every diagnostic it writes
/// is lost — herdr only logs commands *it* spawned, and this process re-execs
/// itself, so there is no other record anywhere.
fn open_log() -> Result<fs::File> {
    let path = config::log_file();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .map_err(|err| format!("cannot write the updater log at {}: {err}", path.display()).into())
}

fn spawn_detached(forwarded: &[String]) -> Result<()> {
    let exe = std::env::current_exe()?;
    let log = open_log()?;
    let mut command = Command::new(exe);
    command
        .arg("--daemon")
        .args(forwarded)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
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
    let mut child = command.spawn()?;
    // A daemon we cannot record is a daemon we cannot stop, so it does not get
    // to live. This is the last place the state dir can turn out to be
    // unwritable — `SpawnLock` already proved it once.
    if let Err(err) = write_marker(&config::pid_file(), &child.id().to_string()) {
        let pid = child.id();
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        let _ = child.wait();
        return Err(format!(
            "could not record the updater pid in {}: {err}. \
             The updater was stopped again rather than left running unstoppable.",
            config::pid_file().display()
        )
        .into());
    }
    Ok(())
}

fn request_stop(pid: i32) {
    // SIGTERM, not SIGKILL: the daemon's handler is what clears the badges.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
}

fn force_stop(pid: i32) {
    unsafe {
        libc::kill(pid, libc::SIGKILL);
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

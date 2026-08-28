//! Crossterm runtime for the interactive collision pane.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::collide::InteractiveGather;
use crate::config::Config;
use crate::Result;

use super::state::{self, Detail, Key, Mode};
use super::view;

type DetailTerminal = Terminal<CrosstermBackend<io::Stdout>>;

pub fn run_watch(config: &Config) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    register_stop_signals(&stop)?;

    let guard = terminal::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let result = event_loop(config, &stop, &mut terminal);
    drop(terminal);
    drop(guard);
    result
}

fn event_loop(config: &Config, stop: &AtomicBool, terminal: &mut DetailTerminal) -> Result<()> {
    let mut detail = Detail::empty();
    let mut gathered: Option<InteractiveGather> = None;
    let mut mouse = view::MouseMap::default();
    let mut refresh_at = Instant::now();
    let mut dirty = true;

    loop {
        if stop.load(Ordering::Relaxed) || detail.is_finished() {
            return Ok(());
        }

        if detail.refresh_requested || Instant::now() >= refresh_at {
            detail = refresh(detail, config, &mut gathered);
            refresh_at = Instant::now() + config.interval;
            dirty = true;
        }

        if let Mode::OpeningHunks { path } = detail.mode.clone() {
            detail = match gathered.as_ref() {
                Some(gathered) => match gathered.explain(&path) {
                    Ok(why) => state::show_hunks(detail, path, why.text, why.prediction_failed),
                    Err(err) => state::show_hunks(
                        detail,
                        path.clone(),
                        format!("unknown: `{path}` could not be explained: {err}\n"),
                        true,
                    ),
                },
                None => state::show_hunks(
                    detail,
                    path.clone(),
                    format!("unknown: `{path}` cannot be explained until a refresh succeeds\n"),
                    true,
                ),
            };
            dirty = true;
        }

        if dirty {
            terminal.draw(|frame| {
                mouse = view::render(
                    frame,
                    &detail,
                    gathered.as_ref().map(InteractiveGather::cycle),
                    config.interval,
                );
            })?;
            dirty = false;
        }

        let Some(event) = next_event()? else {
            continue;
        };
        match event {
            Event::Key(event) => {
                if let Some(key) = map_key_event(event) {
                    detail = state::apply(detail, key);
                    dirty = true;
                }
            }
            Event::Mouse(event) => match event.kind {
                MouseEventKind::Down(MouseButton::Left) if detail.mode == Mode::List => {
                    if let Some(focus) = mouse.focus_at(event.column, event.row) {
                        detail.cursor = focus;
                        dirty = true;
                    }
                }
                MouseEventKind::ScrollUp => {
                    detail = state::apply(detail, Key::Up);
                    dirty = true;
                }
                MouseEventKind::ScrollDown => {
                    detail = state::apply(detail, Key::Down);
                    dirty = true;
                }
                _ => {}
            },
            Event::Resize(_, _) => dirty = true,
            Event::FocusGained | Event::FocusLost | Event::Paste(_) => {}
        }
    }
}

fn refresh(
    mut detail: Detail,
    config: &Config,
    gathered: &mut Option<InteractiveGather>,
) -> Detail {
    match crate::collide::gather_interactive(config) {
        Ok(next) => {
            detail = state::adopt(detail, &next.cycle().report);
            if let Some(path) = detail.open_hunk_path().map(str::to_string) {
                match next.explain(&path) {
                    Ok(why) => {
                        detail = state::show_hunks(detail, path, why.text, why.prediction_failed);
                    }
                    Err(err) => {
                        detail = state::show_hunks(
                            detail,
                            path.clone(),
                            format!("unknown: `{path}` could not be explained: {err}\n"),
                            true,
                        );
                    }
                }
            }
            *gathered = Some(next);
            detail
        }
        Err(err) => state::refresh_failed(detail, err.to_string()),
    }
}

fn next_event() -> Result<Option<Event>> {
    match event::poll(Duration::from_millis(50)) {
        Ok(false) => Ok(None),
        Ok(true) => match event::read() {
            Ok(event) => Ok(Some(event)),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(None),
            Err(err) => Err(err.into()),
        },
        Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(None),
        Err(err) => Err(err.into()),
    }
}

pub fn map_key_event(event: KeyEvent) -> Option<Key> {
    if !matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    Some(match event.code {
        KeyCode::Up | KeyCode::Char('k') => Key::Up,
        KeyCode::Down | KeyCode::Char('j') => Key::Down,
        KeyCode::Enter => Key::Enter,
        KeyCode::Char('R') => Key::Rescan,
        KeyCode::Char('q') | KeyCode::Esc => Key::Back,
        KeyCode::Char('c') if event.modifiers.contains(KeyModifiers::CONTROL) => Key::Quit,
        _ => Key::Other,
    })
}

#[cfg(unix)]
fn register_stop_signals(stop: &Arc<AtomicBool>) -> Result<()> {
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, Arc::clone(stop))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn register_stop_signals(_stop: &Arc<AtomicBool>) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
mod terminal {
    use std::io::{self, IsTerminal};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Once;

    use crossterm::cursor::{Hide, Show};
    use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
    use crossterm::execute;
    use crossterm::terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    };

    static ACTIVE: AtomicBool = AtomicBool::new(false);
    static HOOKS: Once = Once::new();

    pub struct Guard(());

    impl Drop for Guard {
        fn drop(&mut self) {
            restore();
        }
    }

    pub fn enter() -> crate::Result<Guard> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(
                "the live detail pane needs a terminal on stdin and stdout; use --once or --json when there is not one"
                    .into(),
            );
        }

        enable_raw_mode()?;
        ACTIVE.store(true, Ordering::SeqCst);
        install_hooks();
        if let Err(err) = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, Hide) {
            restore();
            return Err(err.into());
        }
        Ok(Guard(()))
    }

    pub fn restore() {
        if !ACTIVE.swap(false, Ordering::SeqCst) {
            return;
        }
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            LeaveAlternateScreen,
            Show
        );
    }

    fn install_hooks() {
        HOOKS.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                restore();
                previous(info);
            }));

            for signal in [
                signal_hook::consts::SIGINT,
                signal_hook::consts::SIGTERM,
                signal_hook::consts::SIGHUP,
            ] {
                let _ = unsafe { signal_hook::low_level::register(signal, restore) };
            }
        });
    }
}

#[cfg(not(unix))]
mod terminal {
    pub struct Guard(());

    pub fn enter() -> crate::Result<Guard> {
        Err("the live detail pane is unix-only; use --once or --json".into())
    }
}

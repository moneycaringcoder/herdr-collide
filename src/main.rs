//! collide — cross-worktree collision warnings for concurrent agents.
//!
//! Verb dispatch only. Each verb is implemented in its own module:
//!
//!   --once / --json / --watch   analysis + rendering  (collide.rs, render.rs)
//!   --enable / --disable /      badge updater control (daemon.rs)
//!   --toggle / --restore / --daemon

mod collide;
mod config;
mod daemon;
mod git;
mod herdr;
mod model;
mod render;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const USAGE: &str = "\
collide — cross-worktree collision warnings for herdr

Usage: collide [VERB]

Analysis:
  --once              Print a one-shot collision report and exit
  --json              Print the same report as JSON and exit
  --watch             Live detail view, refreshing on an interval

Badge updater:
  --enable            Start the background badge updater
  --disable           Stop it and clear every badge this plugin set
  --toggle            Stop it if running, otherwise start it
  --restore           Restart it only if it was enabled (herdr startup hook)
  --daemon            Run the updater in the foreground (internal)

Other:
  --interval <SECS>   Refresh interval for --watch and --daemon
  --version           Print version and exit
  --help              Show this help
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(err) = run(&args) {
        eprintln!("collide: {err}");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Result<()> {
    let verb = args.first().map(String::as_str).unwrap_or("--once");
    match verb {
        "--once" => collide::run_once(&config::load()?),
        "--json" => collide::run_json(&config::load()?),
        "--watch" => render::run_watch(&config::load_with_args(args)?),
        "--enable" => daemon::enable(),
        "--disable" => daemon::disable(),
        "--toggle" => daemon::toggle(),
        "--restore" => daemon::restore(),
        "--daemon" => daemon::run(&config::load_with_args(args)?),
        "--version" => {
            println!("collide {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "--help" | "-h" => {
            print!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown verb `{other}`\n\n{USAGE}").into()),
    }
}

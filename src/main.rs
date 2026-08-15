//! collide — cross-worktree collision warnings for concurrent agents.
//!
//! Verb dispatch only; every verb is implemented in the library crate.

use collide::{collide as analysis, config, daemon, render, setup, Result};

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

Sidebar setup:
  --setup             Add collide's tokens to herdr's config.toml and reload
  --setup-rollback    Restore the config.toml backup taken by --setup

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
        "--once" => analysis::run_once(&config::load()?),
        "--json" => analysis::run_json(&config::load()?),
        "--watch" => render::run_watch(&config::load_with_args(args)?),
        "--enable" => daemon::enable(),
        "--disable" => daemon::disable(),
        "--toggle" => daemon::toggle(),
        "--restore" => daemon::restore(),
        "--daemon" => daemon::run(&config::load_with_args(args)?),
        "--setup" => setup::run_setup(),
        "--setup-rollback" => setup::run_rollback(),
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

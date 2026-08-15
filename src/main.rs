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
  --base-ref <REF>    Ref each change set is measured against
                      (default: origin/HEAD)
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

/// Options that take a value, and so must never be mistaken for the verb.
const VALUED: [&str; 2] = ["--interval", "--base-ref"];

/// The verb is the first argument that is not an option or an option's value,
/// so `collide --base-ref origin/main --once` works as readily as
/// `collide --once --base-ref origin/main`. Ordering that matters is a papercut
/// nobody should have to learn.
fn verb_of(args: &[String]) -> &str {
    let mut skip_value = false;
    for arg in args {
        if skip_value {
            skip_value = false;
            continue;
        }
        let name = arg.split('=').next().unwrap_or(arg);
        if VALUED.contains(&name) {
            // `--interval=5` carries its value; bare `--interval 5` does not.
            skip_value = !arg.contains('=');
            continue;
        }
        return arg;
    }
    "--once"
}

fn run(args: &[String]) -> Result<()> {
    let verb = verb_of(args);
    match verb {
        "--once" => analysis::run_once(&config::load_with_args(args)?),
        "--json" => analysis::run_json(&config::load_with_args(args)?),
        "--watch" => render::run_watch(&config::load_with_args(args)?),
        "--enable" => daemon::enable(args),
        "--disable" => daemon::disable(),
        "--toggle" => daemon::toggle(args),
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

#[cfg(test)]
mod tests {
    use super::verb_of;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_verb_is_found_whatever_the_order() {
        assert_eq!(verb_of(&args(&["--once"])), "--once");
        assert_eq!(verb_of(&args(&["--json", "--interval", "5"])), "--json");
        assert_eq!(verb_of(&args(&["--interval", "5", "--json"])), "--json");
        assert_eq!(verb_of(&args(&["--interval=5", "--json"])), "--json");
        assert_eq!(
            verb_of(&args(&["--base-ref", "origin/main", "--watch"])),
            "--watch"
        );
    }

    #[test]
    fn no_arguments_means_a_one_shot_report() {
        assert_eq!(verb_of(&args(&[])), "--once");
        // Options alone still leave the default verb in place.
        assert_eq!(verb_of(&args(&["--interval", "5"])), "--once");
    }

    #[test]
    fn an_option_value_is_never_mistaken_for_a_verb() {
        // A value that looks like a verb must still be treated as a value.
        assert_eq!(verb_of(&args(&["--base-ref", "--json"])), "--once");
    }
}

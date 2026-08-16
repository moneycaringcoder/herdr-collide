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
                      (default: the repository's integration ref, probed —
                      origin/HEAD, then the conventional trunks)
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

/// Every verb this binary accepts. The list is explicit on purpose — see
/// [`verb_of`].
const VERBS: [&str; 13] = [
    "--once",
    "--json",
    "--watch",
    "--enable",
    "--disable",
    "--toggle",
    "--restore",
    "--daemon",
    "--setup",
    "--setup-rollback",
    "--version",
    "--help",
    "-h",
];

/// The verb is the first argument that *matches a known verb*, so
/// `collide --base-ref origin/main --once` works as readily as
/// `collide --once --base-ref origin/main`. Ordering that matters is a papercut
/// nobody should have to learn.
///
/// Recognising verbs by name rather than by elimination is the load-bearing
/// part. The obvious implementation — "the first argument that is not an option
/// or an option's value" — works only for as long as every option takes a
/// value, and reads the first boolean flag anyone adds *as the verb*. A sibling
/// plugin inherited this function, added one boolean flag, and made an entire
/// verb unreachable from the command line while its tests stayed green. The
/// failure mode is a verb that quietly does the wrong thing, which is precisely
/// the class of bug this codebase exists to be careful about.
///
/// An argument that is neither a known verb nor a known option is an error
/// rather than a shrug: `collide --intervl 5 --once` used to ignore the typo and
/// run with the default interval.
///
/// A *second* verb is an error for the same reason. `collide --disable --enable`
/// silently ran only the first of the two, which for a pair of verbs that undo
/// each other is the worst possible way to be wrong.
fn verb_of(args: &[String]) -> Result<&str> {
    let mut skip_value = false;
    let mut verb: Option<&str> = None;
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
        if VERBS.contains(&arg.as_str()) {
            match verb {
                None => verb = Some(arg.as_str()),
                Some(first) if first == arg.as_str() => {}
                Some(first) => {
                    return Err(format!(
                        "more than one verb given: `{first}` and `{arg}`\n\n{USAGE}"
                    )
                    .into())
                }
            }
            continue;
        }
        return Err(format!("unrecognised argument `{arg}`\n\n{USAGE}").into());
    }
    Ok(verb.unwrap_or("--once"))
}

fn run(args: &[String]) -> Result<()> {
    let verb = verb_of(args)?;
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
        // `verb_of` only ever returns something from `VERBS`, so this arm is
        // unreachable in practice; it exists so that adding a verb to the list
        // without wiring it up fails loudly rather than silently.
        other => Err(format!("verb `{other}` is not wired up\n\n{USAGE}").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{verb_of, VERBS};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn verb(list: &[&str]) -> String {
        verb_of(&args(list)).expect("a verb").to_string()
    }

    #[test]
    fn the_verb_is_found_whatever_the_order() {
        assert_eq!(verb(&["--once"]), "--once");
        assert_eq!(verb(&["--json", "--interval", "5"]), "--json");
        assert_eq!(verb(&["--interval", "5", "--json"]), "--json");
        assert_eq!(verb(&["--interval=5", "--json"]), "--json");
        assert_eq!(verb(&["--base-ref", "origin/main", "--watch"]), "--watch");
    }

    #[test]
    fn no_arguments_means_a_one_shot_report() {
        assert_eq!(verb(&[]), "--once");
        // Options alone still leave the default verb in place.
        assert_eq!(verb(&["--interval", "5"]), "--once");
    }

    #[test]
    fn an_option_value_is_never_mistaken_for_a_verb() {
        // A value that looks like a verb must still be treated as a value.
        assert_eq!(verb(&["--base-ref", "--json"]), "--once");
    }

    /// The trap that broke a sibling plugin: with verbs recognised by
    /// elimination, the first boolean flag added to the binary is read as the
    /// verb, and the verb it displaces becomes unreachable from the command
    /// line while every existing test keeps passing.
    #[test]
    fn a_future_boolean_flag_cannot_be_mistaken_for_a_verb() {
        let err = verb_of(&args(&["--force-dirty", "--once"])).unwrap_err();
        assert!(
            err.to_string().contains("--force-dirty"),
            "an unknown flag must be named, not silently treated as the verb"
        );
        // And once such a flag is genuinely added, it belongs in one of the two
        // lists; nothing else may reach the verb position.
        assert!(!VERBS.contains(&"--force-dirty"));
    }

    /// `--disable --enable` used to run `--disable` and say nothing about the
    /// other half, which for two verbs that undo each other is the quietest
    /// possible way to do the wrong thing.
    #[test]
    fn a_second_verb_is_an_error_naming_both() {
        let err = verb_of(&args(&["--disable", "--enable"])).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("--disable"), "{text}");
        assert!(text.contains("--enable"), "{text}");
        assert!(text.contains("more than one verb"), "{text}");

        // Order does not matter, and options between them do not hide it.
        let err = verb_of(&args(&["--json", "--interval", "5", "--once"])).unwrap_err();
        assert!(err.to_string().contains("more than one verb"));

        // The same verb twice is a harmless repetition, not a contradiction.
        assert_eq!(verb(&["--once", "--once"]), "--once");
    }

    #[test]
    fn a_mistyped_option_is_an_error_rather_than_a_shrug() {
        let err = verb_of(&args(&["--intervl", "5", "--once"])).unwrap_err();
        assert!(err.to_string().contains("--intervl"));
    }

    #[test]
    fn every_verb_in_the_list_is_accepted() {
        for name in VERBS {
            assert_eq!(verb(&[name]), name);
        }
    }
}

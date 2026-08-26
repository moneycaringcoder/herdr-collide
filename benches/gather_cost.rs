//! Manual collision-cost benchmark.
//!
//! Each case asserts its observable verdict before recording a timing. Run with
//! `cargo bench --bench gather_cost`; timings are informational and never a CI
//! gate.

#[path = "../tests/fixtures.rs"]
mod fixtures;

use std::hint::black_box;
use std::time::{Duration, Instant};

use collide::collide::{gather_for, Cycle};
use collide::config::Config;
use collide::git;
use collide::model::FileVerdict;
use fixtures::{checkout, Fixture};

const TIMEOUT: Duration = Duration::from_secs(30);

fn config(predict_conflicts: bool) -> Config {
    Config {
        predict_conflicts,
        base_ref: "main".to_string(),
        git_timeout: TIMEOUT,
        ..Config::default()
    }
}

fn measure(
    label: &str,
    worktrees: usize,
    pairs: usize,
    samples: usize,
    mut run: impl FnMut() -> Cycle,
) {
    black_box(run());
    let started = Instant::now();
    for _ in 0..samples {
        black_box(run());
    }
    let elapsed = started.elapsed();
    let mean_ms = elapsed.as_secs_f64() * 1_000.0 / samples as f64;
    println!(
        "{label},{worktrees},{pairs},{samples},{:.3},{mean_ms:.3},{:.3}",
        elapsed.as_secs_f64() * 1_000.0,
        mean_ms / worktrees.max(1) as f64,
    );
}

fn outer_gather(worktree_count: usize, samples: usize) {
    let fixture = Fixture::new(&format!("bench-outer-{worktree_count}"));
    let key = git::repo_key(&fixture.repo, TIMEOUT).expect("repo key");
    let mut checkouts = Vec::with_capacity(worktree_count);
    for index in 0..worktree_count {
        let name = format!("bench-{index:02}");
        let worktree = fixture.worktree(&name, &name);
        fixture.write(
            &worktree,
            "shared.txt",
            &format!("worktree {index}\nline 2\nline 3\n"),
        );
        checkouts.push(checkout(&name, &worktree, &key.0));
    }
    let expected_pairs = worktree_count * worktree_count.saturating_sub(1) / 2;
    let run_config = config(false);

    measure(
        &format!("outer-{worktree_count}"),
        worktree_count,
        expected_pairs,
        samples,
        || {
            let cycle = gather_for(checkouts.clone(), &run_config).expect("gather outer worktrees");
            assert_eq!(cycle.report.checkouts.len(), worktree_count);
            assert_eq!(cycle.report.pairings.len(), expected_pairs);
            assert!(cycle.report.pairings.iter().all(|pair| {
                pair.shared.len() == 1 && pair.shared[0].verdict == FileVerdict::Overlap
            }));
            cycle
        },
    );
}

fn predicted_conflict(samples: usize) {
    let fixture = Fixture::new("bench-conflict");
    let (left, right) = fixture.committed_conflict_pair();
    let key = git::repo_key(&fixture.repo, TIMEOUT).expect("repo key");
    let checkouts = vec![
        checkout("left", &left, &key.0),
        checkout("right", &right, &key.0),
    ];
    let run_config = config(true);

    measure("predicted-conflict", 2, 1, samples, || {
        let cycle = gather_for(checkouts.clone(), &run_config).expect("gather conflict pair");
        assert_eq!(cycle.report.pairings.len(), 1);
        assert_eq!(cycle.report.pairings[0].conflicts(), 1);
        cycle
    });
}

fn dirty_submodule(samples: usize) {
    let fixture = Fixture::new("bench-submodule");
    let (_superproject, left, right, left_nested) = fixture.superproject_with_submodule("embedded");
    let right_nested = right.join("embedded");
    fixture.write(&left_nested, "payload.txt", "alpha\nLEFT\ngamma\n");
    fixture.write(&right_nested, "payload.txt", "alpha\nRIGHT\ngamma\n");
    let key = git::repo_key(&fixture.repo, TIMEOUT).expect("repo key");
    let checkouts = vec![
        checkout("left", &left, &key.0),
        checkout("right", &right, &key.0),
    ];
    let run_config = config(true);

    measure("dirty-submodule", 2, 1, samples, || {
        let cycle = gather_for(checkouts.clone(), &run_config).expect("gather dirty submodule");
        assert_eq!(cycle.report.pairings.len(), 1);
        assert_eq!(cycle.report.pairings[0].shared.len(), 1);
        assert_eq!(
            cycle.report.pairings[0].shared[0].verdict,
            FileVerdict::Conflict
        );
        cycle
    });
}

fn main() {
    let state = std::env::temp_dir().join(format!("collide-bench-state-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state);
    std::fs::create_dir_all(&state).expect("create benchmark state");
    std::env::set_var("HERDR_PLUGIN_STATE_DIR", &state);

    println!("case,worktrees,pairs,samples,total_ms,mean_ms,mean_ms_per_worktree");
    outer_gather(2, 20);
    outer_gather(4, 15);
    outer_gather(8, 10);
    outer_gather(16, 5);
    predicted_conflict(15);
    dirty_submodule(5);

    let _ = std::fs::remove_dir_all(state);
}

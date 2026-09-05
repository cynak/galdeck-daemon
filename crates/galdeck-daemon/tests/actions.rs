//! A deck invites mashing, so what happens when it is mashed matters.

use std::time::{Duration, Instant};

use galdeck_daemon::actions::{ActionRunner, MAX_OUTSTANDING};

#[test]
fn a_launched_process_is_not_waited_for() {
    // Most actions open a window and run for hours. The deck has to stay
    // responsive the whole time.
    let runner = ActionRunner::new();
    let started = Instant::now();
    runner.run("sleep 30", None);
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "run() blocked for {:?}",
        started.elapsed()
    );
}

#[test]
fn finished_processes_are_collected_without_a_thread_each() {
    // The old shape spawned a thread per action to sit in wait(), so a
    // terminal opened from the deck parked one for as long as the window was
    // up. One reaper handles all of them.
    let runner = ActionRunner::new();
    for _ in 0..20 {
        runner.run("true", None);
    }
    assert!(runner.outstanding() > 0);

    let deadline = Instant::now() + Duration::from_secs(10);
    while runner.outstanding() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        runner.outstanding(),
        0,
        "finished children were never reaped"
    );
}

#[test]
fn mashing_a_key_cannot_fork_without_limit() {
    // Held against the desk, a key used to fork until something gave, and the
    // something was usually the session rather than the daemon.
    let runner = ActionRunner::new();
    for _ in 0..(MAX_OUTSTANDING * 3) {
        runner.run("sleep 30", None);
    }
    assert!(
        runner.outstanding() <= MAX_OUTSTANDING,
        "{} processes outstanding, cap is {MAX_OUTSTANDING}",
        runner.outstanding()
    );
}

#[test]
fn the_rotation_delta_reaches_the_command() {
    // `GALDECK_DELTA` is the one thing a bound command is told about the
    // event that ran it.
    let output = galdeck_daemon::actions::action_command("echo -n \"$GALDECK_DELTA\"", Some(-3))
        .output()
        .expect("echo should run");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "-3");
}

#[test]
fn a_command_without_a_delta_does_not_get_a_stale_one() {
    let output = galdeck_daemon::actions::action_command("echo -n \"[$GALDECK_DELTA]\"", None)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "[]");
}

#[test]
fn a_command_that_does_not_exist_is_survivable() {
    let runner = ActionRunner::new();
    runner.run("definitely-not-a-real-command-xyz", None);
    // `sh` starts fine and exits non-zero, so this is a child like any other.
    runner.run("true", None);
}

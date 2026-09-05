//! Running the commands keys are bound to.
//!
//! Two things make this more than `Command::spawn`.
//!
//! Actions are usually *launches* — a terminal, a browser — which run for
//! hours. The daemon must not wait for them, but it must eventually reap them
//! or they accumulate as zombies. Spawning a thread per action to sit in
//! `wait()` solves the zombies and creates a thread that lives as long as the
//! window the user opened; a few days of use is a few hundred parked threads.
//! One reaper for all of them costs one thread total.
//!
//! And a deck invites mashing. Without a ceiling, a key held against the desk
//! forks until something gives, and the something is usually the session
//! rather than the daemon.

use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How many launched processes may be outstanding at once.
///
/// Far above any real use — a dozen windows open from the deck is a lot — and
/// far below the point where forking more does harm.
pub const MAX_OUTSTANDING: usize = 64;
/// How often to collect finished children.
///
/// They are already gone by then; this only reaps the entry in the process
/// table, so it can be lazy.
const REAP_INTERVAL: Duration = Duration::from_millis(500);

/// Spawns key actions and cleans up after them.
#[derive(Clone)]
pub struct ActionRunner {
    children: Arc<Mutex<Vec<Child>>>,
}

impl Default for ActionRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl ActionRunner {
    pub fn new() -> Self {
        let children: Arc<Mutex<Vec<Child>>> = Arc::new(Mutex::new(Vec::new()));
        let reaping = Arc::clone(&children);

        std::thread::Builder::new()
            .name("galdeck-reaper".into())
            .spawn(move || loop {
                std::thread::sleep(REAP_INTERVAL);
                let mut children = reaping.lock().expect("children poisoned");
                // Keep the ones still running; `try_wait` is what turns a
                // finished child from a zombie into nothing.
                children.retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
                // The daemon outlives every deck session, so this thread never
                // exits; it costs one sleeping thread.
            })
            .expect("spawning the action reaper");

        Self { children }
    }

    /// Launch a command and forget about it.
    ///
    /// Deliberately not waited on: most of these open a window, and the deck
    /// must stay responsive while it is up.
    pub fn run(&self, command: &str, delta: Option<i8>) {
        let mut children = self.children.lock().expect("children poisoned");
        if children.len() >= MAX_OUTSTANDING {
            log::warn!(
                "{MAX_OUTSTANDING} actions are already running; refusing to start {command:?}"
            );
            return;
        }
        log::info!("exec: {command}");
        match action_command(command, delta).spawn() {
            Ok(child) => children.push(child),
            Err(e) => log::warn!("running {command:?}: {e}"),
        }
    }

    /// How many launched processes are outstanding. For tests.
    pub fn outstanding(&self) -> usize {
        self.children.lock().expect("children poisoned").len()
    }
}

/// Build the command a key runs.
///
/// `GALDECK_DELTA` carries the signed step count of the rotation that caused
/// it, so a script can scale its effect rather than being run once per detent
/// and having to guess.
pub fn action_command(command: &str, delta: Option<i8>) -> Command {
    let mut process = Command::new("sh");
    process
        .arg("-c")
        .arg(command)
        // Nothing here should be able to read the daemon's stdin.
        .stdin(Stdio::null());
    if let Some(delta) = delta {
        process.env("GALDECK_DELTA", delta.to_string());
    }
    process
}

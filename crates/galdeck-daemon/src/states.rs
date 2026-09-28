//! Running the commands of keys that cycle through states.
//!
//! Two kinds of job, each run on a short-lived thread the way a geocode
//! request is, and each reported back on a channel with the engine's waker,
//! the way widget samples and control reports are. A job never runs on the
//! engine's thread, which must keep polling the deck.
//!
//! An Enter job runs a state's `exec` as the key enters it. It runs in the
//! user's locale and is never killed: a state may well start something meant
//! to keep running, and killing it at a deadline would undo the change the
//! key just showed. It reports how it exited if it does within
//! [`ENTER_WINDOW`], else [`Outcome::Launched`] then; the thread stays to
//! reap it, but reports nothing more.
//!
//! A Status job runs a key's `status` command to read which state it is in.
//! It runs in the C locale, so nmcli and its like print the untranslated
//! words a state's `match` is written in, and it is killed with everything it
//! started at [`STATUS_DEADLINE`]: it only reads, and a read that hangs
//! answers nothing.
//!
//! A key has at most one job of each kind at a time. The engine keeps to
//! that for Enter jobs itself, holding a tap back while the key's last
//! command runs, and this refuses a job that would break it rather than trust
//! it. A Status job asked for while the key's last read has not answered is
//! refused as a matter of course: a read slower than its interval misses a
//! turn. At most
//! [`MAX_STATUS`] Status jobs run at once and the rest wait here, oldest
//! first, so a page of toggles due for a read together does not start a
//! burst of shells. A waiting job runs on the thread of the one whose place
//! it takes.
//!
//! [`KeyCycle`] is what the engine keeps of each key with states, and the
//! rules for it: what a tap, a finished command and a read each do to what
//! the key shows. They are here rather than in the engine so they can be
//! tested over made-up ticks, as a countdown's are.

use std::collections::{HashSet, VecDeque};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use galdeck_core::{Tick, Waker};
use galdeck_model::v2::{KeyConfig, MAX_STATUS_LINE};

pub use crate::widgets::command::Outcome;
use crate::widgets::command::{run_bounded, run_unkilled};

/// How long an Enter job has to exit before it counts as launched.
pub const ENTER_WINDOW: Duration = Duration::from_secs(10);
/// How long a Status job may take before it is killed.
pub const STATUS_DEADLINE: Duration = Duration::from_secs(5);
/// How many Status jobs may run at once.
pub const MAX_STATUS: usize = 4;
/// What a Status job's environment gains. UTF-8 rather than plain C, so a
/// Wi-Fi network's name comes through as itself.
const STATUS_ENV: &[(&str, &str)] = &[("LC_ALL", "C.UTF-8")];

/// Which key is whose: the key it is, by the names of its page and profile,
/// so what the engine keeps about it survives the page or the profile not
/// showing, and a reload that moves pages around. A name is the first page
/// with it: a later page given the same id keeps nothing about its keys.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyHome {
    pub profile: String,
    pub page: String,
    pub key: u8,
}

/// What a job runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateKind {
    /// A state's `exec`, as the key enters the state.
    Enter { command: String },
    /// The key's `status` command, to read the state it is in.
    Status { command: String },
}

/// A command to run for a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateJob {
    pub home: KeyHome,
    /// The engine's own number for the job, handed back in its report so an
    /// answer to a job it has since given up on can be told apart.
    pub seq: u64,
    pub kind: StateKind,
}

/// How a job went: the job, and its outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateReport {
    pub home: KeyHome,
    pub seq: u64,
    pub kind: StateKind,
    pub outcome: Outcome,
}

/// Runs the jobs, and reports each one once.
pub struct StateRunner {
    shared: Arc<Shared>,
}

/// What the runner and its threads share.
struct Shared {
    /// Unbounded, because a report is never dropped: the engine counts on it
    /// to know the key's command is done. Its length is bounded all the same,
    /// by one job of each kind per key.
    reports: Sender<StateReport>,
    waker: Waker,
    enter_window: Duration,
    status_deadline: Duration,
    lanes: Mutex<Lanes>,
}

/// Which keys have a job, and the Status jobs waiting for a place.
#[derive(Default)]
struct Lanes {
    /// Keys with an Enter job that has not reported yet.
    entering: HashSet<KeyHome>,
    /// Keys with a Status job running or waiting.
    reading: HashSet<KeyHome>,
    /// How many Status jobs are running.
    running: usize,
    /// Status jobs waiting for a place, oldest first. Only ever filled
    /// while [`MAX_STATUS`] are running.
    waiting: VecDeque<StateJob>,
}

impl StateRunner {
    pub fn new(waker: Waker) -> (Self, Receiver<StateReport>) {
        Self::with_timing(waker, ENTER_WINDOW, STATUS_DEADLINE)
    }

    /// A runner whose Enter jobs count as launched after `enter_window` and
    /// whose Status jobs are killed after `status_deadline`. For tests, which
    /// should not take ten seconds to see a launch.
    pub fn with_timing(
        waker: Waker,
        enter_window: Duration,
        status_deadline: Duration,
    ) -> (Self, Receiver<StateReport>) {
        let (reports, reports_rx) = channel();
        let shared = Arc::new(Shared {
            reports,
            waker,
            enter_window,
            status_deadline,
            lanes: Mutex::new(Lanes::default()),
        });
        (Self { shared }, reports_rx)
    }

    /// Start `job`, or queue it behind the Status jobs already running.
    ///
    /// False, and nothing is run, if the key already has a job of the same
    /// kind that has not reported yet. Every job taken is reported exactly
    /// once, even one that could not be started.
    pub fn spawn(&self, job: StateJob) -> bool {
        let mut lanes = self.shared.lanes.lock().expect("state lanes poisoned");
        let lane = match job.kind {
            StateKind::Enter { .. } => &mut lanes.entering,
            StateKind::Status { .. } => &mut lanes.reading,
        };
        if !lane.insert(job.home.clone()) {
            let key = &job.home;
            match job.kind {
                // The engine holds a tap back while the key's last command
                // runs, so this is its mistake.
                StateKind::Enter { .. } => log::warn!(
                    "key {} on {}/{} already has a state change under way; not starting another",
                    key.key,
                    key.profile,
                    key.page
                ),
                // A read slower than the key's interval, or one kept waiting
                // behind others, has not answered by the time the next is
                // due. The next is skipped: the answer on its way is as
                // fresh. Not a warning, or a slow read would log one for ever.
                StateKind::Status { .. } => log::debug!(
                    "key {} on {}/{} is still reading its state; skipping a read",
                    key.key,
                    key.profile,
                    key.page
                ),
            }
            return false;
        }
        if matches!(job.kind, StateKind::Status { .. }) {
            if lanes.running >= MAX_STATUS {
                lanes.waiting.push_back(job);
                return true;
            }
            lanes.running += 1;
        }
        drop(lanes);
        self.shared.start(job);
        true
    }
}

impl Shared {
    /// Run `job` on a thread of its own.
    ///
    /// With no thread to be had, it is reported as failed, and the Status
    /// job waiting for its place, if there is one, is tried in its stead.
    fn start(self: &Arc<Self>, mut job: StateJob) {
        loop {
            let shared = Arc::clone(self);
            let kept = job.clone();
            let spawned = std::thread::Builder::new()
                .name("galdeck-state".into())
                .spawn(move || shared.work(job));
            let Err(e) = spawned else {
                return;
            };
            log::warn!("could not start a thread for a key's command: {e}");
            let failed = Outcome::Failed(format!("could not start a thread for it: {e}"));
            match self.finish(kept, failed) {
                Some(next) => job = next,
                None => return,
            }
        }
    }

    /// Run `job`, then any Status jobs that were waiting for its place.
    fn work(&self, job: StateJob) {
        let mut next = Some(job);
        while let Some(job) = next.take() {
            next = match &job.kind {
                StateKind::Enter { command } => {
                    let command = command.clone();
                    let mut job = Some(job);
                    run_unkilled(&command, self.enter_window, |outcome| {
                        if let Some(job) = job.take() {
                            self.finish(job, outcome);
                        }
                    });
                    None
                }
                StateKind::Status { command } => {
                    let outcome = run_bounded(command, self.status_deadline, STATUS_ENV);
                    self.finish(job, outcome)
                }
            };
        }
    }

    /// Free `job`'s key for another job of its kind and report how it went,
    /// handing back the waiting Status job that takes its place, if any.
    fn finish(&self, job: StateJob, outcome: Outcome) -> Option<StateJob> {
        let next = {
            let mut lanes = self.lanes.lock().expect("state lanes poisoned");
            match job.kind {
                StateKind::Enter { .. } => {
                    lanes.entering.remove(&job.home);
                    None
                }
                StateKind::Status { .. } => {
                    lanes.reading.remove(&job.home);
                    let next = lanes.waiting.pop_front();
                    if next.is_none() {
                        lanes.running -= 1;
                    }
                    next
                }
            }
        };
        // Freed before it is sent, so the engine may start the key's next
        // job the moment it reads this.
        let report = StateReport {
            home: job.home,
            seq: job.seq,
            kind: job.kind,
            outcome,
        };
        // A closed channel is an engine that has stopped, which has no use
        // for the answer.
        let _ = self.reports.send(report);
        self.waker.notify();
        next
    }
}

// ------------------------------------------------------------ key cycles

/// How long a state's command runs before its key shows that it is still
/// running: past a blink, so a command that answers at once never shows it.
pub const PENDING_CUE: Duration = Duration::from_millis(250);
/// How long a key says its command failed, once it has gone back.
pub const FAILED_FLASH: Duration = Duration::from_secs(1);
/// When a key with `status` is read after its command has finished, to see
/// that it took: soon, then twice more for what takes a moment to settle --
/// a Bluetooth adapter powering up, a network connecting.
pub const VERIFY_AFTER: [Duration; 3] = [
    Duration::from_millis(300),
    Duration::from_secs(1),
    Duration::from_secs(3),
];
/// How long after a command has finished a read that disagrees with it
/// needs a second read to agree: what was just switched may still read as
/// it was, or as something in between.
pub const GRACE: Duration = Duration::from_secs(3);
/// How far apart the two reads that overrule a command in its grace must be.
pub const AGREE_APART: Duration = Duration::from_secs(1);
/// The furthest apart a key's reads are spaced after failing in a row.
pub const MAX_READ_INTERVAL: Duration = Duration::from_secs(60);

/// What one read of a key's state found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    /// One of its states, by name.
    State(String),
    /// None of them: the command failed, printed nothing, or printed what no
    /// state matches.
    Nothing,
}

/// What a key's last read found, as an editor is told it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastRead {
    pub at: Tick,
    /// The line it printed, made safe to show.
    pub output: Option<String>,
    /// Why it did not answer, made safe to show; `None` when it printed
    /// something, whether or not that is a state.
    pub error: Option<String>,
    /// Whether what it printed is one of the key's states.
    pub ok: bool,
}

/// What a key shows about its state besides the state itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    /// Its command failed, and it has gone back.
    Failed,
    /// Its command is still running.
    Pending,
    /// What state it is in could not be read.
    Unknown,
}

/// The Enter job a key is waiting on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entering {
    pub seq: u64,
    /// The state it enters.
    pub state: String,
    /// When the key starts to show that it is still running.
    pub cue_at: Tick,
    /// Whether the key has been repainted to show so.
    pub cued: bool,
}

/// What became of an Enter job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entered {
    /// Not the job the key is waiting on.
    Stale,
    /// It worked, and the key is in the state it shows.
    Settled,
    /// The key was moved on while it ran: this is the state to enter now.
    /// The ones tapped past on the way are never entered.
    Next(String),
    /// It failed, and the key has gone back to where it was.
    Reverted,
}

/// What a key's timers asked for when they came due.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Due {
    /// What the key shows has changed.
    pub repaint: bool,
    /// A read after its command is due.
    pub read: bool,
}

/// What the engine keeps of one key with states, by name rather than
/// position, so it means the same after a reload that reorders them.
///
/// The key shows the state it is in, or is going to: a tap moves it at
/// once and its command follows. A tap while a command runs only moves what
/// the key shows, and when the command is done the key's last state is
/// entered, the ones tapped past never. A command that fails takes the key
/// back. A read overrules what the key shows, except a read begun before
/// someone last changed it, and, for a moment after a command, a single read
/// that disagrees with it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyCycle {
    /// The state the key shows. `None` while a key that can read its state
    /// has not yet, when it shows its own look.
    pub shown: Option<String>,
    /// The state it is known to be in: where its last command left it, or
    /// what it was last read as. Where a failed command takes it back to.
    pub settled: Option<String>,
    /// Whether `shown` was read, rather than being what the key last did.
    pub known: bool,
    /// The command running for it.
    pub running: Option<Entering>,
    /// It was moved on while `running` ran, to `shown`.
    pub queued: bool,
    /// Counts the changes someone asked for, so a read begun before one is
    /// not taken as the answer after it.
    pub epoch: u64,
    /// Until when, after a command, a read that disagrees needs another.
    pub grace_until: Option<Tick>,
    /// A read in the grace that disagreed: what it found, and when.
    pub disagree: Option<(Found, Tick)>,
    /// Reads that found nothing, in a row. Each doubles the wait for the
    /// next.
    pub failures: u32,
    /// The last read found nothing: the "?".
    pub unknown: bool,
    /// Until when the key says its command failed.
    pub failed_until: Option<Tick>,
    /// The read under way, and the epoch it began in.
    pub reading: Option<(u64, u64)>,
    /// The reads due after a command, soonest first.
    pub verify: Vec<Tick>,
    /// What the last read found, for an editor.
    pub last_read: Option<LastRead>,
}

impl KeyCycle {
    /// `cfg` before anything has happened to it: in its first state, or,
    /// when it can read which it is in, in none until it has.
    pub fn new(cfg: &KeyConfig) -> KeyCycle {
        let first = cfg
            .status
            .is_none()
            .then(|| cfg.states.first().map(|state| state.name.clone()))
            .flatten();
        KeyCycle {
            shown: first.clone(),
            settled: first,
            ..KeyCycle::default()
        }
    }

    /// Someone moved the key to `to`. It shows it now; whether its command
    /// starts now too, or waits for the one running, is what this returns.
    pub fn press(&mut self, to: String) -> bool {
        self.shown = Some(to);
        self.changed_by_hand();
        if self.running.is_some() {
            self.queued = true;
            return false;
        }
        true
    }

    /// Someone set what the key shows without running anything, for a key
    /// that has got out of step with what it stands for.
    pub fn show(&mut self, to: String) {
        self.shown = Some(to.clone());
        self.settled = Some(to.clone());
        // A state waiting to be entered would be entered once the command
        // running is done, and showing runs nothing.
        self.queued = false;
        // Nor does that command, done, move the key from where it was put:
        // it would otherwise settle on its own state, which the key no
        // longer shows, and a later failure would go back to that.
        if let Some(running) = &mut self.running {
            running.state = to;
        }
        self.changed_by_hand();
    }

    /// What any change made by hand does: what was read before it no
    /// longer counts, and reads start again from their usual pace.
    fn changed_by_hand(&mut self) {
        self.epoch += 1;
        self.known = false;
        self.unknown = false;
        self.failed_until = None;
        self.failures = 0;
        self.grace_until = None;
        self.disagree = None;
        self.verify.clear();
    }

    /// Enter job `seq`, for `state`, has started.
    pub fn begin(&mut self, seq: u64, state: String, now: Tick) {
        self.running = Some(Entering {
            seq,
            state,
            cue_at: now.saturating_add(PENDING_CUE),
            cued: false,
        });
    }

    /// Enter job `seq` has finished, well or not.
    pub fn entered(&mut self, seq: u64, succeeded: bool, now: Tick) -> Entered {
        let Some(running) = self.running.take_if(|running| running.seq == seq) else {
            return Entered::Stale;
        };
        if succeeded {
            self.settled = Some(running.state);
        }
        if std::mem::take(&mut self.queued) && self.shown != self.settled {
            if let Some(next) = self.shown.clone() {
                return Entered::Next(next);
            }
        }
        if succeeded {
            return Entered::Settled;
        }
        self.shown.clone_from(&self.settled);
        self.failed_until = Some(now.saturating_add(FAILED_FLASH));
        Entered::Reverted
    }

    /// Read the key a few times soon, to see that its command took. Until
    /// the last of those, one read that disagrees is not enough.
    pub fn expect_reads(&mut self, now: Tick) {
        self.verify = VERIFY_AFTER
            .iter()
            .map(|after| now.saturating_add(*after))
            .collect();
        self.grace_until = Some(now.saturating_add(GRACE));
        self.disagree = None;
    }

    /// Whether a read may start: not while a command runs or waits, since
    /// its answer would only race the command; not while one is under way.
    pub fn may_read(&self) -> bool {
        self.running.is_none() && !self.queued && self.reading.is_none()
    }

    /// Read `seq` has begun.
    pub fn reading(&mut self, seq: u64) {
        self.reading = Some((seq, self.epoch));
    }

    /// Read `seq` found `found`, and `last` is how to describe it. False,
    /// and nothing changes, when the answer is not wanted: it is not the
    /// read under way, or someone changed the key after it began, or a
    /// command is running or waiting for the key.
    pub fn read(&mut self, seq: u64, found: Found, last: LastRead, now: Tick) -> bool {
        let Some((reading, began)) = self.reading else {
            return false;
        };
        if reading != seq {
            return false;
        }
        self.reading = None;
        if began < self.epoch || self.running.is_some() || self.queued {
            return false;
        }
        self.last_read = Some(last);
        let confirms = matches!(&found, Found::State(name) if self.shown.as_ref() == Some(name));
        if confirms {
            self.settle_on(found);
            return true;
        }
        if self.grace_until.is_some_and(|until| now < until) {
            match &self.disagree {
                Some((before, at))
                    if *before == found && now.duration_since(*at) >= AGREE_APART => {}
                Some((before, _)) if *before == found => return true,
                _ => {
                    self.disagree = Some((found, now));
                    return true;
                }
            }
        }
        self.settle_on(found);
        true
    }

    /// Take what a read found as what the key shows.
    fn settle_on(&mut self, found: Found) {
        self.disagree = None;
        match found {
            Found::State(name) => {
                self.shown = Some(name.clone());
                self.settled = Some(name);
                self.known = true;
                self.unknown = false;
                self.failures = 0;
            }
            // The key keeps what it showed, and says it is not sure of it.
            Found::Nothing => {
                self.known = false;
                self.unknown = true;
                self.failures = self.failures.saturating_add(1);
            }
        }
    }

    /// How long until the key is read again, when it is read `every` so
    /// often while all is well: twice as long for each read in a row that
    /// found nothing, up to [`MAX_READ_INTERVAL`] -- or `every`, when that
    /// is longer still.
    pub fn read_interval(&self, every: Duration) -> Duration {
        let spaced = every.saturating_mul(1 << self.failures.min(16));
        spaced.min(MAX_READ_INTERVAL.max(every))
    }

    /// What the key shows about its state at `now` besides the state, the
    /// most pressing when there is more than one: a failure, which is over
    /// in a moment and says the key just went back; a command still
    /// running, which says a tap has been taken; a state that could not be
    /// read, which stays until one can.
    pub fn badge(&self, now: Tick) -> Option<Badge> {
        if self.failed_until.is_some_and(|until| now < until) {
            return Some(Badge::Failed);
        }
        if self
            .running
            .as_ref()
            .is_some_and(|running| now >= running.cue_at)
        {
            return Some(Badge::Pending);
        }
        self.unknown.then_some(Badge::Unknown)
    }

    /// When the next of the key's timers comes due.
    pub fn next_deadline(&self) -> Option<Tick> {
        let cue = self
            .running
            .as_ref()
            .filter(|running| !running.cued)
            .map(|running| running.cue_at);
        [cue, self.failed_until, self.verify.first().copied()]
            .into_iter()
            .flatten()
            .min()
    }

    /// Act on the key's timers that are due at `now`.
    pub fn due(&mut self, now: Tick) -> Due {
        let mut due = Due::default();
        if self.failed_until.is_some_and(|until| until <= now) {
            self.failed_until = None;
            due.repaint = true;
        }
        if let Some(running) = self.running.as_mut() {
            if !running.cued && running.cue_at <= now {
                running.cued = true;
                due.repaint = true;
            }
        }
        let reads = self.verify.iter().take_while(|at| **at <= now).count();
        if reads > 0 {
            self.verify.drain(..reads);
            due.read = true;
        }
        due
    }

    /// Keep through a reload what still means something on `cfg`, the key
    /// as it is now: states by their names, as they are now spelt, and what
    /// was read only while the key can still read.
    pub fn keep(&mut self, cfg: &KeyConfig) {
        let own = |name: &Option<String>| {
            name.as_deref()
                .and_then(|name| cfg.state(name))
                .map(|state| state.name.clone())
        };
        self.shown = own(&self.shown);
        self.settled = own(&self.settled);
        // What a command under way enters is settled on once it is done, so
        // it is spelt as the rest are.
        if let Some(running) = &mut self.running {
            if let Some(name) = own(&Some(running.state.clone())) {
                running.state = name;
            }
        }
        if let Some((Found::State(name), _)) = &self.disagree {
            if cfg.state(name).is_none() {
                self.disagree = None;
            }
        }
        if cfg.status.is_none() {
            self.known = false;
            self.unknown = false;
            self.failures = 0;
            self.grace_until = None;
            self.disagree = None;
            self.verify.clear();
            self.last_read = None;
        }
    }
}

/// What a Status job's `outcome` says about which of `cfg`'s states it is
/// in, and how to tell an editor what it found.
///
/// Only a command that exits 0 and prints something is taken at its word;
/// anything else says nothing about the state, whatever it printed.
pub fn judge(cfg: &KeyConfig, outcome: &Outcome, now: Tick) -> (Found, LastRead) {
    let safe = |text: &str| crate::pipewire::display_name(text, MAX_STATUS_LINE);
    let (line, error) = match outcome {
        Outcome::Exited {
            code: 0,
            stdout_first_line: Some(line),
            ..
        } => (Some(line.as_str()), None),
        Outcome::Exited { code: 0, .. } => (None, Some("it printed nothing".to_string())),
        Outcome::Exited {
            code,
            stderr_first_line,
            ..
        } => (
            None,
            Some(match stderr_first_line {
                Some(said) => safe(said),
                None => format!("it exited with status {code}"),
            }),
        ),
        Outcome::TimedOut => (
            None,
            Some(format!(
                "it had not answered after {} s",
                STATUS_DEADLINE.as_secs()
            )),
        ),
        // Only an Enter job is ever launched.
        Outcome::Launched => (None, Some("it did not finish".to_string())),
        Outcome::Failed(why) => (None, Some(safe(why))),
    };
    let state = line.and_then(|line| cfg.state_matching(line));
    let found = match state {
        Some(state) => Found::State(state.name.clone()),
        None => Found::Nothing,
    };
    let last = LastRead {
        at: now,
        output: line.map(safe),
        error,
        ok: state.is_some(),
    };
    (found, last)
}

/// A state's label, else its name.
fn state_label(cfg: &KeyConfig, state: &str) -> String {
    cfg.state(state)
        .and_then(|state| state.label.as_deref())
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .unwrap_or(state)
        .to_string()
}

/// What the screen says when someone moves `cfg` to `state`: the key's
/// label and the state's, "Power · Performance", and where it is among
/// them when there are more than two to step through, as a knob's modes
/// say which they are.
pub fn state_text(cfg: &KeyConfig, state: &str) -> String {
    let own = state_label(cfg, state);
    let mut text = match cfg.label.as_deref().map(str::trim) {
        Some(key) if !key.is_empty() => {
            // A state labelled as the key is, as Wi-Fi's "on" is, goes by
            // its name here: "Wi-Fi · Wi-Fi" would not say which it is.
            let label = if key.eq_ignore_ascii_case(&own) {
                state
            } else {
                own.as_str()
            };
            if key.eq_ignore_ascii_case(label) {
                key.to_string()
            } else {
                format!("{key} · {label}")
            }
        }
        _ => own,
    };
    let count = cfg.states.len();
    if count >= 3 {
        if let Some(at) = cfg.states.iter().position(|s| s.name == state) {
            text.push_str(&format!(" {}/{count}", at + 1));
        }
    }
    text
}

/// What the screen says when the command of key `key`, `cfg`, has failed:
/// its label, and the first thing the command said about it, else how it
/// exited.
pub fn failure_text(cfg: &KeyConfig, key: u8, outcome: &Outcome) -> String {
    let label = cfg
        .label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map_or_else(|| format!("Key {}", u32::from(key) + 1), str::to_string);
    let why = match outcome {
        Outcome::Exited {
            stderr_first_line: Some(said),
            ..
        } => crate::pipewire::display_name(said, 60),
        Outcome::Exited { code, .. } => format!("command failed (exit {code})"),
        Outcome::Failed(why) => crate::pipewire::display_name(why, 60),
        Outcome::TimedOut | Outcome::Launched => "command failed".to_string(),
    };
    format!("{label}: {why}")
}

/// The state after `from` on `cfg`, going round, or the one before it; the
/// first when there is no `from`, or it is no longer one of them.
pub fn neighbour(cfg: &KeyConfig, from: Option<&str>, forward: bool) -> Option<String> {
    let count = cfg.states.len();
    let at = from.and_then(|from| cfg.states.iter().position(|state| state.name == from));
    let to = match at {
        Some(at) if forward => (at + 1) % count,
        Some(at) => (at + count - 1) % count,
        None => 0,
    };
    cfg.states.get(to).map(|state| state.name.clone())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Instant;

    use super::*;
    use crate::widgets::command::testing::*;

    fn home(key: u8) -> KeyHome {
        KeyHome {
            profile: "default".into(),
            page: "main".into(),
            key,
        }
    }

    fn enter(key: u8, seq: u64, command: &str) -> StateJob {
        StateJob {
            home: home(key),
            seq,
            kind: StateKind::Enter {
                command: command.into(),
            },
        }
    }

    fn status(key: u8, seq: u64, command: &str) -> StateJob {
        StateJob {
            home: home(key),
            seq,
            kind: StateKind::Status {
                command: command.into(),
            },
        }
    }

    /// A command that waits until `gate` exists, having first touched
    /// `started`. It prints nothing: an Enter job's stdout is the test's.
    fn held(started: &Path, gate: &Path) -> String {
        format!(
            "touch '{}'; while [ ! -e '{}' ]; do sleep 0.02; done",
            started.display(),
            gate.display()
        )
    }

    fn runner(enter_window: Duration) -> (StateRunner, Receiver<StateReport>) {
        let (waker, _) = galdeck_core::wake_channel();
        StateRunner::with_timing(waker, enter_window, STATUS_DEADLINE)
    }

    fn report(reports: &Receiver<StateReport>) -> StateReport {
        reports
            .recv_timeout(Duration::from_secs(10))
            .expect("a report")
    }

    #[test]
    fn an_enter_job_that_fails_quickly_reports_its_exit_code() {
        let (waker, _) = galdeck_core::wake_channel();
        let (runner, reports) = StateRunner::new(waker);
        assert!(runner.spawn(enter(3, 7, "echo 'no such connection' >&2; exit 1")));
        assert_eq!(
            report(&reports),
            StateReport {
                home: home(3),
                seq: 7,
                kind: StateKind::Enter {
                    command: "echo 'no such connection' >&2; exit 1".into()
                },
                outcome: Outcome::Exited {
                    code: 1,
                    stdout_first_line: None,
                    stderr_first_line: Some("no such connection".into()),
                },
            }
        );
    }

    #[test]
    fn an_enter_job_that_outlives_its_window_is_launched_and_left_running() {
        let dir = scratch("states-launched");
        let pid = dir.join("pid");
        let (runner, reports) = runner(Duration::from_millis(200));
        let started = Instant::now();
        assert!(runner.spawn(enter(
            0,
            1,
            &format!("echo $$ > '{}'; sleep 30", pid.display())
        )));
        let launched = report(&reports);
        assert_eq!(launched.outcome, Outcome::Launched);
        assert!(started.elapsed() < Duration::from_secs(5));

        // Well past the window, it is still running, and the key is free for
        // its next command.
        let shell = pid_in(&pid);
        std::thread::sleep(Duration::from_millis(400));
        assert!(is_alive(shell), "an Enter job is never killed");
        assert!(runner.spawn(enter(0, 2, "true")));
        assert_eq!(report(&reports).seq, 2);

        // SAFETY: a plain system call; the group is the one the job's shell
        // leads, and the shell is still running, so the number is its own.
        unsafe { libc::kill(-shell, libc::SIGKILL) };
        assert!(eventually(Duration::from_secs(3), || !is_alive(shell)));
        assert!(
            reports.recv_timeout(Duration::from_millis(500)).is_err(),
            "a launched job's exit is not reported"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_status_job_gives_its_first_line_its_error_and_its_exit_code_in_the_c_locale() {
        let (runner, reports) = runner(ENTER_WINDOW);
        let command = "echo; echo \"  'performance' $LC_ALL \"; echo 'went wrong' >&2; exit 2";
        assert!(runner.spawn(status(5, 9, command)));
        let got = report(&reports);
        assert_eq!((got.home, got.seq), (home(5), 9));
        assert_eq!(
            got.outcome,
            Outcome::Exited {
                code: 2,
                stdout_first_line: Some("'performance' C.UTF-8".into()),
                stderr_first_line: Some("went wrong".into()),
            }
        );
    }

    #[test]
    fn a_status_job_that_hangs_is_killed_at_its_deadline() {
        let (waker, _) = galdeck_core::wake_channel();
        let (runner, reports) =
            StateRunner::with_timing(waker, ENTER_WINDOW, Duration::from_millis(200));
        assert!(runner.spawn(status(1, 1, "sleep 30 | cat")));
        assert_eq!(report(&reports).outcome, Outcome::TimedOut);
    }

    #[test]
    fn a_key_has_one_job_of_each_kind_at_a_time() {
        let dir = scratch("states-one-each");
        let gate = dir.join("go");
        let (runner, reports) = runner(ENTER_WINDOW);

        assert!(runner.spawn(enter(0, 1, &held(&dir.join("enter-0"), &gate))));
        assert!(!runner.spawn(enter(0, 2, "true")), "a second Enter");
        assert!(runner.spawn(enter(1, 3, "true")), "another key's Enter");
        assert!(runner.spawn(status(0, 4, &held(&dir.join("status-0"), &gate))));
        assert!(!runner.spawn(status(0, 5, "true")), "a second Status");
        assert_eq!(report(&reports).seq, 3);

        std::fs::write(&gate, "").unwrap();
        let mut seqs = [report(&reports).seq, report(&reports).seq];
        seqs.sort();
        assert_eq!(seqs, [1, 4]);

        // Each key is free again once its job has reported.
        assert!(runner.spawn(enter(0, 6, "true")));
        assert!(runner.spawn(status(0, 7, "true")));
        let mut seqs = [report(&reports).seq, report(&reports).seq];
        seqs.sort();
        assert_eq!(seqs, [6, 7]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn at_most_four_status_jobs_run_at_once_and_the_rest_wait_their_turn() {
        let dir = scratch("states-four");
        let gate = dir.join("go");
        let started = |key: u8| dir.join(format!("started-{key}"));
        let (runner, reports) = runner(ENTER_WINDOW);
        for key in 0..6 {
            assert!(runner.spawn(status(key, u64::from(key), &held(&started(key), &gate))));
        }
        assert!(eventually(Duration::from_secs(5), || (0..4)
            .all(|key| started(key).exists())));
        std::thread::sleep(Duration::from_millis(300));
        assert!(!started(4).exists() && !started(5).exists());

        // An Enter job does not wait for a place.
        assert!(runner.spawn(enter(9, 99, "true")));
        assert_eq!(report(&reports).seq, 99);

        std::fs::write(&gate, "").unwrap();
        let mut seqs: Vec<u64> = (0..6).map(|_| report(&reports).seq).collect();
        seqs.sort();
        assert_eq!(seqs, [0, 1, 2, 3, 4, 5]);
        assert!((0..6).all(|key| started(key).exists()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------ key cycles

    fn key(toml: &str) -> KeyConfig {
        toml::from_str(toml).expect("a key")
    }

    /// Three states, and a way to read which one the key is in.
    const POWER: &str = r#"
key = 0
label = "Power"
status = "powerprofilesctl get"
[[states]]
name = "power-saver"
label = "Saver"
[[states]]
name = "balanced"
label = "Balanced"
[[states]]
name = "performance"
label = "Performance"
"#;

    /// Two states and no way to read them.
    const LAMP: &str = r#"
key = 3
label = "Lamp"
[[states]]
name = "off"
[[states]]
name = "on"
label = "Lamp"
"#;

    fn ms(n: u64) -> Tick {
        Tick(n * 1000)
    }

    fn read_as(line: &str) -> Outcome {
        Outcome::Exited {
            code: 0,
            stdout_first_line: Some(line.into()),
            stderr_first_line: None,
        }
    }

    /// Start a read and answer it with `outcome` at `now`, as the engine
    /// would; whether the answer was taken.
    fn answer(
        cycle: &mut KeyCycle,
        cfg: &KeyConfig,
        seq: u64,
        outcome: Outcome,
        now: Tick,
    ) -> bool {
        cycle.reading(seq);
        let (found, last) = judge(cfg, &outcome, now);
        cycle.read(seq, found, last, now)
    }

    #[test]
    fn a_key_that_cannot_read_its_state_starts_in_its_first_and_one_that_can_in_none() {
        let lamp = KeyCycle::new(&key(LAMP));
        assert_eq!(lamp.shown.as_deref(), Some("off"));
        assert_eq!(lamp.settled.as_deref(), Some("off"));
        let power = KeyCycle::new(&key(POWER));
        assert_eq!(power.shown, None);
        assert!(!power.known);
    }

    #[test]
    fn taps_go_round_forwards_and_backwards_from_wherever_the_key_is() {
        let power = key(POWER);
        let next = |from: Option<&str>, forward| neighbour(&power, from, forward);
        assert_eq!(next(Some("balanced"), true).as_deref(), Some("performance"));
        assert_eq!(
            next(Some("performance"), true).as_deref(),
            Some("power-saver")
        );
        assert_eq!(
            next(Some("power-saver"), false).as_deref(),
            Some("performance")
        );
        // Not knowing where it is, or where it was having gone, the first.
        assert_eq!(next(None, true).as_deref(), Some("power-saver"));
        assert_eq!(next(None, false).as_deref(), Some("power-saver"));
        assert_eq!(next(Some("turbo"), true).as_deref(), Some("power-saver"));
        assert_eq!(neighbour(&key("key = 0"), None, true), None);
    }

    #[test]
    fn a_tap_while_a_command_runs_waits_and_only_the_last_one_is_entered() {
        let mut cycle = KeyCycle::new(&key(LAMP));
        assert!(cycle.press("on".into()), "nothing is running");
        cycle.begin(1, "on".into(), ms(0));
        // Two more taps while "on" runs: off, then on again, then off.
        assert!(!cycle.press("off".into()));
        assert!(!cycle.press("on".into()));
        assert!(!cycle.press("off".into()));
        assert_eq!(cycle.shown.as_deref(), Some("off"), "shown at once");
        assert_eq!(cycle.entered(1, true, ms(400)), Entered::Next("off".into()));
        assert_eq!(cycle.settled.as_deref(), Some("on"));

        // Tapped back to where the command left it: nothing more to enter.
        cycle.begin(2, "off".into(), ms(400));
        assert!(!cycle.press("on".into()));
        assert!(!cycle.press("off".into()));
        assert_eq!(cycle.entered(2, true, ms(800)), Entered::Settled);
        assert!(!cycle.queued);
        // An answer to a job the key is not waiting on changes nothing.
        assert_eq!(cycle.entered(2, false, ms(900)), Entered::Stale);
        assert_eq!(cycle.shown.as_deref(), Some("off"));
    }

    #[test]
    fn a_failed_command_takes_the_key_back_unless_it_was_moved_on_meanwhile() {
        let mut cycle = KeyCycle::new(&key(LAMP));
        cycle.press("on".into());
        cycle.begin(1, "on".into(), ms(0));
        assert_eq!(cycle.entered(1, false, ms(100)), Entered::Reverted);
        assert_eq!(cycle.shown.as_deref(), Some("off"));
        assert_eq!(cycle.badge(ms(100)), Some(Badge::Failed));
        assert_eq!(cycle.badge(ms(1100)), None, "the flash is over");

        // Moved on while it ran: what it was moved on to is entered, and
        // nothing is taken back.
        let mut cycle = KeyCycle::new(&key(POWER));
        cycle.press("power-saver".into());
        cycle.begin(1, "power-saver".into(), ms(0));
        cycle.press("balanced".into());
        assert_eq!(
            cycle.entered(1, false, ms(100)),
            Entered::Next("balanced".into())
        );
        assert_eq!(cycle.failed_until, None);
    }

    #[test]
    fn a_read_begun_before_a_change_or_while_a_command_runs_is_not_taken() {
        let power = key(POWER);
        let mut cycle = KeyCycle::new(&power);
        cycle.reading(1);
        // Someone sets the key while the read is under way.
        cycle.show("performance".into());
        let (found, last) = judge(&power, &read_as("balanced"), ms(10));
        assert!(!cycle.read(1, found, last, ms(10)));
        assert_eq!(cycle.shown.as_deref(), Some("performance"));
        assert_eq!(cycle.reading, None, "the read is over, taken or not");
        assert_eq!(cycle.last_read, None);

        // A command running: reads wait, and one already out is not taken.
        cycle.press("power-saver".into());
        cycle.begin(2, "power-saver".into(), ms(20));
        assert!(!cycle.may_read());
        assert!(!answer(&mut cycle, &power, 3, read_as("balanced"), ms(30)));
        assert_eq!(cycle.shown.as_deref(), Some("power-saver"));
        // Nor is an answer to a read that is not the one under way.
        cycle.entered(2, true, ms(40));
        cycle.reading(4);
        let (found, last) = judge(&power, &read_as("balanced"), ms(50));
        assert!(!cycle.read(5, found, last, ms(50)));
        assert_eq!(cycle.reading.map(|(seq, _)| seq), Some(4));
    }

    #[test]
    fn after_a_command_a_read_that_disagrees_needs_another_a_second_later() {
        let power = key(POWER);
        let mut cycle = KeyCycle::new(&power);
        cycle.press("performance".into());
        cycle.begin(1, "performance".into(), ms(0));
        assert_eq!(cycle.entered(1, true, ms(100)), Entered::Settled);
        cycle.expect_reads(ms(100));
        assert_eq!(cycle.verify, [ms(400), ms(1100), ms(3100)]);

        // Still switching: one read says otherwise, and is not believed.
        assert!(answer(&mut cycle, &power, 2, read_as("balanced"), ms(400)));
        assert_eq!(cycle.shown.as_deref(), Some("performance"));
        // Nor is a second too soon after it.
        assert!(answer(&mut cycle, &power, 3, read_as("balanced"), ms(900)));
        assert_eq!(cycle.shown.as_deref(), Some("performance"));
        // A second a second later is.
        assert!(answer(&mut cycle, &power, 4, read_as("balanced"), ms(1400)));
        assert_eq!(cycle.shown.as_deref(), Some("balanced"));
        assert!(cycle.known);

        // A read that agrees clears what disagreed before it.
        let mut cycle = KeyCycle::new(&power);
        cycle.press("performance".into());
        cycle.begin(1, "performance".into(), ms(0));
        cycle.entered(1, true, ms(0));
        cycle.expect_reads(ms(0));
        answer(&mut cycle, &power, 2, read_as("balanced"), ms(300));
        answer(&mut cycle, &power, 3, read_as("performance"), ms(1000));
        assert_eq!(cycle.disagree, None);
        answer(&mut cycle, &power, 4, read_as("balanced"), ms(2000));
        assert_eq!(cycle.shown.as_deref(), Some("performance"));

        // Past the grace, one read is enough.
        answer(&mut cycle, &power, 5, read_as("power-saver"), ms(3500));
        assert_eq!(cycle.shown.as_deref(), Some("power-saver"));
    }

    #[test]
    fn a_read_that_finds_nothing_keeps_the_state_and_says_it_is_not_sure() {
        let power = key(POWER);
        let mut cycle = KeyCycle::new(&power);
        assert!(answer(&mut cycle, &power, 1, read_as("'balanced'"), ms(0)));
        assert_eq!(cycle.shown.as_deref(), Some("balanced"));
        assert!(cycle.known && cycle.badge(ms(0)).is_none());

        let failed = Outcome::Exited {
            code: 1,
            stdout_first_line: Some("balanced".into()),
            stderr_first_line: Some("No such key\u{202e}".into()),
        };
        assert!(answer(&mut cycle, &power, 2, failed, ms(5000)));
        assert_eq!(cycle.shown.as_deref(), Some("balanced"));
        assert!(!cycle.known);
        assert_eq!(cycle.badge(ms(5000)), Some(Badge::Unknown));
        let last = cycle.last_read.clone().unwrap();
        assert_eq!(
            (last.output, last.error.as_deref(), last.ok),
            (None, Some("No such key"), false)
        );

        // What it printed, when that is none of the states, is said too.
        assert!(answer(&mut cycle, &power, 3, read_as("turbo"), ms(15000)));
        let last = cycle.last_read.clone().unwrap();
        assert_eq!((last.output.as_deref(), last.error), (Some("turbo"), None));
        assert_eq!(cycle.failures, 2);

        // A tap clears it: the key shows what it did.
        cycle.press("performance".into());
        assert_eq!(cycle.badge(ms(15000)), None);
        assert_eq!(cycle.failures, 0);
    }

    #[test]
    fn reads_that_keep_failing_are_spaced_out_up_to_a_minute() {
        let mut cycle = KeyCycle::default();
        let every = Duration::from_secs(5);
        assert_eq!(cycle.read_interval(every), every);
        cycle.failures = 1;
        assert_eq!(cycle.read_interval(every), Duration::from_secs(10));
        cycle.failures = 3;
        assert_eq!(cycle.read_interval(every), Duration::from_secs(40));
        cycle.failures = 40;
        assert_eq!(cycle.read_interval(every), MAX_READ_INTERVAL);
        // A key read less often than that already is left at its own pace.
        let slow = Duration::from_secs(120);
        assert_eq!(cycle.read_interval(slow), slow);
    }

    #[test]
    fn a_failure_outranks_a_running_command_which_outranks_an_unknown_state() {
        let mut cycle = KeyCycle::new(&key(LAMP));
        cycle.unknown = true;
        cycle.begin(1, "on".into(), ms(0));
        // A command that answers at once never shows as running.
        assert_eq!(cycle.badge(ms(100)), Some(Badge::Unknown));
        assert_eq!(cycle.badge(ms(250)), Some(Badge::Pending));
        cycle.failed_until = Some(ms(1250));
        assert_eq!(cycle.badge(ms(300)), Some(Badge::Failed));
        assert_eq!(cycle.badge(ms(1250)), Some(Badge::Pending));
    }

    #[test]
    fn each_timer_of_a_key_comes_due_once() {
        let mut cycle = KeyCycle::new(&key(POWER));
        cycle.begin(1, "balanced".into(), ms(0));
        assert_eq!(cycle.next_deadline(), Some(ms(250)));
        assert_eq!(cycle.due(ms(100)), Due::default());
        assert_eq!(
            cycle.due(ms(250)),
            Due {
                repaint: true,
                read: false
            }
        );
        assert_eq!(cycle.next_deadline(), None, "the cue is showing");
        assert_eq!(cycle.due(ms(300)), Due::default());

        cycle.entered(1, false, ms(500));
        assert_eq!(cycle.next_deadline(), Some(ms(1500)));
        cycle.expect_reads(ms(500));
        assert_eq!(cycle.next_deadline(), Some(ms(800)));
        assert_eq!(
            cycle.due(ms(1600)),
            Due {
                repaint: true,
                read: true
            }
        );
        // Two reads were due; one is enough, and the last is still to come.
        assert_eq!(cycle.verify, [ms(3500)]);
    }

    #[test]
    fn a_reload_keeps_a_state_by_its_name_and_forgets_one_that_went() {
        let mut cycle = KeyCycle::new(&key(POWER));
        cycle.show("balanced".into());
        cycle.known = true;
        cycle.unknown = true;
        // Renamed in capitals, and the reading dropped.
        let renamed = key(&POWER.replace("\"balanced\"", "\"Balanced\""));
        let mut kept = cycle.clone();
        kept.keep(&renamed);
        assert_eq!(kept.shown.as_deref(), Some("Balanced"));
        assert!(kept.known && kept.unknown);

        let without_status = key(&POWER.replace("status = \"powerprofilesctl get\"\n", ""));
        let mut kept = cycle.clone();
        kept.keep(&without_status);
        assert!(!kept.known && !kept.unknown, "it can no longer read");

        let gone = key(&POWER.replace("\"balanced\"", "\"quiet\""));
        let mut kept = cycle;
        kept.keep(&gone);
        assert_eq!((kept.shown, kept.settled), (None, None));

        // A command under way settles, once done, on its state as it is
        // spelt now, which is the one the key shows.
        let mut running = KeyCycle::new(&key(POWER));
        running.press("balanced".into());
        running.begin(1, "balanced".into(), ms(0));
        running.keep(&renamed);
        assert_eq!(running.entered(1, true, ms(100)), Entered::Settled);
        assert_eq!(running.settled, running.shown);
        assert_eq!(running.settled.as_deref(), Some("Balanced"));
    }

    #[test]
    fn a_key_shown_in_a_state_while_a_command_runs_stays_in_it() {
        let power = key(POWER);
        // The command works: the key stays where it was put.
        let mut cycle = KeyCycle::new(&power);
        cycle.press("performance".into());
        cycle.begin(1, "performance".into(), ms(0));
        cycle.show("balanced".into());
        assert_eq!(cycle.entered(1, true, ms(100)), Entered::Settled);
        assert_eq!(cycle.shown.as_deref(), Some("balanced"));
        assert_eq!(cycle.settled.as_deref(), Some("balanced"));

        // A later command that fails goes back there, not to what the one
        // before was entering.
        cycle.press("power-saver".into());
        cycle.begin(2, "power-saver".into(), ms(200));
        assert_eq!(cycle.entered(2, false, ms(300)), Entered::Reverted);
        assert_eq!(cycle.shown.as_deref(), Some("balanced"));

        // Shown while one that then fails runs: it stays there too.
        cycle.press("performance".into());
        cycle.begin(3, "performance".into(), ms(400));
        cycle.show("power-saver".into());
        assert_eq!(cycle.entered(3, false, ms(500)), Entered::Reverted);
        assert_eq!(cycle.shown.as_deref(), Some("power-saver"));
    }

    #[test]
    fn a_read_counts_only_when_it_exits_0_and_its_first_line_is_a_state() {
        let dark = key(r#"
key = 0
status = "gsettings get org.gnome.desktop.interface color-scheme"
[[states]]
name = "light"
match = ["'default'", "prefer-light"]
[[states]]
name = "dark"
match = ["\"prefer-dark\""]
"#);
        let state = |outcome: Outcome| judge(&dark, &outcome, ms(0)).0;
        let named = |name: &str| Found::State(name.into());
        // One pair of quotes off either side, and case ignored.
        assert_eq!(state(read_as("'prefer-dark'")), named("dark"));
        assert_eq!(state(read_as("PREFER-LIGHT")), named("light"));
        assert_eq!(state(read_as("default")), named("light"));
        assert_eq!(state(read_as("''prefer-dark''")), Found::Nothing);
        assert_eq!(state(read_as("turbo")), Found::Nothing);
        let printed_nothing = Outcome::Exited {
            code: 0,
            stdout_first_line: None,
            stderr_first_line: None,
        };
        assert_eq!(state(printed_nothing.clone()), Found::Nothing);
        assert_eq!(
            judge(&dark, &printed_nothing, ms(0)).1.error.as_deref(),
            Some("it printed nothing")
        );
        let (found, last) = judge(&dark, &Outcome::TimedOut, ms(0));
        assert_eq!(found, Found::Nothing);
        assert!(last.error.unwrap().contains("5 s"));
        let silent_failure = Outcome::Exited {
            code: 4,
            stdout_first_line: None,
            stderr_first_line: None,
        };
        assert_eq!(
            judge(&dark, &silent_failure, ms(0)).1.error.as_deref(),
            Some("it exited with status 4")
        );
    }

    #[test]
    fn the_screen_names_the_key_and_the_state_and_counts_three_or_more() {
        assert_eq!(
            state_text(&key(POWER), "performance"),
            "Power · Performance 3/3"
        );
        let lamp = key(LAMP);
        // A state without a label is called by its name, and so is one
        // labelled as the key is: the key's label is not said twice, and
        // the screen still says which state it is.
        assert_eq!(state_text(&lamp, "off"), "Lamp · off");
        assert_eq!(state_text(&lamp, "on"), "Lamp · on");
        assert_eq!(
            state_text(
                &key("key = 0\nlabel = \"On\"\n[[states]]\nname = \"on\""),
                "on"
            ),
            "On"
        );
        assert_eq!(
            state_text(
                &key("key = 0\n[[states]]\nname = \"a\"\n[[states]]\nname = \"b\""),
                "b"
            ),
            "b"
        );
    }

    #[test]
    fn a_failure_says_what_the_command_said_or_else_how_it_exited() {
        let said = Outcome::Exited {
            code: 1,
            stdout_first_line: None,
            stderr_first_line: Some("Failed to set power on:\torg.bluez.Error.Blocked".into()),
        };
        assert_eq!(
            failure_text(&key(LAMP), 3, &said),
            "Lamp: Failed to set power on: org.bluez.Error.Blocked"
        );
        let quiet = Outcome::Exited {
            code: 10,
            stdout_first_line: None,
            stderr_first_line: None,
        };
        assert_eq!(
            failure_text(&key("key = 5"), 5, &quiet),
            "Key 6: command failed (exit 10)"
        );
    }
}

//! Doing things outside the daemon: typing, scrolling, sound, media.
//!
//! Two workers, and the split is the point. Keystrokes go through a worker
//! that only ever writes to the virtual keyboard, so a chord is never stuck
//! behind a sound server or a media player taking its time to answer; typing
//! on the numpad page must feel like typing. Everything that asks another
//! process -- `wpctl` and `pw-dump`, a media player over D-Bus -- goes
//! through the other, with a deadline on every job.
//!
//! Both queues are bounded and fed with `try_send`, like the rotation runner:
//! a knob spun faster than the work can be done drops work rather than
//! replaying it after the hand has stopped. Keystrokes older than
//! [`STALE`] when their turn comes are dropped for the same reason.
//!
//! Results come back as [`ControlReport`]s on a channel with the engine's
//! waker, the way widget samples do, so the ring can show the level the sound
//! server actually settled on rather than the one the daemon guessed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use galdeck_core::Waker;

use crate::audio::{self, AudioTarget};
use crate::uinput::VirtualInput;
use crate::widgets::media::{MediaCommand, MediaControl, Outcome};

/// Depth of each queue. Deep enough for a fast spin's worth of events,
/// shallow enough that a backlog is dropped rather than paid off late.
const QUEUE_DEPTH: usize = 16;
/// A keystroke that waited longer than this is dropped: typed half a second
/// late it lands somewhere the user has already moved on from.
const STALE: Duration = Duration::from_millis(300);
/// Pause between repeats of a chord for one knob event, so a compositor that
/// coalesces fast input sees each one.
const REPEAT_GAP: Duration = Duration::from_millis(10);
/// The loudest the sound built-ins will go, as a percentage. Hearing is not
/// worth a setting.
pub const CEILING: f64 = 100.0;

/// Work for the keystroke worker.
#[derive(Debug)]
pub enum InputJob {
    /// Create the virtual device now, so it has settled before the first
    /// keystroke is needed.
    Prepare,
    Chord {
        codes: Vec<u16>,
        times: u8,
        queued: Instant,
    },
    Wheel {
        horizontal: bool,
        notches: i32,
        modifiers: Vec<u16>,
        queued: Instant,
    },
}

/// Work for the services worker.
#[derive(Debug, Clone, PartialEq)]
pub enum ServiceJob {
    /// Change a volume by this many percentage points.
    Volume { target: AudioTarget, delta: f64 },
    /// Toggle mute, or set it.
    Mute {
        target: AudioTarget,
        to: Option<bool>,
    },
    /// Read a level without changing it.
    Read { target: AudioTarget },
    Media {
        command: MediaCommand,
        target: Option<String>,
    },
    /// Make the output `step` places on from the default the default.
    /// `order` is the `outputs` list from galdeck.toml, since the worker has
    /// no config of its own; `from` is the knob that asked, if one did, and
    /// only comes back in the report.
    Output {
        step: i8,
        order: Vec<String>,
        from: Option<u8>,
    },
    /// Make the output `name` names the default.
    OutputSet {
        name: String,
        order: Vec<String>,
        from: Option<u8>,
    },
    /// Move an app's volume by this many percentage points: the app `app`
    /// names, else the one playing.
    AppVolume {
        app: Option<String>,
        delta: f64,
        from: Option<u8>,
    },
    /// Mute an app, or unmute it.
    AppMute {
        app: Option<String>,
        from: Option<u8>,
    },
    /// Move a knob on to the app after `current`.
    AppStep {
        current: Option<String>,
        from: Option<u8>,
    },
}

/// What happened.
#[derive(Debug, Clone)]
pub enum ControlReport {
    /// A device's level after changing it: the answer to whichever knob or
    /// key asked for the change.
    Volume {
        target: AudioTarget,
        level: audio::Level,
    },
    /// A device's level as read without changing it, or why it could not be
    /// read. Nobody is waiting on a read, so the deck corrects what it shows
    /// without putting anything on the screen: a read that lands during a
    /// spin must not pull the ring back to where the knob started.
    Read {
        target: AudioTarget,
        level: Result<audio::Level, String>,
    },
    /// Changing a device's level failed, so what the deck knew of it no
    /// longer holds: a mute key that kept showing "muted" over a microphone
    /// that is live would be worse than showing nothing.
    LevelFailed {
        target: AudioTarget,
        what: &'static str,
        reason: String,
    },
    Media(Outcome),
    /// Where an output switch landed: the output now the default, its place
    /// among those switched between, and its level if it could be read.
    Output {
        display: String,
        index: usize,
        count: usize,
        level: Option<audio::Level>,
        from: Option<u8>,
    },
    /// The app a per-app job acted on, and its level after it. `app` is what
    /// the app is known by, for a knob to remember; `display` is its name to
    /// show.
    App {
        app: String,
        display: String,
        index: usize,
        count: usize,
        level: audio::Level,
        from: Option<u8>,
    },
    /// A per-app job found nothing to act on: no app is playing (`app` None),
    /// or the one asked for has no sound. `app` is only for saying which.
    NoApp {
        app: Option<String>,
        from: Option<u8>,
    },
    Failed {
        what: &'static str,
        reason: String,
    },
}

/// Hands work to the two workers.
pub struct ControlHost {
    input: SyncSender<InputJob>,
    services: SyncSender<ServiceJob>,
    /// What the virtual device is up to, in the words the status reports.
    input_state: Arc<Mutex<String>>,
    /// Whether the virtual device may exist. Shared with its worker so a
    /// reload can turn it on or off without a restart.
    allowed: Arc<AtomicBool>,
    /// Push-to-talk's wanted microphone state, for its own worker.
    talk: Arc<Talk>,
}

/// The microphone state push-to-talk wants, and a way to wake its worker.
///
/// Not a queue: only the latest wish matters, it can never be dropped for a
/// full queue, and it never waits behind a media player that has stopped
/// answering. An open microphone that was meant to close is the one failure
/// here that is worse than doing nothing.
#[derive(Default)]
struct Talk {
    /// `Some(true)` to mute, `Some(false)` to open; taken by the worker.
    wanted: Mutex<Option<bool>>,
    ready: Condvar,
}

/// How often push-to-talk tries again when the sound server did not take a
/// change, and how many times, before saying so.
const TALK_RETRY: Duration = Duration::from_millis(200);
const TALK_ATTEMPTS: u32 = 5;

impl ControlHost {
    /// Start the workers. With `virtual_input` off the keystroke worker never
    /// opens /dev/uinput.
    pub fn new(waker: Waker, virtual_input: bool) -> (Self, Receiver<ControlReport>) {
        let (reports_tx, reports_rx) = sync_channel::<ControlReport>(QUEUE_DEPTH);
        let allowed = Arc::new(AtomicBool::new(virtual_input));
        let talk = Arc::new(Talk::default());
        {
            let talk = Arc::clone(&talk);
            let reports = reports_tx.clone();
            let waker = waker.clone();
            std::thread::Builder::new()
                .name("galdeck-talk".into())
                .spawn(move || talk_worker(talk, reports, waker))
                .expect("spawning the push-to-talk worker");
        }
        let input_state = Arc::new(Mutex::new(if virtual_input {
            "unused".to_string()
        } else {
            "off".to_string()
        }));

        let (input_tx, input_rx) = sync_channel::<InputJob>(QUEUE_DEPTH);
        {
            let reports = reports_tx.clone();
            let waker = waker.clone();
            let state = Arc::clone(&input_state);
            let allowed = Arc::clone(&allowed);
            std::thread::Builder::new()
                .name("galdeck-keys".into())
                .spawn(move || input_worker(input_rx, allowed, state, reports, waker))
                .expect("spawning the keystroke worker");
        }

        let (services_tx, services_rx) = sync_channel::<ServiceJob>(QUEUE_DEPTH);
        std::thread::Builder::new()
            .name("galdeck-services".into())
            .spawn(move || services_worker(services_rx, reports_tx, waker))
            .expect("spawning the services worker");

        (
            Self {
                input: input_tx,
                services: services_tx,
                input_state,
                allowed,
                talk,
            },
            reports_rx,
        )
    }

    /// Queue keystroke work. False when the queue is full.
    pub fn input(&self, job: InputJob) -> bool {
        match self.input.try_send(job) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                log::debug!("keystroke queue full, dropping");
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// Open or close the microphone for push-to-talk. Never dropped.
    pub fn talk(&self, live: bool) {
        if let Ok(mut wanted) = self.talk.wanted.lock() {
            *wanted = Some(!live);
            self.talk.ready.notify_one();
        }
    }

    /// Allow or forbid the virtual device, as a reloaded config says.
    /// Forbidding it destroys the device if there is one.
    pub fn set_virtual_input(&self, allowed: bool) {
        if self.allowed.swap(allowed, Ordering::Relaxed) != allowed {
            // Wakes the worker, which applies the change.
            self.input(InputJob::Prepare);
        }
    }

    /// Queue sound or media work. False when the queue is full.
    pub fn service(&self, job: ServiceJob) -> bool {
        match self.services.try_send(job) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                log::debug!("services queue full, dropping");
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// `ready`, `waiting`, `unused`, `off` or `unavailable: <why>`.
    pub fn virtual_input_state(&self) -> String {
        self.input_state
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|_| "unavailable: worker failed".into())
    }
}

fn report(reports: &SyncSender<ControlReport>, waker: &Waker, report: ControlReport) {
    if reports.try_send(report).is_ok() {
        waker.notify();
    }
}

/// Why the virtual device could not be opened, in terms of what to do.
fn explain(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => {
            "no permission to use /dev/uinput; install udev/71-galdeck-uinput.rules and log in again"
                .into()
        }
        std::io::ErrorKind::NotFound => "this system has no /dev/uinput".into(),
        _ => error.to_string(),
    }
}

/// Carry out push-to-talk's wishes, newest first, retrying what the sound
/// server did not take.
fn talk_worker(talk: Arc<Talk>, reports: SyncSender<ControlReport>, waker: Waker) {
    loop {
        let muted = {
            let Ok(mut wanted) = talk.wanted.lock() else {
                return;
            };
            loop {
                if let Some(muted) = wanted.take() {
                    break muted;
                }
                wanted = match talk.ready.wait(wanted) {
                    Ok(wanted) => wanted,
                    Err(_) => return,
                };
            }
        };
        let mut last_error = None;
        for attempt in 0..TALK_ATTEMPTS {
            // A newer wish replaces this one; the loop above picks it up.
            if attempt > 0 {
                std::thread::sleep(TALK_RETRY);
                if talk.wanted.lock().map(|w| w.is_some()).unwrap_or(false) {
                    last_error = None;
                    break;
                }
            }
            match audio::set_mute(&AudioTarget::Input, muted) {
                Ok(level) => {
                    report(
                        &reports,
                        &waker,
                        ControlReport::Volume {
                            target: AudioTarget::Input,
                            level,
                        },
                    );
                    last_error = None;
                    break;
                }
                Err(e) => last_error = Some(e.to_string()),
            }
        }
        if let Some(reason) = last_error {
            log::warn!("push-to-talk could not change the microphone: {reason}");
            report(
                &reports,
                &waker,
                ControlReport::LevelFailed {
                    target: AudioTarget::Input,
                    what: if muted {
                        "muting the microphone"
                    } else {
                        "opening the microphone"
                    },
                    reason,
                },
            );
        }
    }
}

fn input_worker(
    jobs: Receiver<InputJob>,
    allowed: Arc<AtomicBool>,
    state: Arc<Mutex<String>>,
    reports: SyncSender<ControlReport>,
    waker: Waker,
) {
    let set_state = |text: String| {
        if let Ok(mut state) = state.lock() {
            *state = text;
        }
    };
    let mut device: Option<VirtualInput> = None;
    // Tried once per run: a failure to open is not going to fix itself on
    // the next keystroke, and trying again would log it on every one.
    let mut tried = false;
    let codes = galdeck_model::keys::all_codes();

    let open = |device: &mut Option<VirtualInput>, tried: &mut bool| {
        if device.is_some() || *tried || !allowed.load(Ordering::Relaxed) {
            return;
        }
        *tried = true;
        match VirtualInput::open("galdeck virtual input", &codes) {
            Ok(created) => {
                set_state("waiting".into());
                *device = Some(created);
            }
            Err(e) => {
                let why = explain(&e);
                log::warn!("keystroke and scroll actions are off: {why}");
                set_state(format!("unavailable: {why}"));
                report(
                    &reports,
                    &waker,
                    ControlReport::Failed {
                        what: "virtual input",
                        reason: why,
                    },
                );
            }
        }
    };

    // Whether the device has been said to be ready yet. Until then the
    // worker wakes when it settles, so the status stops saying "waiting"
    // without needing a keystroke to notice.
    let mut announced = false;
    loop {
        let settling = device
            .as_ref()
            .filter(|_| !announced)
            .map(|input| input.ready_at().saturating_duration_since(Instant::now()));
        let job = match settling {
            Some(wait) => match jobs.recv_timeout(wait) {
                Ok(job) => job,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    set_state("ready".into());
                    announced = true;
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match jobs.recv() {
                Ok(job) => job,
                Err(_) => break,
            },
        };
        // Turned off by a reload: the device goes, and so does anything
        // waiting for it. Turned back on: it may be tried again.
        if !allowed.load(Ordering::Relaxed) {
            if device.take().is_some() {
                log::info!("virtual input turned off; the virtual keyboard is gone");
            }
            set_state("off".into());
            tried = false;
            announced = false;
            continue;
        } else if !tried && device.is_none() && matches!(job, InputJob::Prepare) {
            set_state("unused".into());
        }
        let queued = match &job {
            InputJob::Prepare => {
                open(&mut device, &mut tried);
                continue;
            }
            InputJob::Chord { queued, .. } | InputJob::Wheel { queued, .. } => *queued,
        };
        if queued.elapsed() > STALE {
            log::debug!("dropping a keystroke that waited {:?}", queued.elapsed());
            continue;
        }
        open(&mut device, &mut tried);
        let Some(input) = device.as_mut() else {
            continue;
        };
        let result = match job {
            InputJob::Chord { codes, times, .. } => {
                // Keypad digits type digits only with Num Lock on. When the
                // desktop has said it is off, turn it on first.
                if codes
                    .iter()
                    .any(|c| galdeck_model::keys::needs_num_lock(*c))
                    && input.numlock() == Some(false)
                {
                    let _ = input.chord(&[69]);
                }
                let mut result = Ok(());
                for time in 0..times.max(1) {
                    if time > 0 {
                        std::thread::sleep(REPEAT_GAP);
                    }
                    result = input.chord(&codes);
                    if result.is_err() {
                        break;
                    }
                }
                result
            }
            InputJob::Wheel {
                horizontal,
                notches,
                modifiers,
                ..
            } => input.wheel(horizontal, notches, &modifiers),
            InputJob::Prepare => Ok(()),
        };
        if !announced && Instant::now() >= input.ready_at() {
            set_state("ready".into());
            announced = true;
        }
        if let Err(e) = result {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                log::debug!("keystrokes arriving faster than the limit; dropping one");
            } else {
                log::warn!("writing to the virtual keyboard: {e}");
                report(
                    &reports,
                    &waker,
                    ControlReport::Failed {
                        what: "keystroke",
                        reason: e.to_string(),
                    },
                );
            }
        }
    }
}

/// Merge queued work that can be done as one: consecutive volume changes to
/// the same device add up to one `wpctl` call, and consecutive seeks on the
/// same player to one D-Bus call. An eight-detent spin is then one process,
/// not eight, and lands as one step the size of the spin.
///
/// Output switches are the exception: consecutive ones are never added up,
/// only the last is kept. Eight steps round three outputs land on whichever
/// the remainder says, which could be the television, and each hop moves
/// every stream on the machine; the last one asked for is the one meant.
pub fn coalesce(jobs: Vec<ServiceJob>) -> Vec<ServiceJob> {
    let mut merged: Vec<ServiceJob> = Vec::with_capacity(jobs.len());
    for job in jobs {
        if switches_output(&job) && merged.last().is_some_and(switches_output) {
            merged.pop();
        }
        // Reading a device twice with only reads between says nothing new.
        // The level poll queues its reads every two seconds whatever the
        // worker is doing, so behind a sound server taking its time to answer
        // they pile up, and read one by one they would never drain: the
        // queue would fill, and the volume change or media key queued next
        // would be dropped.
        if matches!(job, ServiceJob::Read { .. })
            && merged
                .iter()
                .rev()
                .take_while(|queued| matches!(queued, ServiceJob::Read { .. }))
                .any(|queued| *queued == job)
        {
            continue;
        }
        match (merged.last_mut(), &job) {
            (
                Some(ServiceJob::Volume { target, delta }),
                ServiceJob::Volume {
                    target: next,
                    delta: more,
                },
            ) if target == next => *delta += more,
            (
                Some(ServiceJob::Media {
                    command: MediaCommand::Seek(offset),
                    target,
                }),
                ServiceJob::Media {
                    command: MediaCommand::Seek(more),
                    target: next,
                },
            ) if target == next => *offset = offset.saturating_add(*more),
            // Only for the same app asked for by the same knob: the report
            // goes back to whoever asked.
            (
                Some(ServiceJob::AppVolume { app, delta, from }),
                ServiceJob::AppVolume {
                    app: next,
                    delta: more,
                    from: by,
                },
            ) if app == next && from == by => *delta += more,
            // Reading straight after anything else for the same device adds
            // nothing: every change reports the level it left.
            (Some(previous), ServiceJob::Read { target }) if changes(previous, target) => {}
            _ => merged.push(job),
        }
    }
    merged
}

/// Whether a job changes this device's level, and so reports it anyway.
fn changes(job: &ServiceJob, device: &AudioTarget) -> bool {
    matches!(job, ServiceJob::Volume { target, .. } | ServiceJob::Mute { target, .. } if target == device)
}

/// Whether a job changes which output is the default.
fn switches_output(job: &ServiceJob) -> bool {
    matches!(
        job,
        ServiceJob::Output { .. } | ServiceJob::OutputSet { .. }
    )
}

fn services_worker(jobs: Receiver<ServiceJob>, reports: SyncSender<ControlReport>, waker: Waker) {
    let mut media = MediaControl::default();
    while let Ok(first) = jobs.recv() {
        let mut batch = vec![first];
        while let Ok(more) = jobs.try_recv() {
            batch.push(more);
        }
        for job in coalesce(batch) {
            let done = match job {
                ServiceJob::Volume { target, delta } => {
                    let result = audio::adjust(&target, delta, CEILING);
                    changed(target, "volume", result)
                }
                ServiceJob::Mute { target, to } => {
                    let result = match to {
                        Some(muted) => audio::set_mute(&target, muted),
                        None => audio::toggle_mute(&target),
                    };
                    changed(target, "mute", result)
                }
                // Not logged here: a poll behind a mute key reads every two
                // seconds, and the engine says once that it cannot.
                ServiceJob::Read { target } => ControlReport::Read {
                    level: audio::read(&target).map_err(|e| e.to_string()),
                    target,
                },
                ServiceJob::Media { command, target } => {
                    match media.send(command, target.as_deref()) {
                        Ok(outcome) => ControlReport::Media(outcome),
                        Err(reason) => {
                            log::warn!("media: {reason}");
                            ControlReport::Failed {
                                what: "media",
                                reason,
                            }
                        }
                    }
                }
                // One deadline for everything each of these runs: a dump,
                // then a process per stream, then a read. A deadline apiece
                // would let one app with many streams, on a sound server that
                // has stopped answering, hold every knob and key queued
                // behind it for as many times two seconds.
                ServiceJob::Output { step, order, from } => {
                    let until = Instant::now() + audio::DEADLINE;
                    switched(audio::switch_output(step, &order, until), from)
                }
                ServiceJob::OutputSet { name, order, from } => {
                    let until = Instant::now() + audio::DEADLINE;
                    switched(audio::set_output(&name, &order, until), from)
                }
                ServiceJob::AppVolume { app, delta, from } => {
                    let until = Instant::now() + audio::DEADLINE;
                    acted(
                        audio::adjust_app(app.as_deref(), delta, CEILING, until),
                        from,
                    )
                }
                ServiceJob::AppMute { app, from } => {
                    let until = Instant::now() + audio::DEADLINE;
                    acted(audio::toggle_app_mute(app.as_deref(), until), from)
                }
                ServiceJob::AppStep { current, from } => {
                    let until = Instant::now() + audio::DEADLINE;
                    acted(audio::next_app(current.as_deref(), until), from)
                }
            };
            report(&reports, &waker, done);
        }
    }
}

/// Where an output switch landed, or why it did not, for the knob that asked.
fn switched(
    result: Result<audio::OutputOutcome, audio::AudioError>,
    from: Option<u8>,
) -> ControlReport {
    match result {
        Ok(outcome) => ControlReport::Output {
            display: outcome.display,
            index: outcome.index,
            count: outcome.count,
            level: outcome.level,
            from,
        },
        Err(e) => {
            let reason = e.to_string();
            log::warn!("output: {reason}");
            ControlReport::Failed {
                what: "output",
                reason,
            }
        }
    }
}

/// Which app a per-app job acted on, or why it did nothing, for the knob
/// that asked.
fn acted(result: Result<audio::AppOutcome, audio::AudioError>, from: Option<u8>) -> ControlReport {
    match result {
        Ok(audio::AppOutcome::Found {
            app,
            display,
            index,
            count,
            level,
        }) => ControlReport::App {
            app,
            display,
            index,
            count,
            level,
            from,
        },
        Ok(audio::AppOutcome::NoApp { app }) => ControlReport::NoApp { app, from },
        Err(e) => {
            let reason = e.to_string();
            log::warn!("app volume: {reason}");
            ControlReport::Failed {
                what: "app volume",
                reason,
            }
        }
    }
}

/// What [`AudioTargets`] finds: every output and app, or why they could not
/// be listed.
pub type Found = Result<crate::pipewire::Targets, String>;

/// How long a list of outputs and apps is good for: long enough that an
/// editor asking for each of its target fields in turn runs one `pw-dump`,
/// short enough that headphones plugged in a moment ago are on the list.
const TARGETS_FRESH: Duration = Duration::from_secs(1);

/// The outputs and apps there are, for an editor to offer as targets.
///
/// Found on a thread of its own, since `pw-dump` takes a tenth of a second
/// here and longer on a busy machine, and never on the engine's. At most one
/// runs at a time: whoever asks while one is running waits for its answer
/// rather than starting another, and an answer is given again to anyone who
/// asks within a second of it.
#[derive(Clone)]
pub struct AudioTargets {
    state: Arc<Mutex<Lookup>>,
    find: fn() -> Found,
    fresh: Duration,
}

type Answer = Box<dyn FnOnce(Found) + Send>;

#[derive(Default)]
struct Lookup {
    last: Option<(Instant, Found)>,
    running: bool,
    waiting: Vec<Answer>,
}

impl Default for AudioTargets {
    fn default() -> Self {
        Self::with(
            || audio::audio_targets(Instant::now() + audio::DEADLINE).map_err(|e| e.to_string()),
            TARGETS_FRESH,
        )
    }
}

impl AudioTargets {
    fn with(find: fn() -> Found, fresh: Duration) -> Self {
        Self {
            state: Arc::new(Mutex::new(Lookup::default())),
            find,
            fresh,
        }
    }

    /// Call `answer` with the outputs and apps, now if a recent answer is at
    /// hand, else from the thread that finds them.
    pub fn ask(&self, answer: impl FnOnce(Found) + Send + 'static) {
        let Ok(mut state) = self.state.lock() else {
            answer(Err("the audio target lookup failed".into()));
            return;
        };
        if let Some((at, found)) = &state.last {
            if at.elapsed() < self.fresh {
                let found = found.clone();
                drop(state);
                answer(found);
                return;
            }
        }
        state.waiting.push(Box::new(answer));
        if state.running {
            return;
        }
        state.running = true;
        drop(state);

        let lookup = Arc::clone(&self.state);
        let find = self.find;
        let spawned = std::thread::Builder::new()
            .name("galdeck-targets".into())
            .spawn(move || {
                // A lookup that panicked must still answer and stand down:
                // left running, every later question would wait on it for
                // as long as the daemon runs.
                let found = std::panic::catch_unwind(find)
                    .unwrap_or_else(|_| Err("listing the outputs and apps failed".into()));
                settle_lookup(&lookup, Some((Instant::now(), found.clone())), found);
            });
        if let Err(e) = spawned {
            log::warn!("listing audio targets: {e}");
            settle_lookup(&self.state, None, Err(e.to_string()));
        }
    }
}

/// Answer everyone waiting on a lookup, and remember what it found.
fn settle_lookup(lookup: &Mutex<Lookup>, last: Option<(Instant, Found)>, found: Found) {
    let waiting = match lookup.lock() {
        Ok(mut state) => {
            state.running = false;
            if last.is_some() {
                state.last = last;
            }
            std::mem::take(&mut state.waiting)
        }
        Err(_) => return,
    };
    // Outside the lock: an answer is a send on someone's channel, and
    // nothing it does should hold up the next question.
    for answer in waiting {
        answer(found.clone());
    }
}

/// What changing a device's level came to. A change that failed leaves the
/// level unknown, and says so.
fn changed(
    target: AudioTarget,
    what: &'static str,
    result: Result<audio::Level, audio::AudioError>,
) -> ControlReport {
    match result {
        Ok(level) => ControlReport::Volume { target, level },
        Err(e) => {
            let reason = e.to_string();
            log::warn!("{what}: {reason}");
            ControlReport::LevelFailed {
                target,
                what,
                reason,
            }
        }
    }
}

/// Which sound tool is on the PATH, without running a shell to find out.
pub fn audio_tool() -> &'static str {
    let on_path = |program: &str| {
        std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
    };
    if on_path("wpctl") {
        "wpctl"
    } else if on_path("pactl") {
        "pactl"
    } else {
        "missing"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(target: AudioTarget, delta: f64) -> ServiceJob {
        ServiceJob::Volume { target, delta }
    }

    #[test]
    fn a_spin_becomes_one_volume_change() {
        let merged = coalesce(vec![
            volume(AudioTarget::Output, 2.0),
            volume(AudioTarget::Output, 2.0),
            volume(AudioTarget::Output, -2.0),
            volume(AudioTarget::Output, 2.0),
        ]);
        assert_eq!(merged, vec![volume(AudioTarget::Output, 4.0)]);
    }

    #[test]
    fn different_devices_and_other_work_keep_their_order() {
        let mute = ServiceJob::Mute {
            target: AudioTarget::Output,
            to: None,
        };
        let merged = coalesce(vec![
            volume(AudioTarget::Output, 2.0),
            volume(AudioTarget::Input, 2.0),
            mute.clone(),
            volume(AudioTarget::Output, 2.0),
        ]);
        assert_eq!(
            merged,
            vec![
                volume(AudioTarget::Output, 2.0),
                volume(AudioTarget::Input, 2.0),
                mute,
                volume(AudioTarget::Output, 2.0),
            ]
        );
    }

    #[test]
    fn seeks_on_one_player_add_up() {
        let seek = |us: i64| ServiceJob::Media {
            command: MediaCommand::Seek(us),
            target: None,
        };
        assert_eq!(coalesce(vec![seek(5), seek(5), seek(-2)]), vec![seek(8)]);
    }

    #[test]
    fn a_read_after_a_change_is_redundant() {
        let merged = coalesce(vec![
            volume(AudioTarget::Output, 2.0),
            ServiceJob::Read {
                target: AudioTarget::Output,
            },
            ServiceJob::Read {
                target: AudioTarget::Input,
            },
        ]);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn reads_that_piled_up_are_each_read_once() {
        let read = |target| ServiceJob::Read { target };
        let merged = coalesce(vec![
            read(AudioTarget::Output),
            read(AudioTarget::Input),
            read(AudioTarget::Output),
            read(AudioTarget::Input),
            read(AudioTarget::Output),
        ]);
        assert_eq!(
            merged,
            vec![read(AudioTarget::Output), read(AudioTarget::Input)]
        );
        // A change between two reads is not a read, so the second still
        // says where the device is now.
        let mute = ServiceJob::Mute {
            target: AudioTarget::Input,
            to: None,
        };
        let merged = coalesce(vec![
            read(AudioTarget::Output),
            mute.clone(),
            read(AudioTarget::Output),
        ]);
        assert_eq!(
            merged,
            vec![read(AudioTarget::Output), mute, read(AudioTarget::Output)]
        );
    }

    fn output(step: i8, from: Option<u8>) -> ServiceJob {
        ServiceJob::Output {
            step,
            order: Vec::new(),
            from,
        }
    }

    fn app_volume(app: Option<&str>, delta: f64, from: Option<u8>) -> ServiceJob {
        ServiceJob::AppVolume {
            app: app.map(String::from),
            delta,
            from,
        }
    }

    #[test]
    fn a_run_of_output_switches_keeps_only_the_last_and_never_adds_them_up() {
        let set = ServiceJob::OutputSet {
            name: "Headphones".into(),
            order: Vec::new(),
            from: None,
        };
        assert_eq!(
            coalesce(vec![
                output(1, Some(0)),
                output(1, Some(0)),
                output(1, Some(0))
            ]),
            vec![output(1, Some(0))]
        );
        assert_eq!(
            coalesce(vec![output(1, Some(0)), set.clone(), output(-1, Some(1))]),
            vec![output(-1, Some(1))]
        );
        // Only a run: a volume change in between is for the output the first
        // switch lands on, and the switch after it still happens.
        assert_eq!(
            coalesce(vec![
                output(1, None),
                volume(AudioTarget::Output, 2.0),
                output(1, None),
            ]),
            vec![
                output(1, None),
                volume(AudioTarget::Output, 2.0),
                output(1, None),
            ]
        );
        // A read after a switch still says where the new output is.
        let read = ServiceJob::Read {
            target: AudioTarget::Output,
        };
        assert_eq!(coalesce(vec![set.clone(), read.clone()]), vec![set, read]);
    }

    #[test]
    fn a_spin_on_one_app_becomes_one_change_and_other_apps_keep_theirs() {
        assert_eq!(
            coalesce(vec![
                app_volume(None, 2.0, Some(0)),
                app_volume(None, 2.0, Some(0)),
                app_volume(None, -1.0, Some(0)),
            ]),
            vec![app_volume(None, 3.0, Some(0))]
        );
        // Another app, or the same app asked for by another knob, is a
        // change of its own: its report goes somewhere else.
        let apart = vec![
            app_volume(Some("spotify"), 2.0, Some(0)),
            app_volume(Some("firefox"), 2.0, Some(0)),
            app_volume(Some("firefox"), 2.0, Some(1)),
            app_volume(Some("firefox"), 2.0, None),
        ];
        assert_eq!(coalesce(apart.clone()), apart);
        // Mute and moving on to the next app are never merged.
        let mute = ServiceJob::AppMute {
            app: None,
            from: Some(0),
        };
        let step = ServiceJob::AppStep {
            current: None,
            from: Some(0),
        };
        let separate = vec![mute.clone(), mute, step.clone(), step];
        assert_eq!(coalesce(separate.clone()), separate);
    }

    #[test]
    fn an_output_switch_reports_where_it_landed_to_the_knob_that_asked() {
        let level = audio::Level {
            percent: 64.0,
            muted: false,
        };
        let landed = switched(
            Ok(audio::OutputOutcome {
                display: "Headphones".into(),
                index: 1,
                count: 2,
                level: Some(level),
                changed: true,
            }),
            Some(1),
        );
        assert!(
            matches!(
                &landed,
                ControlReport::Output {
                    display,
                    index: 1,
                    count: 2,
                    level: Some(_),
                    from: Some(1),
                } if display == "Headphones"
            ),
            "{landed:?}"
        );
        let failed = switched(
            Err(audio::AudioError::Failed(
                "Headphones did not become the default output".into(),
            )),
            Some(1),
        );
        assert!(
            matches!(&failed, ControlReport::Failed { what: "output", .. }),
            "{failed:?}"
        );
    }

    #[test]
    fn an_app_job_reports_the_app_it_found_or_that_there_was_none() {
        let level = audio::Level {
            percent: 40.0,
            muted: false,
        };
        let found = acted(
            Ok(audio::AppOutcome::Found {
                app: "spotify".into(),
                display: "Spotify".into(),
                index: 0,
                count: 3,
                level,
            }),
            Some(0),
        );
        assert!(
            matches!(
                &found,
                ControlReport::App { app, display, count: 3, from: Some(0), .. }
                    if app == "spotify" && display == "Spotify"
            ),
            "{found:?}"
        );
        let none = acted(Ok(audio::AppOutcome::NoApp { app: None }), None);
        assert!(
            matches!(
                none,
                ControlReport::NoApp {
                    app: None,
                    from: None
                }
            ),
            "{none:?}"
        );
        let failed = acted(Err(audio::AudioError::TimedOut), Some(0));
        assert!(
            matches!(
                &failed,
                ControlReport::Failed {
                    what: "app volume",
                    ..
                }
            ),
            "{failed:?}"
        );
    }

    /// How many times the fake lookup below has run.
    static LOOKUPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn slow_lookup() -> Found {
        LOOKUPS.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(100));
        Ok(crate::pipewire::Targets::default())
    }

    #[test]
    fn audio_targets_are_looked_up_once_for_everyone_asking_at_the_same_time() {
        let targets = AudioTargets::with(slow_lookup, Duration::from_secs(60));
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..5 {
            let tx = tx.clone();
            targets.ask(move |found| {
                let _ = tx.send(found.is_ok());
            });
        }
        for _ in 0..5 {
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(true));
        }
        assert_eq!(LOOKUPS.load(Ordering::SeqCst), 1);

        // Asked again while the answer is fresh, it is answered at once from
        // what was found.
        let tx = tx.clone();
        targets.ask(move |found| {
            let _ = tx.send(found.is_ok());
        });
        assert_eq!(rx.try_recv(), Ok(true));
        assert_eq!(LOOKUPS.load(Ordering::SeqCst), 1);
    }

    static FAILED_LOOKUPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    #[test]
    fn an_old_answer_is_looked_up_again() {
        fn failing() -> Found {
            FAILED_LOOKUPS.fetch_add(1, Ordering::SeqCst);
            Err("pw-dump is not installed".into())
        }
        let targets = AudioTargets::with(failing, Duration::ZERO);
        for _ in 0..2 {
            let (tx, rx) = std::sync::mpsc::channel();
            targets.ask(move |found| {
                let _ = tx.send(found);
            });
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(5)),
                Ok(Err("pw-dump is not installed".to_string()))
            );
        }
        assert_eq!(FAILED_LOOKUPS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_change_that_failed_says_which_device_it_leaves_unknown() {
        let level = audio::Level {
            percent: 40.0,
            muted: true,
        };
        assert!(matches!(
            changed(AudioTarget::Input, "mute", Ok(level)),
            ControlReport::Volume {
                target: AudioTarget::Input,
                level: audio::Level { muted: true, .. },
            }
        ));
        let failed = changed(
            AudioTarget::Input,
            "mute",
            Err(audio::AudioError::Failed("no such node".into())),
        );
        assert!(
            matches!(
                &failed,
                ControlReport::LevelFailed {
                    target: AudioTarget::Input,
                    what: "mute",
                    reason,
                } if reason == "no such node"
            ),
            "{failed:?}"
        );
    }
}

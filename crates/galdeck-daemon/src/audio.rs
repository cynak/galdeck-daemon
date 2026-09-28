//! Sound volume and mute, through the sound server's own command-line tool.
//!
//! `wpctl` for PipeWire, which is nearly every desktop now, and `pactl` only
//! where `wpctl` is not installed at all: a PulseAudio system. Which one is
//! decided once, by looking along `PATH`, because the answer does not change
//! while the daemon runs and a knob turned twice a second should not pay for
//! finding out again. The file found is the file run, by its full path.
//!
//! No shell is involved anywhere. The target comes from config, and a string
//! that reaches `sh -c` is a string someone can hide a second command in;
//! passed as its own argument it is only ever a node name, and one that could
//! be mistaken for an option is refused before it gets that far.
//!
//! Every call has a deadline. A wedged sound server would otherwise hold up
//! whatever thread asked, and on a single worker that means every knob and key
//! queued behind it.
//!
//! Switching outputs and per-app volume need more than a volume tool: which
//! sinks and streams there are comes from `pw-dump`, read by
//! [`crate::pipewire`], and then only `wpctl` can act on them, by id. So those
//! work on PipeWire alone (see [`mixer`]), and each of their jobs has one
//! deadline for every process it runs.

use std::ffi::{OsStr, OsString};
use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{ChildStdout, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::pipewire::{self, App, Sink};

/// How long one operation may take, all its processes together, before the
/// one still running is killed.
///
/// `wpctl` answers in a few milliseconds; two seconds only runs out when the
/// sound server is not answering at all, and waiting longer would not help.
pub const DEADLINE: Duration = Duration::from_secs(2);
/// Longest deadline [`run`] keeps.
///
/// A caller may well pass `Duration::MAX` to mean "no deadline", and adding
/// that to the clock overflows it; a year is the same thing for a command
/// that should take milliseconds.
const LONGEST_DEADLINE: Duration = Duration::from_secs(365 * 24 * 60 * 60);
/// How often to check on a running command and collect its output.
const POLL: Duration = Duration::from_millis(5);
/// Most output kept from one command.
///
/// The longest answer either tool gives here is one line of channel volumes;
/// anything past this is not an answer worth keeping.
const MAX_OUTPUT: usize = 4096;
/// Most `pw-dump` may print before it is stopped and its answer refused.
///
/// About thirteen times what a laptop with five outputs prints. A dump cut
/// short is not JSON, so there is no keeping the start of it the way the
/// start of a `wpctl` answer is kept.
const MAX_DUMP: usize = 4 << 20;
/// How much is read from a child in one go: a whole pipe's worth.
const READ_SIZE: usize = 64 << 10;
/// Most reads per look at a child's output, so a child that never stops
/// talking still lets the clock be checked. Sixteen pipes' worth is as much
/// as the largest pipe Linux allows by default holds, so one look after the
/// child exits takes everything it left behind.
const READS_PER_LOOK: usize = 16;
/// How often to ask, after making a sink the default, whether it is yet.
const DEFAULT_POLL: Duration = Duration::from_millis(20);
/// How long WirePlumber is given to make it so.
const DEFAULT_WAIT: Duration = Duration::from_millis(200);
/// Longest node name accepted, in characters. PipeWire's own names run to
/// about 60.
const MAX_NODE: usize = 128;

const WPCTL: &str = "wpctl";
const PACTL: &str = "pactl";
const PW_DUMP: &str = "pw-dump";
/// The one kind of object the wait after a switch needs to see.
const METADATA: &str = "PipeWire:Interface:Metadata";

/// Which sound device an operation is about.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AudioTarget {
    /// The default output: speakers or headphones, whichever is current.
    Output,
    /// The default input.
    Input,
    /// A node. `wpctl` knows one only by its id (`42`, from `wpctl status`);
    /// `pactl` takes a sink's index or name.
    Node(String),
}

impl AudioTarget {
    /// The target a widget's or action's `source` names.
    ///
    /// Nothing means the default output and `mic` the default input; anything
    /// else must look like a node id or name, and is refused otherwise rather
    /// than handed to a sound tool to interpret.
    pub fn parse(source: Option<&str>) -> Result<AudioTarget, String> {
        match source {
            None | Some("") => Ok(AudioTarget::Output),
            Some("mic") => Ok(AudioTarget::Input),
            Some(node) => {
                check_node(node)?;
                Ok(AudioTarget::Node(node.to_string()))
            }
        }
    }

    /// How `wpctl` names this target.
    pub fn wpctl_id(&self) -> String {
        match self {
            AudioTarget::Output => "@DEFAULT_AUDIO_SINK@".into(),
            AudioTarget::Input => "@DEFAULT_AUDIO_SOURCE@".into(),
            AudioTarget::Node(node) => node.clone(),
        }
    }

    /// Whether `pactl` calls this a sink or a source, and what it calls it.
    fn pactl_device(&self) -> (&'static str, String) {
        match self {
            AudioTarget::Output => ("sink", "@DEFAULT_SINK@".into()),
            AudioTarget::Input => ("source", "@DEFAULT_SOURCE@".into()),
            AudioTarget::Node(node) => ("sink", node.clone()),
        }
    }
}

/// A node id (`42`) or name (`alsa_output.pci-0000_00_1f.3.analog-stereo`).
///
/// Deliberately narrower than what PipeWire allows in a name: these are the
/// characters real node names use, and none of them means anything to a
/// shell. A leading `-` is refused because the tool would read it as an
/// option. Whether the tool in use takes a name at all is for [`usable`].
fn check_node(node: &str) -> Result<(), String> {
    if node.is_empty() {
        return Err("an audio source names a node, and this one is empty".into());
    }
    // In characters, as the message says. Only ASCII gets past the check on
    // characters below, so the limit in bytes is the same one.
    let length = node.chars().count();
    if length > MAX_NODE {
        return Err(format!(
            "an audio source is a node id or name of at most {MAX_NODE} characters; this one has {length}"
        ));
    }
    if node.starts_with('-') {
        return Err(format!(
            "audio source {node:?} starts with '-', which a sound tool would read as an option"
        ));
    }
    if let Some(c) = node
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-')))
    {
        return Err(format!(
            "audio source {node:?} contains {c:?}; use \"mic\", or a node id such as 42 \
             from `wpctl status` (a node name works too, but only with {PACTL})"
        ));
    }
    Ok(())
}

/// A device's volume, and whether it is muted.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Level {
    /// 100 is the device's nominal full volume; a device can be set above it.
    pub percent: f64,
    pub muted: bool,
}

#[derive(Debug)]
pub enum AudioError {
    /// Neither `wpctl` nor `pactl` is installed.
    Missing,
    /// The tool ran and said no, or said something that is not an answer.
    Failed(String),
    /// The tool did not finish before the deadline and was killed.
    TimedOut,
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioError::Missing => write!(f, "neither {WPCTL} nor {PACTL} is installed"),
            AudioError::Failed(reason) => f.write_str(reason),
            AudioError::TimedOut => write!(f, "the sound server did not answer in time"),
        }
    }
}

impl std::error::Error for AudioError {}

/// The sound tool in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tool {
    Wpctl,
    Pactl,
}

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Tool::Wpctl => WPCTL,
            Tool::Pactl => PACTL,
        }
    }
}

/// The sound tool in use, and the file it was found at.
///
/// That file is what runs, by its full path. Spawning by name would search
/// `PATH` again, the C library's way, and that way an empty or relative entry
/// means the current directory: a different file from the one that was
/// found, and one the daemon should never run.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Installed {
    tool: Tool,
    program: PathBuf,
}

impl Installed {
    fn run(&self, args: &[String], until: Instant) -> Result<String, AudioError> {
        run_until(&self.program, args, until)
    }
}

/// Whether volume can be read and changed on this machine at all, without
/// running anything to find out.
pub fn available() -> bool {
    installed().is_ok()
}

/// The target's volume and mute state. Blocking: it asks the sound server.
pub fn read(target: &AudioTarget) -> Result<Level, AudioError> {
    let sound = prepare(target)?;
    read_with(sound, target, Instant::now() + DEADLINE)
}

/// Move the target's volume by `delta_percent`, up or down, never raising it
/// past `ceiling_percent`, and return where it ended up.
///
/// One change per call, however large: a knob spun through several detents
/// should add them up and call once, not start a process per detent.
///
/// Raising also unmutes. Turning a muted device up and hearing nothing reads
/// as a broken knob, and it is what desktop volume keys do.
pub fn adjust(
    target: &AudioTarget,
    delta_percent: f64,
    ceiling_percent: f64,
) -> Result<Level, AudioError> {
    check_change(delta_percent, ceiling_percent)?;
    let sound = prepare(target)?;
    let until = Instant::now() + DEADLINE;
    match sound.tool {
        Tool::Wpctl => {
            if delta_percent > 0.0 {
                // Unconditionally: unmuting what is not muted changes nothing,
                // and costs the same one process a read to find out would.
                sound.run(&wpctl_mute_args(target, Mute::Off), until)?;
            }
            if delta_percent != 0.0 {
                let args = wpctl_adjust_args(target, delta_percent, ceiling_percent);
                sound.run(&args, until)?;
            }
        }
        Tool::Pactl => {
            // pactl has no ceiling of its own, so the new level is worked out
            // here and set as an absolute one.
            let before = read_with(sound, target, until)?;
            if delta_percent > 0.0 && before.muted {
                sound.run(&pactl_mute_args(target, Mute::Off), until)?;
            }
            let after = pactl_level(before.percent, delta_percent, ceiling_percent);
            if f64::from(after) != before.percent {
                sound.run(&pactl_set_volume_args(target, after), until)?;
            }
        }
    }
    read_with(sound, target, until)
}

/// A volume change that can be made: a step that is a number, under a
/// ceiling that is a positive one.
fn check_change(delta_percent: f64, ceiling_percent: f64) -> Result<(), AudioError> {
    if !delta_percent.is_finite() {
        return Err(AudioError::Failed(format!(
            "a volume step of {delta_percent} is not a number of percent"
        )));
    }
    if !(ceiling_percent.is_finite() && ceiling_percent > 0.0) {
        return Err(AudioError::Failed(format!(
            "a volume ceiling of {ceiling_percent} is not a positive number of percent"
        )));
    }
    Ok(())
}

/// Mute the target if it is not, unmute it if it is, and return the result.
pub fn toggle_mute(target: &AudioTarget) -> Result<Level, AudioError> {
    mute(target, Mute::Toggle)
}

/// Mute or unmute the target, and return the result.
pub fn set_mute(target: &AudioTarget, muted: bool) -> Result<Level, AudioError> {
    mute(target, if muted { Mute::On } else { Mute::Off })
}

fn mute(target: &AudioTarget, mute: Mute) -> Result<Level, AudioError> {
    let sound = prepare(target)?;
    let until = Instant::now() + DEADLINE;
    match sound.tool {
        Tool::Wpctl => sound.run(&wpctl_mute_args(target, mute), until)?,
        Tool::Pactl => sound.run(&pactl_mute_args(target, mute), until)?,
    };
    read_with(sound, target, until)
}

/// Where an output switch landed.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputOutcome {
    /// What to call the output now in use.
    pub display: String,
    /// Its place among the outputs switched between, from 0.
    pub index: usize,
    /// How many outputs there are to switch between.
    pub count: usize,
    /// Its volume and mute, read by its id, since `@DEFAULT_AUDIO_SINK@`
    /// could still be the one before. None when it could not be read; the
    /// switch happened all the same.
    pub level: Option<Level>,
    /// Whether the default changed: not when there is only the one output,
    /// nor when the one asked for already was the default.
    pub changed: bool,
}

/// Which app a per-app job acted on.
#[derive(Clone, Debug, PartialEq)]
pub enum AppOutcome {
    Found {
        /// What the app is known by (see [`App::key`]), for a knob to
        /// remember and name it by next time.
        app: String,
        display: String,
        /// Its place among the apps, from 0.
        index: usize,
        /// How many apps there are.
        count: usize,
        /// Its first stream's level, after the job.
        level: Level,
    },
    /// Nothing was done: no app is playing (`app` None), or the one asked
    /// for has no streams, or they ended while the job ran.
    ///
    /// `app` is only for saying which, so it has been through
    /// [`pipewire::display_name`]: a knob's remembered app is whatever name
    /// some client gave itself.
    NoApp { app: Option<String> },
}

/// Whether switching outputs and per-app volume can work here, and why not
/// when they cannot. Nothing is run to find out.
///
/// They need `wpctl` itself, since only it acts on a node by id, and
/// `pw-dump` to see which nodes there are.
pub fn mixer() -> Result<(), String> {
    tools().map(|_| ())
}

/// Make the output `step` places on from the current default the default,
/// going round at either end, and say where it landed.
///
/// `order` is the `outputs` list from galdeck.toml, empty when there is none
/// (see [`pipewire::outputs`]). Blocking, until `until` at the latest, for
/// every process together.
pub fn switch_output(
    step: i8,
    order: &[String],
    until: Instant,
) -> Result<OutputOutcome, AudioError> {
    let tools = tools().map_err(AudioError::Failed)?;
    change_output(&tools, Choice::Step(step), order, until)
}

/// Make the output `name` names the default (see
/// [`pipewire::find_output`]), and say where it landed.
pub fn set_output(
    name: &str,
    order: &[String],
    until: Instant,
) -> Result<OutputOutcome, AudioError> {
    let tools = tools().map_err(AudioError::Failed)?;
    change_output(&tools, Choice::Named(name), order, until)
}

/// Move every stream of an app by `delta_percent`, never past
/// `ceiling_percent`, and return the app's level.
///
/// The app `app` names, or with none the one playing (see
/// [`pipewire::pick_app`]). Raising also unmutes, as it does for a device.
///
/// wpctl's limit holds each stream at the ceiling, and a stream at 100% is
/// exactly as loud as its output, so with the ceiling at 100 no app gets
/// louder than the output it plays through. The limit applies to where a
/// change ends up, so a stream something else left above the ceiling comes
/// down to it at the next turn, whichever way: the safe side to err on.
pub fn adjust_app(
    app: Option<&str>,
    delta_percent: f64,
    ceiling_percent: f64,
    until: Instant,
) -> Result<AppOutcome, AudioError> {
    check_change(delta_percent, ceiling_percent)?;
    let tools = tools().map_err(AudioError::Failed)?;
    adjust_app_with(&tools, app, delta_percent, ceiling_percent, until)
}

/// Mute an app if its first stream is not muted, unmute it if it is, and
/// return its level.
pub fn toggle_app_mute(app: Option<&str>, until: Instant) -> Result<AppOutcome, AudioError> {
    let tools = tools().map_err(AudioError::Failed)?;
    toggle_app_mute_with(&tools, app, until)
}

/// The app after `current` (see [`pipewire::next_app`]), and its level.
/// Nothing is changed.
pub fn next_app(current: Option<&str>, until: Instant) -> Result<AppOutcome, AudioError> {
    let tools = tools().map_err(AudioError::Failed)?;
    next_app_with(&tools, current, until)
}

/// Every output and app there is, for the editor to offer as targets.
pub fn audio_targets(until: Instant) -> Result<pipewire::Targets, AudioError> {
    let tools = tools().map_err(AudioError::Failed)?;
    targets_with(&tools, until)
}

/// What the output and app jobs run.
///
/// A seam, so tests can stand in for PipeWire with answers given in advance,
/// and never touch a real default or volume.
trait Mixer {
    /// `pw-dump -N`: the whole graph, or with `only`, objects of that type.
    fn dump(&self, only: Option<&str>, until: Instant) -> Result<Vec<u8>, AudioError>;
    /// `wpctl` with `args`, and what it printed.
    fn wpctl(&self, args: &[String], until: Instant) -> Result<String, AudioError>;
}

/// The real programs, by the paths they were found at.
#[derive(Debug)]
struct Tools<'a> {
    wpctl: &'a Path,
    pw_dump: &'a Path,
}

impl Mixer for Tools<'_> {
    fn dump(&self, only: Option<&str>, until: Instant) -> Result<Vec<u8>, AudioError> {
        let mut args = vec!["-N".to_string()];
        args.extend(only.map(str::to_string));
        run_capped(self.pw_dump, &args, until, MAX_DUMP)
    }

    fn wpctl(&self, args: &[String], until: Instant) -> Result<String, AudioError> {
        run_until(self.wpctl, args, until)
    }
}

fn tools() -> Result<Tools<'static>, String> {
    mixer_tools(installed().ok(), pw_dump())
}

/// The programs the output and app jobs need, or why they cannot run.
fn mixer_tools<'a>(
    sound: Option<&'a Installed>,
    pw_dump: Option<&'a Path>,
) -> Result<Tools<'a>, String> {
    let wpctl = match sound {
        Some(Installed {
            tool: Tool::Wpctl,
            program,
        }) => program,
        Some(Installed {
            tool: Tool::Pactl, ..
        }) => return Err(format!("only {PACTL} is installed, and this needs {WPCTL}")),
        None => return Err(format!("{WPCTL} is not installed")),
    };
    let pw_dump = pw_dump.ok_or_else(|| format!("{PW_DUMP} is not installed"))?;
    Ok(Tools { wpctl, pw_dump })
}

/// Which output a switch is to.
#[derive(Clone, Copy, Debug)]
enum Choice<'a> {
    /// So many places on from the current default.
    Step(i8),
    /// The one a name finds.
    Named(&'a str),
}

fn change_output(
    mixer: &impl Mixer,
    choice: Choice,
    order: &[String],
    until: Instant,
) -> Result<OutputOutcome, AudioError> {
    let graph = pipewire::parse(&mixer.dump(None, until)?);
    let outputs = pipewire::outputs(&graph.sinks, order);
    let current = graph.default_sink.as_deref();
    let index = match choice {
        Choice::Step(step) => {
            pipewire::step_output(&outputs, current, step).ok_or_else(|| no_output(order))?
        }
        Choice::Named(name) => {
            pipewire::find_output(&graph.sinks, &outputs, name).map_err(AudioError::Failed)?
        }
    };
    let chosen = outputs[index];
    let changed = current != Some(chosen.name.as_str());
    if changed {
        // `outputs` holds only sinks that can be heard. Making one that
        // cannot the default would silence every app, and this is the last
        // place to be sure it does not happen.
        if !chosen.usable {
            return Err(AudioError::Failed(format!(
                "{} is not connected",
                chosen.display
            )));
        }
        mixer.wpctl(&["set-default".into(), chosen.id.to_string()], until)?;
        wait_for_default(mixer, chosen, until)?;
    }
    let level = read_node(mixer, chosen.id, until).unwrap_or_else(|e| {
        log::debug!("reading {:?} after switching to it: {e}", chosen.display);
        None
    });
    Ok(OutputOutcome {
        display: chosen.display.clone(),
        index,
        count: outputs.len(),
        level,
        changed,
    })
}

fn no_output(order: &[String]) -> AudioError {
    AudioError::Failed(if order.is_empty() {
        "no output can be heard".into()
    } else {
        "none of the outputs in galdeck.toml can be heard".into()
    })
}

/// Wait for `sink` to be the default output.
///
/// `wpctl set-default` only asks: WirePlumber makes the change in its own
/// time, and until it has, `@DEFAULT_AUDIO_SINK@` is still the old sink.
/// A switch that never shows is a failure, not a success to report.
fn wait_for_default(mixer: &impl Mixer, sink: &Sink, until: Instant) -> Result<(), AudioError> {
    let give_up = (Instant::now() + DEFAULT_WAIT).min(until);
    loop {
        let dump = mixer.dump(Some(METADATA), until)?;
        if pipewire::parse(&dump).default_sink.as_deref() == Some(sink.name.as_str()) {
            return Ok(());
        }
        if Instant::now() + DEFAULT_POLL > give_up {
            return Err(AudioError::Failed(format!(
                "{} did not become the default output",
                sink.display
            )));
        }
        std::thread::sleep(DEFAULT_POLL);
    }
}

fn adjust_app_with(
    mixer: &impl Mixer,
    wanted: Option<&str>,
    delta_percent: f64,
    ceiling_percent: f64,
    until: Instant,
) -> Result<AppOutcome, AudioError> {
    let apps = look(mixer, until)?;
    let Some(index) = pipewire::pick_app(&apps, wanted) else {
        return Ok(no_app(wanted));
    };
    for &id in &apps[index].streams {
        let stream = node_target(id);
        if delta_percent > 0.0 {
            mixer.wpctl(&wpctl_mute_args(&stream, Mute::Off), until)?;
        }
        if delta_percent != 0.0 {
            let args = wpctl_adjust_args(&stream, delta_percent, ceiling_percent);
            mixer.wpctl(&args, until)?;
        }
    }
    settle(mixer, apps, index, until)
}

fn toggle_app_mute_with(
    mixer: &impl Mixer,
    wanted: Option<&str>,
    until: Instant,
) -> Result<AppOutcome, AudioError> {
    let apps = look(mixer, until)?;
    let Some(index) = pipewire::pick_app(&apps, wanted) else {
        return Ok(no_app(wanted));
    };
    let key = apps[index].key.clone();
    let Some((apps, index, level)) = read_app(mixer, apps, index, until)? else {
        return Ok(no_app(Some(&key)));
    };
    // One state for every stream, the opposite of the first one's. Toggling
    // each would leave an app whose streams disagree disagreeing still.
    let mute = if level.muted { Mute::Off } else { Mute::On };
    for &id in &apps[index].streams {
        mixer.wpctl(&wpctl_mute_args(&node_target(id), mute), until)?;
    }
    settle(mixer, apps, index, until)
}

fn next_app_with(
    mixer: &impl Mixer,
    current: Option<&str>,
    until: Instant,
) -> Result<AppOutcome, AudioError> {
    let apps = look(mixer, until)?;
    let Some(index) = pipewire::next_app(&apps, current) else {
        return Ok(AppOutcome::NoApp { app: None });
    };
    settle(mixer, apps, index, until)
}

fn targets_with(mixer: &impl Mixer, until: Instant) -> Result<pipewire::Targets, AudioError> {
    let graph = pipewire::parse(&mixer.dump(None, until)?);
    Ok(pipewire::targets(&graph))
}

/// The apps there are now.
fn look(mixer: &impl Mixer, until: Instant) -> Result<Vec<App>, AudioError> {
    Ok(pipewire::apps(&pipewire::parse(&mixer.dump(None, until)?)))
}

fn no_app(wanted: Option<&str>) -> AppOutcome {
    AppOutcome::NoApp {
        app: wanted.map(|name| pipewire::display_name(name, pipewire::DISPLAY_LENGTH)),
    }
}

/// What an app job reports: the app, and its level after the job.
fn settle(
    mixer: &impl Mixer,
    apps: Vec<App>,
    index: usize,
    until: Instant,
) -> Result<AppOutcome, AudioError> {
    let key = apps[index].key.clone();
    Ok(match read_app(mixer, apps, index, until)? {
        Some((apps, index, level)) => AppOutcome::Found {
            app: apps[index].key.clone(),
            display: apps[index].display.clone(),
            index,
            count: apps.len(),
            level,
        },
        None => no_app(Some(&key)),
    })
}

/// An app's level, from its first stream, with the apps it was found among.
///
/// A stream can end between the dump and the read, and then wpctl prints
/// nothing. The app is looked up once more before it counts as gone, since a
/// player often ends one stream only to open the next.
fn read_app(
    mixer: &impl Mixer,
    apps: Vec<App>,
    index: usize,
    until: Instant,
) -> Result<Option<(Vec<App>, usize, Level)>, AudioError> {
    if let Some(level) = first_level(mixer, &apps[index], until)? {
        return Ok(Some((apps, index, level)));
    }
    let again = look(mixer, until)?;
    let Some(found) = pipewire::find_app(&again, &apps[index].key) else {
        return Ok(None);
    };
    let level = first_level(mixer, &again[found], until)?;
    Ok(level.map(|level| (again, found, level)))
}

fn first_level(mixer: &impl Mixer, app: &App, until: Instant) -> Result<Option<Level>, AudioError> {
    match app.streams.first() {
        Some(&id) => read_node(mixer, id, until),
        None => Ok(None),
    }
}

/// A node's level, by its id. None when there is no such node any more:
/// wpctl says so on stderr, prints nothing, and still exits 0.
fn read_node(mixer: &impl Mixer, id: u32, until: Instant) -> Result<Option<Level>, AudioError> {
    let output = mixer.wpctl(&wpctl_read_args(&node_target(id)), until)?;
    if output.trim().is_empty() {
        return Ok(None);
    }
    parse_wpctl(&output)
        .map(Some)
        .ok_or_else(|| unexpected(WPCTL, &output))
}

/// A node found in a dump, as a target. Its id comes from [`pipewire::parse`],
/// which takes only ids wpctl takes, so it needs no checking again.
fn node_target(id: u32) -> AudioTarget {
    AudioTarget::Node(id.to_string())
}

/// Everything that can refuse an operation before a process is started: the
/// target itself, whether there is a sound tool, and whether that tool can
/// name the target.
fn prepare(target: &AudioTarget) -> Result<&'static Installed, AudioError> {
    checked(target)?;
    let sound = installed()?;
    usable(sound.tool, target)?;
    Ok(sound)
}

/// The target checked again, for one built by hand rather than by
/// [`AudioTarget::parse`]: the variant is public, and a node that starts with
/// `-` must not reach a command line whichever way it was made.
fn checked(target: &AudioTarget) -> Result<(), AudioError> {
    match target {
        AudioTarget::Node(node) => check_node(node).map_err(AudioError::Failed),
        _ => Ok(()),
    }
}

/// Whether `tool` can name `target` at all.
///
/// wpctl knows a node only by its id. Handed a name it prints its usage and
/// exits 1, which in the log would say only that it failed; this says why.
fn usable(tool: Tool, target: &AudioTarget) -> Result<(), AudioError> {
    match (tool, target) {
        (Tool::Wpctl, AudioTarget::Node(node)) if !is_node_id(node) => {
            Err(AudioError::Failed(format!(
                "{WPCTL} knows a node only by its id, such as 42 from `wpctl status`, \
                 and {node:?} is not one; a node name works only with {PACTL}"
            )))
        }
        _ => Ok(()),
    }
}

/// A number wpctl takes as a node id: from 1 up, short of `u32::MAX`, which
/// PipeWire keeps to mean no id at all.
fn is_node_id(node: &str) -> bool {
    node.parse::<u32>()
        .is_ok_and(|id| (1..u32::MAX).contains(&id))
}

fn read_with(sound: &Installed, target: &AudioTarget, until: Instant) -> Result<Level, AudioError> {
    let name = sound.tool.name();
    match sound.tool {
        Tool::Wpctl => {
            let output = sound.run(&wpctl_read_args(target), until)?;
            parse_wpctl(&output).ok_or_else(|| unexpected(name, &output))
        }
        Tool::Pactl => {
            // pactl reports volume and mute separately, so a reading is two
            // questions.
            let output = sound.run(&pactl_volume_args(target), until)?;
            let percent = parse_pactl_volume(&output).ok_or_else(|| unexpected(name, &output))?;
            let output = sound.run(&pactl_mute_state_args(target), until)?;
            let muted = parse_pactl_mute(&output).ok_or_else(|| unexpected(name, &output))?;
            Ok(Level { percent, muted })
        }
    }
}

/// An answer that is not the one asked for, with enough of it quoted to see
/// what went wrong.
fn unexpected(program: &str, output: &str) -> AudioError {
    let first: String = output
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(80)
        .collect();
    AudioError::Failed(format!(
        "{program} answered {first:?}, which is not a reading"
    ))
}

/// The tool to use, looked for once.
fn installed() -> Result<&'static Installed, AudioError> {
    static INSTALLED: OnceLock<Option<Installed>> = OnceLock::new();
    INSTALLED
        .get_or_init(|| {
            let path = search_path();
            choose(|program| find(program, &path))
        })
        .as_ref()
        .ok_or(AudioError::Missing)
}

/// Where `pw-dump` is, looked for once, the way [`installed`] looks.
fn pw_dump() -> Option<&'static Path> {
    static PW_DUMP_AT: OnceLock<Option<PathBuf>> = OnceLock::new();
    PW_DUMP_AT
        .get_or_init(|| find(PW_DUMP, &search_path()))
        .as_deref()
}

/// `PATH`, or where the C library looks when it is unset.
pub(crate) fn search_path() -> OsString {
    std::env::var_os("PATH").unwrap_or_else(|| "/bin:/usr/bin".into())
}

/// `wpctl` if it can be found, else `pactl`.
///
/// Not the other way round: on PipeWire `pactl` works too, through PipeWire's
/// PulseAudio emulation, but only `wpctl` has a ceiling of its own and
/// reports mute with the volume in one call.
fn choose(find: impl Fn(&str) -> Option<PathBuf>) -> Option<Installed> {
    let found = |tool: Tool| find(tool.name()).map(|program| Installed { tool, program });
    found(Tool::Wpctl).or_else(|| found(Tool::Pactl))
}

/// The executable file called `program` in the first of `path`'s directories
/// that has one.
///
/// Relative entries are skipped: an empty entry means the current directory,
/// and a daemon should not run whatever happens to be in the one it was
/// started from. What this returns is absolute, so it is also what runs.
pub(crate) fn find(program: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|file| is_executable(file))
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Mute, unmute, or flip. Both tools spell these `1`, `0` and `toggle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mute {
    On,
    Off,
    Toggle,
}

impl Mute {
    fn arg(self) -> &'static str {
        match self {
            Mute::On => "1",
            Mute::Off => "0",
            Mute::Toggle => "toggle",
        }
    }
}

/// `wpctl get-volume ID`.
fn wpctl_read_args(target: &AudioTarget) -> Vec<String> {
    vec!["get-volume".into(), target.wpctl_id()]
}

/// `wpctl set-volume -l 1 ID 2%+`, or `2%-` to lower.
///
/// The limit is passed when lowering too: it can only ever hold the result
/// down, and one shape of command is one thing to get right.
fn wpctl_adjust_args(
    target: &AudioTarget,
    delta_percent: f64,
    ceiling_percent: f64,
) -> Vec<String> {
    let sign = if delta_percent < 0.0 { '-' } else { '+' };
    vec![
        "set-volume".into(),
        "-l".into(),
        decimal(ceiling_percent / 100.0),
        target.wpctl_id(),
        format!("{}%{sign}", decimal(delta_percent.abs())),
    ]
}

/// `wpctl set-mute ID 1|0|toggle`.
fn wpctl_mute_args(target: &AudioTarget, mute: Mute) -> Vec<String> {
    vec!["set-mute".into(), target.wpctl_id(), mute.arg().into()]
}

/// `pactl get-sink-volume ID`, or `get-source-volume`.
fn pactl_volume_args(target: &AudioTarget) -> Vec<String> {
    let (kind, id) = target.pactl_device();
    vec![format!("get-{kind}-volume"), id]
}

/// `pactl get-sink-mute ID`, or `get-source-mute`.
fn pactl_mute_state_args(target: &AudioTarget) -> Vec<String> {
    let (kind, id) = target.pactl_device();
    vec![format!("get-{kind}-mute"), id]
}

/// `pactl set-sink-volume ID 47%`: absolute, in whole percent.
fn pactl_set_volume_args(target: &AudioTarget, percent: u32) -> Vec<String> {
    let (kind, id) = target.pactl_device();
    vec![format!("set-{kind}-volume"), id, format!("{percent}%")]
}

/// `pactl set-sink-mute ID 1|0|toggle`, or `set-source-mute`.
fn pactl_mute_args(target: &AudioTarget, mute: Mute) -> Vec<String> {
    let (kind, id) = target.pactl_device();
    vec![format!("set-{kind}-mute"), id, mute.arg().into()]
}

/// The absolute level to ask `pactl` for, in whole percent.
///
/// Whole, because pactl gives a `.` in a volume a meaning of its own and does
/// not read `47.5%` as a percentage. Rounded away from where it is now, so a
/// step smaller than one percent still moves. A raise never lowers: a level
/// already above the ceiling was put there on purpose, by something else.
fn pactl_level(current: f64, delta_percent: f64, ceiling_percent: f64) -> u32 {
    let wanted = if delta_percent > 0.0 {
        (current + delta_percent)
            .ceil()
            .min(ceiling_percent.floor())
            .max(current.round())
    } else if delta_percent < 0.0 {
        (current + delta_percent).floor()
    } else {
        current.round()
    };
    // `as` saturates, and a level is never negative or anywhere near u32::MAX.
    wanted.max(0.0) as u32
}

/// A number as short as it can be written: `1`, `0.8`, `2.5`.
///
/// Three places are more than either tool distinguishes, and trailing zeros
/// only make the commands in the log harder to read.
fn decimal(value: f64) -> String {
    let text = format!("{value:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// `Volume: 0.45`, or `Volume: 0.45 [MUTED]` when muted.
fn parse_wpctl(output: &str) -> Option<Level> {
    let line = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("Volume:"))?;
    let level: f64 = line.split_whitespace().next()?.parse().ok()?;
    if !(level.is_finite() && level >= 0.0) {
        return None;
    }
    Some(Level {
        percent: (level * 100.0).round(),
        muted: line.contains("[MUTED]"),
    })
}

/// `Volume: front-left: 29491 /  45% / -20.81 dB,   front-right: ...`: the
/// first channel's percentage.
fn parse_pactl_volume(output: &str) -> Option<f64> {
    let line = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("Volume:"))?;
    let percent: f64 = line
        .split_whitespace()
        .find_map(|word| word.strip_suffix('%'))?
        .parse()
        .ok()?;
    (percent.is_finite() && percent >= 0.0).then_some(percent)
}

/// `Mute: yes` or `Mute: no`.
fn parse_pactl_mute(output: &str) -> Option<bool> {
    let answer = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("Mute:"))?;
    match answer.trim() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

/// Run `program` with `args` and return what it printed, if it succeeded.
///
/// No shell: the arguments reach the program exactly as given. Nothing is
/// read from stdin and stderr is thrown away. Output past [`MAX_OUTPUT`] is
/// read and dropped rather than left in the pipe, so a program that prints a
/// lot still finishes. If it has not finished by the deadline it is killed and
/// reaped, and the answer is [`AudioError::TimedOut`]. A program that does not
/// exist is [`AudioError::Missing`]. A deadline longer than a year, up to
/// `Duration::MAX`, is a year.
///
/// A bare name is looked for along `PATH` the C library's way, which takes an
/// empty or relative entry as the current directory. The operations here do
/// not pass a name: they pass the absolute path [`find`] settled on.
///
/// It runs in the C locale. Both tools print and read numbers the locale's
/// way, and pactl translates its labels, while everything here writes and
/// parses `0.45` and `Volume:`.
pub fn run(program: &str, args: &[String], deadline: Duration) -> Result<String, AudioError> {
    let now = Instant::now();
    let until = now
        .checked_add(deadline.min(LONGEST_DEADLINE))
        .ok_or_else(|| {
            AudioError::Failed(format!(
                "a deadline {deadline:?} away is past where the clock ends"
            ))
        })?;
    run_until(Path::new(program), args, until)
}

/// [`run`], against a deadline shared by every process of one operation.
fn run_until(program: &Path, args: &[String], until: Instant) -> Result<String, AudioError> {
    let (status, output) = collect(program, args, until, MAX_OUTPUT, Overflow::Drop)?;
    exited(program, args, status)?;
    Ok(String::from_utf8_lossy(&output).into_owned())
}

/// Run `program` with `args` and return everything it printed, if it
/// succeeded and printed at most `max` bytes.
///
/// [`run`]'s rules otherwise: no shell, nothing on stdin, stderr thrown away,
/// the C locale, and killed at `until`. Unlike it, output past the most kept
/// is not read and dropped: the program is stopped and its answer refused,
/// because what this runs is `pw-dump`, and a dump cut short is not JSON at
/// all. Exit status 255 is how pw-dump says it could not connect to PipeWire,
/// and is reported as that.
pub fn run_capped(
    program: &Path,
    args: &[String],
    until: Instant,
    max: usize,
) -> Result<Vec<u8>, AudioError> {
    let (status, output) = collect(program, args, until, max, Overflow::Refuse)?;
    if status.code() == Some(255) {
        return Err(AudioError::Failed(format!(
            "{} could not connect to PipeWire",
            program.display()
        )));
    }
    exited(program, args, status)?;
    Ok(output)
}

/// What to do with output past the most a caller keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Overflow {
    /// Read it and drop it: the answer is at the start, and the rest only has
    /// to leave the pipe for the program to finish.
    Drop,
    /// Stop the program and refuse the answer.
    Refuse,
}

/// Run `program`, collecting at most `max` bytes of what it prints, until it
/// exits or `until` comes, and say how it exited.
fn collect(
    program: &Path,
    args: &[String],
    until: Instant,
    max: usize,
    overflow: Overflow,
) -> Result<(ExitStatus, Vec<u8>), AudioError> {
    // The earlier steps of the operation used the time up. Better not to
    // start this one than to start it and kill it halfway.
    if Instant::now() >= until {
        return Err(AudioError::TimedOut);
    }
    let mut child = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| match e.kind() {
            ErrorKind::NotFound => AudioError::Missing,
            _ => AudioError::Failed(format!("{} did not start: {e}", program.display())),
        })?;
    let described = || described(program, args);
    let too_much =
        || AudioError::Failed(format!("{} printed more than {}", described(), size(max)));

    let mut stdout = child.stdout.take().expect("stdout was asked to be piped");
    if let Err(e) = set_nonblocking(&stdout) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(AudioError::Failed(format!("{}: {e}", described())));
    }

    let mut buffer = vec![0; READ_SIZE];
    let mut output = Vec::new();
    loop {
        let looked = drain(&mut stdout, &mut buffer, &mut output, max);
        if looked.over && overflow == Overflow::Refuse {
            let _ = child.kill();
            let _ = child.wait();
            return Err(too_much());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // Whatever it wrote between the last look and exiting.
                let last = drain(&mut stdout, &mut buffer, &mut output, max);
                if last.over && overflow == Overflow::Refuse {
                    return Err(too_much());
                }
                return Ok((status, output));
            }
            Ok(None) if Instant::now() >= until => {
                log::debug!("{} overran its deadline, killing it", described());
                let _ = child.kill();
                let _ = child.wait();
                return Err(AudioError::TimedOut);
            }
            // It is still talking, so look again straight away. A sleep
            // after every pipe's worth would spend most of a large dump's
            // deadline asleep.
            Ok(None) if looked.read => {}
            Ok(None) => std::thread::sleep(POLL),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AudioError::Failed(format!(
                    "waiting on {}: {e}",
                    described()
                )));
            }
        }
    }
}

/// A command as the log and the error messages quote it.
fn described(program: &Path, args: &[String]) -> String {
    format!("{} {}", program.display(), args.join(" "))
}

/// Refuse the answer of a program that did not succeed.
fn exited(program: &Path, args: &[String], status: ExitStatus) -> Result<(), AudioError> {
    if status.success() {
        return Ok(());
    }
    Err(AudioError::Failed(format!(
        "{} failed: {status}",
        described(program, args)
    )))
}

/// A number of bytes the way a person reads one: `4 MiB`, `64 KiB`, `100
/// bytes`.
fn size(bytes: usize) -> String {
    const KIB: usize = 1 << 10;
    const MIB: usize = 1 << 20;
    if bytes >= MIB && bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else if bytes >= KIB && bytes.is_multiple_of(KIB) {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{bytes} bytes")
    }
}

/// Make reads from the child's stdout return at once when it has nothing.
///
/// So one thread can both collect output and watch the clock; a blocking read
/// would wait for as long as the child chose to stay quiet.
fn set_nonblocking(stdout: &ChildStdout) -> std::io::Result<()> {
    let fd = stdout.as_raw_fd();
    // SAFETY: fcntl on a descriptor this process owns and keeps open for the
    // duration; no memory is passed.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// What one look at a child's output found.
struct Looked {
    /// Whether it had written anything.
    read: bool,
    /// Whether that went past the most kept.
    over: bool,
}

/// Take what the child has written so far, keeping at most `max` bytes of it
/// all, and reading through `buffer`.
///
/// Reading while it runs, rather than once it exits, is what lets a child
/// that prints more than a pipe holds finish at all: otherwise it blocks on a
/// full pipe and is killed at the deadline for talking too much. At most
/// [`READS_PER_LOOK`] reads are made per look, so a child that never stops
/// talking cannot keep the caller from checking the clock.
fn drain(stdout: &mut ChildStdout, buffer: &mut [u8], output: &mut Vec<u8>, max: usize) -> Looked {
    let mut looked = Looked {
        read: false,
        over: false,
    };
    for _ in 0..READS_PER_LOOK {
        match stdout.read(buffer) {
            Ok(0) => break,
            Ok(n) => {
                looked.read = true;
                let room = max.saturating_sub(output.len());
                looked.over |= n > room;
                output.extend_from_slice(&buffer[..n.min(room)]);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            // WouldBlock: nothing more for now. Anything else will show up
            // again, or the child's exit will.
            Err(_) => break,
        }
    }
    looked
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn node(name: &str) -> AudioTarget {
        AudioTarget::Node(name.into())
    }

    #[test]
    fn no_source_is_the_output_and_mic_is_the_input() {
        assert_eq!(AudioTarget::parse(None), Ok(AudioTarget::Output));
        assert_eq!(AudioTarget::parse(Some("")), Ok(AudioTarget::Output));
        assert_eq!(AudioTarget::parse(Some("mic")), Ok(AudioTarget::Input));
    }

    #[test]
    fn a_node_is_an_id_or_a_name() {
        for source in [
            "42",
            "alsa_output.pci-0000_00_1f.3.analog-stereo",
            "bluez_output.AA:BB:CC:DD:EE:FF.1",
            "Mic-2",
        ] {
            assert_eq!(AudioTarget::parse(Some(source)), Ok(node(source)));
        }
        let longest = "a".repeat(MAX_NODE);
        assert_eq!(AudioTarget::parse(Some(&longest)), Ok(node(&longest)));
    }

    #[test]
    fn a_source_that_could_be_read_as_more_than_a_node_is_refused() {
        for source in [
            "-x",
            "--help",
            "-1",
            "a b",
            " 42",
            "42 ",
            "'42'",
            "\"42\"",
            "42;reboot",
            "$(id)",
            "`id`",
            "a|b",
            "a&b",
            "a\nb",
            "a/b",
            "@DEFAULT_AUDIO_SINK@",
            "micro\u{fe0f}",
            "é",
        ] {
            let refused = AudioTarget::parse(Some(source));
            assert!(refused.is_err(), "{source:?} was accepted: {refused:?}");
        }
        let error = AudioTarget::parse(Some(&"a".repeat(MAX_NODE + 1))).unwrap_err();
        assert!(error.contains("129"), "{error}");
        // Too long is counted in characters, as the message says it is, and
        // what is short enough but not ASCII is refused for its characters.
        let error = AudioTarget::parse(Some(&"é".repeat(MAX_NODE + 1))).unwrap_err();
        assert!(error.contains("has 129"), "{error}");
        let error = AudioTarget::parse(Some(&"é".repeat(100))).unwrap_err();
        assert!(error.contains("contains 'é'"), "{error}");
        // A message quotes what it refused, escaped, never raw.
        let error = AudioTarget::parse(Some("a\nb")).unwrap_err();
        assert!(!error.contains('\n'), "{error}");
    }

    #[test]
    fn a_node_built_by_hand_is_checked_all_the_same() {
        assert!(matches!(checked(&node("-x")), Err(AudioError::Failed(_))));
        assert!(matches!(checked(&node("")), Err(AudioError::Failed(_))));
        assert!(checked(&node("42")).is_ok());
        assert!(checked(&AudioTarget::Output).is_ok());
        // Refused by the check itself, before any sound tool is asked. Were it
        // missing, `wpctl get-volume --help` would print its usage and still
        // fail as an unreadable answer, so the reason is what tells them apart.
        let refused = read(&node("--help"));
        assert!(
            matches!(&refused, Err(AudioError::Failed(m)) if m.contains("starts with '-'")),
            "{refused:?}"
        );
        let refused = read(&node(""));
        assert!(
            matches!(&refused, Err(AudioError::Failed(m)) if m.contains("empty")),
            "{refused:?}"
        );
    }

    #[test]
    fn wpctl_is_given_only_a_node_id() {
        let name = "alsa_output.pci-0000_00_1f.3.analog-stereo";
        let takes = |tool, source: &str| usable(tool, &node(source)).is_ok();
        assert!(takes(Tool::Wpctl, "42"));
        assert!(takes(Tool::Wpctl, "046"));
        assert!(!takes(Tool::Wpctl, name));
        assert!(!takes(Tool::Wpctl, "0"));
        assert!(!takes(Tool::Wpctl, "4294967295"));
        assert!(!takes(Tool::Wpctl, "99999999999"));
        assert!(takes(Tool::Pactl, name));
        assert!(takes(Tool::Pactl, "0"));
        for tool in [Tool::Wpctl, Tool::Pactl] {
            assert!(usable(tool, &AudioTarget::Output).is_ok());
            assert!(usable(tool, &AudioTarget::Input).is_ok());
        }
        let error = usable(Tool::Wpctl, &node(name)).unwrap_err();
        assert!(error.to_string().contains("pactl"), "{error}");
    }

    #[test]
    fn targets_are_named_the_way_each_tool_names_them() {
        assert_eq!(AudioTarget::Output.wpctl_id(), "@DEFAULT_AUDIO_SINK@");
        assert_eq!(AudioTarget::Input.wpctl_id(), "@DEFAULT_AUDIO_SOURCE@");
        assert_eq!(node("42").wpctl_id(), "42");
        assert_eq!(
            AudioTarget::Output.pactl_device(),
            ("sink", "@DEFAULT_SINK@".into())
        );
        assert_eq!(
            AudioTarget::Input.pactl_device(),
            ("source", "@DEFAULT_SOURCE@".into())
        );
        assert_eq!(node("42").pactl_device(), ("sink", "42".into()));
    }

    #[test]
    fn raising_carries_the_ceiling_and_lowering_the_sign() {
        assert_eq!(
            wpctl_adjust_args(&AudioTarget::Output, 2.0, 100.0),
            strings(&["set-volume", "-l", "1", "@DEFAULT_AUDIO_SINK@", "2%+"])
        );
        assert_eq!(
            wpctl_adjust_args(&AudioTarget::Input, -6.0, 100.0),
            strings(&["set-volume", "-l", "1", "@DEFAULT_AUDIO_SOURCE@", "6%-"])
        );
        assert_eq!(
            wpctl_adjust_args(&node("42"), 2.5, 80.0),
            strings(&["set-volume", "-l", "0.8", "42", "2.5%+"])
        );
        assert_eq!(
            wpctl_adjust_args(&AudioTarget::Output, 16.0, 150.0),
            strings(&["set-volume", "-l", "1.5", "@DEFAULT_AUDIO_SINK@", "16%+"])
        );
    }

    #[test]
    fn wpctl_reads_and_mutes() {
        assert_eq!(
            wpctl_read_args(&AudioTarget::Input),
            strings(&["get-volume", "@DEFAULT_AUDIO_SOURCE@"])
        );
        assert_eq!(
            wpctl_mute_args(&AudioTarget::Output, Mute::Toggle),
            strings(&["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"])
        );
        assert_eq!(
            wpctl_mute_args(&AudioTarget::Input, Mute::On),
            strings(&["set-mute", "@DEFAULT_AUDIO_SOURCE@", "1"])
        );
        assert_eq!(
            wpctl_mute_args(&node("42"), Mute::Off),
            strings(&["set-mute", "42", "0"])
        );
    }

    #[test]
    fn pactl_is_asked_about_a_sink_or_a_source() {
        assert_eq!(
            pactl_volume_args(&AudioTarget::Output),
            strings(&["get-sink-volume", "@DEFAULT_SINK@"])
        );
        assert_eq!(
            pactl_mute_state_args(&AudioTarget::Input),
            strings(&["get-source-mute", "@DEFAULT_SOURCE@"])
        );
        assert_eq!(
            pactl_set_volume_args(&AudioTarget::Input, 47),
            strings(&["set-source-volume", "@DEFAULT_SOURCE@", "47%"])
        );
        assert_eq!(
            pactl_mute_args(&node("alsa_output.usb"), Mute::Toggle),
            strings(&["set-sink-mute", "alsa_output.usb", "toggle"])
        );
    }

    #[test]
    fn pactl_levels_are_whole_clamped_and_never_backwards() {
        assert_eq!(pactl_level(45.0, 2.0, 100.0), 47);
        assert_eq!(pactl_level(45.0, -2.0, 100.0), 43);
        // Up to the ceiling and no further.
        assert_eq!(pactl_level(99.0, 5.0, 100.0), 100);
        // Down to silence and no further.
        assert_eq!(pactl_level(1.0, -5.0, 100.0), 0);
        // A small step still moves, in the direction asked.
        assert_eq!(pactl_level(45.0, 0.5, 100.0), 46);
        assert_eq!(pactl_level(45.0, -0.5, 100.0), 44);
        // Above the ceiling already: a raise leaves it, a lower lowers it.
        assert_eq!(pactl_level(120.0, 2.0, 100.0), 120);
        assert_eq!(pactl_level(120.0, -2.0, 100.0), 118);
        assert_eq!(pactl_level(45.0, 0.0, 100.0), 45);
    }

    #[test]
    fn numbers_are_written_short() {
        assert_eq!(decimal(1.0), "1");
        assert_eq!(decimal(0.8), "0.8");
        assert_eq!(decimal(0.7), "0.7");
        assert_eq!(decimal(2.5), "2.5");
        assert_eq!(decimal(10.0), "10");
        assert_eq!(decimal(100.0), "100");
        assert_eq!(decimal(0.0), "0");
    }

    #[test]
    fn wpctl_output_is_read() {
        let level = |percent, muted| Some(Level { percent, muted });
        assert_eq!(parse_wpctl("Volume: 0.45"), level(45.0, false));
        assert_eq!(parse_wpctl("Volume: 0.45\n"), level(45.0, false));
        assert_eq!(parse_wpctl("Volume: 1.00 [MUTED]\n"), level(100.0, true));
        assert_eq!(parse_wpctl("Volume: 1.53"), level(153.0, false));
        assert_eq!(parse_wpctl("Volume: 0.00 [MUTED]"), level(0.0, true));
        assert_eq!(parse_wpctl("nonsense"), None);
        assert_eq!(parse_wpctl(""), None);
        assert_eq!(parse_wpctl("Volume: NaN"), None);
        assert_eq!(parse_wpctl("Volume: -1.00"), None);
        assert_eq!(parse_wpctl("Volume: 0,45"), None);
    }

    #[test]
    fn pactl_output_is_read() {
        assert_eq!(
            parse_pactl_volume(
                "Volume: front-left: 29491 /  45% / -20.81 dB,   front-right: 29491 /  45% / -20.81 dB\n        balance 0.00\n"
            ),
            Some(45.0)
        );
        assert_eq!(
            parse_pactl_volume("Volume: mono: 65536 / 100% / 0.00 dB\n"),
            Some(100.0)
        );
        assert_eq!(parse_pactl_volume("Failure: No such entity"), None);
        assert_eq!(parse_pactl_volume(""), None);

        assert_eq!(parse_pactl_mute("Mute: yes\n"), Some(true));
        assert_eq!(parse_pactl_mute("Mute: no"), Some(false));
        assert_eq!(parse_pactl_mute("Mute: ja"), None);
        assert_eq!(parse_pactl_mute(""), None);
    }

    #[test]
    fn wpctl_is_preferred_and_pactl_is_the_fallback() {
        let only = |name: &'static str| {
            move |program: &str| (program == name).then(|| Path::new("/opt/bin").join(program))
        };
        let installed = |tool, program: &str| Installed {
            tool,
            program: program.into(),
        };
        assert_eq!(
            choose(|program| Some(Path::new("/opt/bin").join(program))),
            Some(installed(Tool::Wpctl, "/opt/bin/wpctl"))
        );
        assert_eq!(
            choose(only(WPCTL)),
            Some(installed(Tool::Wpctl, "/opt/bin/wpctl"))
        );
        assert_eq!(
            choose(only(PACTL)),
            Some(installed(Tool::Pactl, "/opt/bin/pactl"))
        );
        assert_eq!(choose(|_| None), None);
    }

    #[test]
    fn a_program_counts_only_as_an_executable_file_on_an_absolute_path() {
        let dir = std::env::temp_dir().join(format!(
            "galdeck-audio-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("wpctl")).unwrap();
        let executable = |name: &str, mode: u32| {
            let file = dir.join(name);
            std::fs::write(&file, "").unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        executable("pactl", 0o755);
        executable("plain", 0o644);

        let path = std::env::join_paths(["/nonexistent-galdeck".as_ref(), dir.as_path()]).unwrap();
        assert_eq!(find("pactl", &path), Some(dir.join("pactl")));
        assert_eq!(find("plain", &path), None, "not executable");
        assert_eq!(find("wpctl", &path), None, "a directory");
        assert_eq!(find("missing", &path), None);
        assert_eq!(
            choose(|program| find(program, &path)),
            Some(Installed {
                tool: Tool::Pactl,
                program: dir.join("pactl")
            })
        );

        // A relative entry that leads to the very same file is still not
        // searched: where it leads depends on where the daemon was started.
        let cwd = std::env::current_dir().unwrap();
        let up: PathBuf = cwd.components().skip(1).map(|_| "..").collect();
        let relative = up.join(dir.strip_prefix("/").unwrap());
        assert!(is_executable(&relative.join("pactl")));
        assert_eq!(find("pactl", relative.as_os_str()), None);
        assert_eq!(find("pactl", OsStr::new("")), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn operations_run_the_file_that_was_found() {
        // `false` standing in for wpctl: whatever runs must be the file at
        // the path found, which the failure names. Spawned by its name
        // instead, it would be the real wpctl answering.
        let path = std::env::var_os("PATH").unwrap_or_default();
        let program = find("false", &path).expect("false is installed");
        let sound = Installed {
            tool: Tool::Wpctl,
            program: program.clone(),
        };
        let result = read_with(&sound, &AudioTarget::Output, Instant::now() + DEADLINE);
        let named = format!("{} get-volume", program.display());
        assert!(
            matches!(&result, Err(AudioError::Failed(m)) if m.starts_with(&named)),
            "{result:?}"
        );
    }

    #[test]
    fn a_deadline_too_long_for_the_clock_is_no_deadline() {
        assert_eq!(run("true", &[], Duration::MAX).unwrap(), "");
    }

    #[test]
    fn a_command_that_overruns_is_killed() {
        let started = Instant::now();
        let result = run("sleep", &strings(&["5"]), Duration::from_millis(100));
        assert!(matches!(result, Err(AudioError::TimedOut)), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_command_that_never_stops_talking_still_times_out() {
        let started = Instant::now();
        let result = run("yes", &[], Duration::from_millis(100));
        assert!(matches!(result, Err(AudioError::TimedOut)), "{result:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_program_that_is_not_installed_is_missing() {
        let result = run("galdeck-no-such-program", &[], Duration::from_secs(1));
        assert!(matches!(result, Err(AudioError::Missing)), "{result:?}");
    }

    #[test]
    fn output_is_bounded_and_a_long_answer_still_finishes() {
        // Far more than a pipe holds: it only exits if it is read as it goes.
        let output = run(
            "head",
            &strings(&["-c", "200000", "/dev/zero"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(output.len(), MAX_OUTPUT);
    }

    #[test]
    fn arguments_arrive_as_given_and_failure_is_reported() {
        let output = run("printf", &strings(&["%s|", "a b", "$(id)"]), DEADLINE).unwrap();
        assert_eq!(output, "a b|$(id)|");
        let result = run("false", &[], DEADLINE);
        assert!(matches!(result, Err(AudioError::Failed(_))), "{result:?}");
    }

    #[test]
    fn it_runs_in_the_c_locale() {
        let output = run("printenv", &strings(&["LC_ALL"]), DEADLINE).unwrap();
        assert_eq!(output, "C\n");
    }

    /// The file an installed program is at, as [`find`] finds it.
    fn program(name: &str) -> PathBuf {
        let path = std::env::var_os("PATH").unwrap_or_default();
        find(name, &path).unwrap_or_else(|| panic!("{name} is installed"))
    }

    fn soon() -> Instant {
        Instant::now() + DEADLINE
    }

    #[test]
    fn a_capped_run_keeps_all_of_an_answer_up_to_the_cap() {
        let head = program("head");
        let exactly = strings(&["-c", "131072", "/dev/zero"]);
        let output = run_capped(&head, &exactly, soon(), 128 << 10).unwrap();
        assert_eq!(output.len(), 128 << 10);
    }

    #[test]
    fn a_capped_run_refuses_an_answer_past_the_cap_rather_than_cut_it_short() {
        let head = program("head");
        let one_more = strings(&["-c", "131073", "/dev/zero"]);
        let refused = run_capped(&head, &one_more, soon(), 128 << 10);
        assert!(
            matches!(&refused, Err(AudioError::Failed(m)) if m.ends_with("printed more than 128 KiB")),
            "{refused:?}"
        );

        // One that would never stop is stopped at the cap, not the deadline.
        let started = Instant::now();
        let refused = run_capped(&program("yes"), &[], started + DEADLINE, MAX_DUMP);
        assert!(
            matches!(&refused, Err(AudioError::Failed(m)) if m.ends_with("printed more than 4 MiB")),
            "{refused:?}"
        );
        assert!(started.elapsed() < DEADLINE);
    }

    #[test]
    fn a_dump_as_large_as_the_cap_arrives_inside_the_deadline() {
        let head = program("head");
        let largest = strings(&["-c", &MAX_DUMP.to_string(), "/dev/zero"]);
        let output = run_capped(&head, &largest, soon(), MAX_DUMP).unwrap();
        assert_eq!(output.len(), MAX_DUMP);
    }

    #[test]
    fn a_capped_run_says_when_pipewire_could_not_be_reached() {
        let sh = program("sh");
        let result = run_capped(&sh, &strings(&["-c", "exit 255"]), soon(), MAX_DUMP);
        let expected = format!("{} could not connect to PipeWire", sh.display());
        assert!(
            matches!(&result, Err(AudioError::Failed(m)) if *m == expected),
            "{result:?}"
        );
        let result = run_capped(&program("false"), &[], soon(), MAX_DUMP);
        assert!(
            matches!(&result, Err(AudioError::Failed(m)) if m.ends_with("failed: exit status: 1")),
            "{result:?}"
        );
        let past = Instant::now() - Duration::from_millis(1);
        let result = run_capped(&program("true"), &[], past, MAX_DUMP);
        assert!(matches!(result, Err(AudioError::TimedOut)), "{result:?}");
    }

    #[test]
    fn sizes_are_written_the_way_people_read_them() {
        assert_eq!(size(MAX_DUMP), "4 MiB");
        assert_eq!(size(64 << 10), "64 KiB");
        assert_eq!(size(1536), "1536 bytes");
        assert_eq!(size(100), "100 bytes");
        assert_eq!(size(0), "0 bytes");
    }

    #[test]
    fn outputs_and_app_volume_need_wpctl_and_pw_dump() {
        let wpctl = Installed {
            tool: Tool::Wpctl,
            program: "/usr/bin/wpctl".into(),
        };
        let pactl = Installed {
            tool: Tool::Pactl,
            program: "/usr/bin/pactl".into(),
        };
        let dump = Path::new("/usr/bin/pw-dump");
        let tools = mixer_tools(Some(&wpctl), Some(dump)).unwrap();
        assert_eq!(tools.wpctl, Path::new("/usr/bin/wpctl"));
        assert_eq!(tools.pw_dump, dump);
        assert_eq!(
            mixer_tools(Some(&pactl), Some(dump)).unwrap_err(),
            "only pactl is installed, and this needs wpctl"
        );
        assert_eq!(
            mixer_tools(None, Some(dump)).unwrap_err(),
            "wpctl is not installed"
        );
        assert_eq!(
            mixer_tools(Some(&wpctl), None).unwrap_err(),
            "pw-dump is not installed"
        );
    }

    #[test]
    fn the_real_programs_are_run_by_path_with_the_arguments_asked_for() {
        // `echo` standing in for both, to show what each would be given.
        let echo = program("echo");
        let tools = Tools {
            wpctl: &echo,
            pw_dump: &echo,
        };
        assert_eq!(tools.dump(None, soon()).unwrap(), b"-N\n");
        assert_eq!(
            tools.dump(Some(METADATA), soon()).unwrap(),
            b"-N PipeWire:Interface:Metadata\n"
        );
        assert_eq!(
            tools
                .wpctl(&strings(&["get-volume", "46"]), soon())
                .unwrap(),
            "get-volume 46\n"
        );
    }

    mod jobs {
        use std::cell::{Cell, RefCell};
        use std::collections::{HashMap, VecDeque};

        use serde_json::json;

        use super::super::*;
        use crate::pipewire::testing::*;

        /// PipeWire, played from a script: dumps and answers given in
        /// advance, and every wpctl command written down. Nothing here
        /// reaches a real sound server.
        #[derive(Default)]
        struct Fake {
            /// What each whole dump prints, in turn; the last one again once
            /// they run out.
            dumps: RefCell<VecDeque<Vec<u8>>>,
            /// The default sink each metadata dump names, in turn, the same
            /// way.
            defaults: RefCell<VecDeque<&'static str>>,
            /// What `wpctl get-volume ID` prints. Nothing for any other id,
            /// which is how wpctl answers for a node that has gone.
            volumes: HashMap<u32, &'static str>,
            /// Every wpctl command, arguments joined by spaces.
            commands: RefCell<Vec<String>>,
            /// How many whole dumps were asked for.
            looked: Cell<usize>,
        }

        impl Fake {
            fn new(dumps: Vec<Vec<u8>>) -> Fake {
                Fake {
                    dumps: RefCell::new(dumps.into()),
                    defaults: RefCell::new(VecDeque::from([SPEAKER_NAME])),
                    ..Fake::default()
                }
            }

            fn defaults(self, names: &[&'static str]) -> Fake {
                *self.defaults.borrow_mut() = names.iter().copied().collect();
                self
            }

            fn volume(mut self, id: u32, answer: &'static str) -> Fake {
                self.volumes.insert(id, answer);
                self
            }

            fn commands(&self) -> Vec<String> {
                self.commands.borrow().clone()
            }
        }

        fn next<T: Clone>(queue: &RefCell<VecDeque<T>>) -> T {
            let mut queue = queue.borrow_mut();
            if queue.len() > 1 {
                queue.pop_front().unwrap()
            } else {
                queue.front().cloned().expect("an answer was given")
            }
        }

        impl Mixer for Fake {
            fn dump(&self, only: Option<&str>, _until: Instant) -> Result<Vec<u8>, AudioError> {
                match only {
                    None => {
                        self.looked.set(self.looked.get() + 1);
                        Ok(next(&self.dumps))
                    }
                    Some(kind) => {
                        assert_eq!(kind, METADATA);
                        Ok(dump(&[default_metadata(41, next(&self.defaults))]))
                    }
                }
            }

            fn wpctl(&self, args: &[String], _until: Instant) -> Result<String, AudioError> {
                self.commands.borrow_mut().push(args.join(" "));
                let answer = match args {
                    [verb, id] if verb == "get-volume" => id
                        .parse()
                        .ok()
                        .and_then(|id| self.volumes.get(&id).copied()),
                    _ => None,
                };
                Ok(answer.unwrap_or_default().to_string())
            }
        }

        fn soon() -> Instant {
            Instant::now() + DEADLINE
        }

        fn level(percent: f64, muted: bool) -> Level {
            Level { percent, muted }
        }

        /// This machine, plus a virtual sink that can always be heard.
        fn with_loopback() -> Vec<u8> {
            with(&[sink(
                200,
                "loopback_out",
                json!({ "node.nick": "Loopback", "priority.session": 100 }),
            )])
        }

        /// This machine, plus Firefox playing through three streams.
        fn with_firefox(ids: [u32; 3]) -> Vec<u8> {
            with(&[
                stream(ids[0], json!("Firefox"), Some("firefox"), "running"),
                stream(ids[1], json!("Firefox"), Some("firefox"), "idle"),
                stream(ids[2], json!("Firefox"), Some("firefox"), "idle"),
            ])
        }

        #[test]
        fn a_switch_on_this_machine_stays_on_its_only_output() {
            let fake = Fake::new(vec![DUMP.into()]).volume(SPEAKER, "Volume: 0.20\n");
            let outcome = change_output(&fake, Choice::Step(1), &[], soon()).unwrap();
            assert_eq!(
                outcome,
                OutputOutcome {
                    display: "Speaker".into(),
                    index: 0,
                    count: 1,
                    level: Some(level(20.0, false)),
                    changed: false,
                }
            );
            assert_eq!(fake.commands(), ["get-volume 46"]);
        }

        #[test]
        fn a_switch_makes_the_next_output_the_default_and_waits_until_it_is() {
            let fake = Fake::new(vec![with_loopback()])
                .defaults(&[SPEAKER_NAME, SPEAKER_NAME, "loopback_out"])
                .volume(200, "Volume: 1.00 [MUTED]\n");
            let outcome = change_output(&fake, Choice::Step(1), &[], soon()).unwrap();
            assert_eq!(
                outcome,
                OutputOutcome {
                    display: "Loopback".into(),
                    index: 1,
                    count: 2,
                    level: Some(level(100.0, true)),
                    changed: true,
                }
            );
            // Its level read by its id, never @DEFAULT_AUDIO_SINK@, which
            // could still have been the speaker.
            assert_eq!(fake.commands(), ["set-default 200", "get-volume 200"]);
        }

        #[test]
        fn an_output_that_never_becomes_the_default_is_a_failure() {
            let fake = Fake::new(vec![with_loopback()]);
            let started = Instant::now();
            let result = change_output(&fake, Choice::Step(-1), &[], soon());
            assert!(
                matches!(&result, Err(AudioError::Failed(m)) if m == "Loopback did not become the default output"),
                "{result:?}"
            );
            assert!(started.elapsed() >= DEFAULT_WAIT - DEFAULT_POLL);
            assert!(started.elapsed() < DEADLINE);
            assert_eq!(fake.commands(), ["set-default 200"]);
        }

        #[test]
        fn an_output_that_cannot_be_heard_is_never_made_the_default() {
            let fake = Fake::new(vec![DUMP.into()]);
            let refused = change_output(&fake, Choice::Named("Headphones"), &[], soon());
            assert!(
                matches!(&refused, Err(AudioError::Failed(m)) if m == "Headphones is not connected"),
                "{refused:?}"
            );
            let order = ["HDMI".to_string()];
            let refused = change_output(&fake, Choice::Step(1), &order, soon());
            assert!(
                matches!(&refused, Err(AudioError::Failed(m)) if m == "none of the outputs in galdeck.toml can be heard"),
                "{refused:?}"
            );
            assert!(fake.commands().is_empty());
        }

        #[test]
        fn setting_the_output_that_is_already_the_default_only_reads_it() {
            let fake = Fake::new(vec![with_loopback()]).volume(SPEAKER, "Volume: 0.35\n");
            let outcome = change_output(&fake, Choice::Named("speaker"), &[], soon()).unwrap();
            assert_eq!((outcome.index, outcome.count), (0, 2));
            assert!(!outcome.changed);
            assert_eq!(outcome.level, Some(level(35.0, false)));
            assert_eq!(fake.commands(), ["get-volume 46"]);
        }

        #[test]
        fn a_level_that_cannot_be_read_does_not_undo_a_switch() {
            let fake = Fake::new(vec![with_loopback()]).defaults(&["loopback_out"]);
            let outcome = change_output(&fake, Choice::Named("Loopback"), &[], soon()).unwrap();
            assert!(outcome.changed);
            assert_eq!(outcome.level, None);
        }

        #[test]
        fn app_volume_moves_every_stream_of_the_app_and_raising_unmutes_it() {
            let fake = Fake::new(vec![with_firefox([201, 202, 203])]).volume(201, "Volume: 0.42\n");
            let outcome = adjust_app_with(&fake, Some("Firefox"), 2.0, 100.0, soon()).unwrap();
            assert_eq!(
                outcome,
                AppOutcome::Found {
                    app: "firefox".into(),
                    display: "Firefox".into(),
                    index: 0,
                    count: 2,
                    level: level(42.0, false),
                }
            );
            assert_eq!(
                fake.commands(),
                [
                    "set-mute 201 0",
                    "set-volume -l 1 201 2%+",
                    "set-mute 202 0",
                    "set-volume -l 1 202 2%+",
                    "set-mute 203 0",
                    "set-volume -l 1 203 2%+",
                    "get-volume 201",
                ]
            );
        }

        #[test]
        fn lowering_app_volume_leaves_mute_alone() {
            let fake = Fake::new(vec![with_firefox([201, 202, 203])]).volume(201, "Volume: 0.10\n");
            adjust_app_with(&fake, Some("firefox"), -5.0, 100.0, soon()).unwrap();
            assert_eq!(
                fake.commands(),
                [
                    "set-volume -l 1 201 5%-",
                    "set-volume -l 1 202 5%-",
                    "set-volume -l 1 203 5%-",
                    "get-volume 201",
                ]
            );
        }

        #[test]
        fn with_no_app_playing_an_untargeted_job_changes_nothing() {
            // This machine's only stream is open and silent.
            let fake = Fake::new(vec![DUMP.into()]);
            assert_eq!(
                adjust_app_with(&fake, None, 2.0, 100.0, soon()).unwrap(),
                AppOutcome::NoApp { app: None }
            );
            assert_eq!(
                toggle_app_mute_with(&fake, None, soon()).unwrap(),
                AppOutcome::NoApp { app: None }
            );
            assert_eq!(
                adjust_app_with(&fake, Some("Spotify"), 2.0, 100.0, soon()).unwrap(),
                AppOutcome::NoApp {
                    app: Some("Spotify".into())
                }
            );
            assert!(fake.commands().is_empty());
        }

        #[test]
        fn an_untargeted_job_takes_the_app_that_is_playing() {
            let fake = Fake::new(vec![with_firefox([201, 202, 203])]).volume(201, "Volume: 0.50\n");
            let outcome = adjust_app_with(&fake, None, 0.0, 100.0, soon()).unwrap();
            assert!(
                matches!(&outcome, AppOutcome::Found { app, .. } if app == "firefox"),
                "{outcome:?}"
            );
            // A step of nothing changes nothing, but still reads.
            assert_eq!(fake.commands(), ["get-volume 201"]);
        }

        #[test]
        fn a_stream_that_ends_during_the_job_sends_it_looking_once_more() {
            let fake = Fake::new(vec![
                with_firefox([201, 202, 203]),
                with_firefox([204, 202, 203]),
            ])
            .volume(204, "Volume: 0.30\n");
            let outcome = adjust_app_with(&fake, Some("firefox"), -2.0, 100.0, soon()).unwrap();
            assert_eq!(
                outcome,
                AppOutcome::Found {
                    app: "firefox".into(),
                    display: "Firefox".into(),
                    index: 0,
                    count: 2,
                    level: level(30.0, false),
                }
            );
            assert_eq!(fake.looked.get(), 2);
            // The change is not made twice: the second look only reads.
            let commands = fake.commands();
            assert_eq!(&commands[3..], ["get-volume 201", "get-volume 204"]);
        }

        #[test]
        fn an_app_whose_streams_all_end_is_reported_gone() {
            let fake = Fake::new(vec![with_firefox([201, 202, 203]), DUMP.into()]);
            let outcome = adjust_app_with(&fake, None, 2.0, 100.0, soon()).unwrap();
            assert_eq!(
                outcome,
                AppOutcome::NoApp {
                    app: Some("firefox".into())
                }
            );
            assert_eq!(fake.looked.get(), 2);
        }

        #[test]
        fn an_app_that_is_not_there_is_named_only_as_it_may_be_shown() {
            // What a knob remembers is whatever name a client gave itself.
            let odd = "Evil\u{202e}\n\u{7}Player";
            let fake = Fake::new(vec![DUMP.into()]);
            assert_eq!(
                adjust_app_with(&fake, Some(odd), 2.0, 100.0, soon()).unwrap(),
                AppOutcome::NoApp {
                    app: Some("Evil Player".into())
                }
            );

            // Found, and then gone before it could be read.
            let fake = Fake::new(vec![
                with(&[stream(201, json!(odd), None, "running")]),
                DUMP.into(),
            ]);
            assert_eq!(
                toggle_app_mute_with(&fake, None, soon()).unwrap(),
                AppOutcome::NoApp {
                    app: Some("Evil Player".into())
                }
            );
            assert_eq!(fake.commands(), ["get-volume 201"]);
        }

        #[test]
        fn app_mute_sets_every_stream_to_the_opposite_of_the_first() {
            let fake = Fake::new(vec![with_firefox([201, 202, 203])]).volume(201, "Volume: 0.40\n");
            toggle_app_mute_with(&fake, Some("firefox"), soon()).unwrap();
            assert_eq!(
                fake.commands(),
                [
                    "get-volume 201",
                    "set-mute 201 1",
                    "set-mute 202 1",
                    "set-mute 203 1",
                    "get-volume 201",
                ]
            );

            let fake = Fake::new(vec![with_firefox([201, 202, 203])])
                .volume(201, "Volume: 0.40 [MUTED]\n");
            let outcome = toggle_app_mute_with(&fake, Some("firefox"), soon()).unwrap();
            assert_eq!(
                &fake.commands()[1..4],
                ["set-mute 201 0", "set-mute 202 0", "set-mute 203 0"]
            );
            assert!(matches!(outcome, AppOutcome::Found { .. }), "{outcome:?}");
        }

        #[test]
        fn app_mute_on_a_stream_that_has_ended_looks_again_before_setting_anything() {
            let fake = Fake::new(vec![
                with_firefox([201, 202, 203]),
                with_firefox([204, 202, 203]),
            ])
            .volume(204, "Volume: 0.40\n");
            toggle_app_mute_with(&fake, Some("firefox"), soon()).unwrap();
            assert_eq!(
                fake.commands(),
                [
                    "get-volume 201",
                    "get-volume 204",
                    "set-mute 204 1",
                    "set-mute 202 1",
                    "set-mute 203 1",
                    "get-volume 204",
                ]
            );
        }

        #[test]
        fn next_app_moves_on_from_the_current_one_and_changes_nothing() {
            let fake = Fake::new(vec![with_firefox([201, 202, 203])]).volume(103, "Volume: 1.00\n");
            let outcome = next_app_with(&fake, Some("firefox"), soon()).unwrap();
            assert_eq!(
                outcome,
                AppOutcome::Found {
                    app: "sd_dummy".into(),
                    display: "speech-dispatcher-dummy".into(),
                    index: 1,
                    count: 2,
                    level: level(100.0, false),
                }
            );
            assert_eq!(fake.commands(), ["get-volume 103"]);

            let fake = Fake::new(vec![dump(&[])]);
            assert_eq!(
                next_app_with(&fake, Some("firefox"), soon()).unwrap(),
                AppOutcome::NoApp { app: None }
            );
        }

        #[test]
        fn the_editor_is_offered_every_output_and_app() {
            let fake = Fake::new(vec![with_firefox([201, 202, 203])]);
            let targets = targets_with(&fake, soon()).unwrap();
            assert_eq!(targets.outputs.len(), 5);
            let apps: Vec<&str> = targets.apps.iter().map(|app| app.app.as_str()).collect();
            assert_eq!(apps, ["firefox", "sd_dummy"]);
            assert!(fake.commands().is_empty());
        }
    }
}

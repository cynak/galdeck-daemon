//! The device engine: owns the HID handle on one thread, applies pages,
//! dispatches input events to actions, and services control requests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use galdeck::{Buttons, Encoders, Event, Rgb};
use galdeck_core::{Clock, DeadlineCell, Scheduler, Tick, WakeReceiver, Waker};
use galdeck_device::{KeyTarget, Paint};
// The framework's calibrated geometry. Aliased because `Layout` is already
// taken here by the protocol's page description, which is a different thing
// entirely -- one is where the keys physically are, the other is what is on
// them.
use galdeck::layout::{Grid as PanelGrid, Layout as PanelLayout, Rect as PanelRect};
use galdeck_ipc::{
    AnimationInfo, CalRect, CalSource, CalZone, Calibration, ConfigFile, ConfigSnapshot,
    DeckDevice, EncoderInfo, KeyInfo, Layout, LcdGrid, Patch, Request, Response, Status, TileInfo,
    WidgetInfo,
};

use crate::actions::ActionRunner;
use crate::countdown::Countdown;
use crate::input::{Bindings, Decision, Gesture, InputMachine};
use crate::io::DeviceMsg;
use crate::plugins::{PluginEvent, PluginHost};
use crate::preview::Preview;
use crate::widgets::{draw, Sample, Slot, SlotState, WidgetHost};

use crate::render;
use crate::ring::RingFeedback;
use crate::states::{KeyCycle, KeyHome};
use galdeck_model::v2::{EncoderConfig, KeyConfig, Page, Profile};
use galdeck_model::{
    config_file_names, Animation, ConfigDocument, Diagnostics, ResolvedPalette, ResolvedStyle,
    StyleLayer, Workspace,
};

/// Longest the core loop sleeps with nothing scheduled.
///
/// Nothing depends on this for correctness: every real deadline is registered
/// with the scheduler and every message rings the waker. It only bounds how
/// long shutdown takes to notice.
const CORE_IDLE: Duration = Duration::from_millis(250);
/// Most often the whole screen is sent, whatever asks for it.
///
/// Every repaint is a full frame -- 70 KB over a moving background -- and
/// turning a knob asks for one per detent, for the level shown over the
/// screen. The module's firmware drops off the bus under that on top of a
/// moving background (seen on 3.05.005), so a repaint asked for sooner
/// waits for this.
const SCREEN_FRAME_MIN: Duration = Duration::from_millis(50);
/// A background moving on the screen at least this often sets the pace of
/// the screen: anything else that changes it waits for the background's
/// next frame and goes out with that, so moving it costs no frames of its
/// own, and the screen never falls out of step with the keys.
const SCREEN_RIDES_BACKGROUND: Duration = Duration::from_millis(100);
/// How many pages back a `back` key can walk.
///
/// A deck is navigated by hand, so this is far past any real trail; the point
/// is only that the stack cannot grow for as long as the daemon runs.
const MAX_PAGE_STACK: usize = 32;
/// How many animation frames a page may pre-render before it is worth saying
/// something. Each is a JPEG encode on the page-switch path.
const MAX_PRERENDERED_FRAMES: usize = 64;
/// Cap on commands spawned for one coalesced rotation report.
const MAX_DETENTS_PER_EVENT: u32 = 8;
/// Depth of one encoder's rotation queue. Deep enough to absorb a fast
/// spin, shallow enough that a slow action cannot build a backlog the
/// knob keeps paying off after the user has stopped turning.
const ROTATION_QUEUE_DEPTH: usize = 16;
/// Space between tiles on the info screen, and between a tile and the edge.
const TILE_GAP: u32 = 6;
/// Corner radius of a tile's card.
const TILE_RADIUS: u32 = 12;
/// How often a key's answer to a press is drawn again as it fades. Each
/// step is a JPEG encode, so a fifth of a second is five or six of them.
const PRESS_STEP: Duration = Duration::from_millis(40);

/// A patch value as the `toml` crate spells it.
fn toml_value(value: galdeck_model::Value) -> toml::Value {
    match value {
        galdeck_model::Value::String(s) => toml::Value::String(s),
        galdeck_model::Value::Integer(i) => toml::Value::Integer(i),
        galdeck_model::Value::Float(f) => toml::Value::Float(f),
        galdeck_model::Value::Boolean(b) => toml::Value::Boolean(b),
        galdeck_model::Value::Array(items) => {
            toml::Value::Array(items.into_iter().map(toml_value).collect())
        }
    }
}

/// Where an action came from, which decides where its feedback goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum From {
    Key(u8),
    Knob(u8),
    /// The configuration UI's "Try it".
    Editor,
    /// A timer that has finished, running its `on_done`. Its key may be on
    /// a page that is not showing, so nothing is fed back to a key.
    Timer,
    /// A key entering one of its states, running what the state runs. Its
    /// page may not be showing either, and the state is the feedback.
    State(u8),
}

/// The knob an action came from, if it came from one: where a report on it
/// goes back to.
fn knob(from: From) -> Option<u8> {
    match from {
        From::Knob(encoder) => Some(encoder),
        _ => None,
    }
}

/// Whether a built-in can act from where it was fired, and if not, what to
/// say instead. Some act on the key or knob they are bound to -- its timer,
/// its modes -- and anywhere else there is nothing for them to act on.
fn misplaced(action: galdeck_model::BuiltIn, from: From) -> Option<String> {
    use galdeck_model::SlotRule;
    match (action.slot_rule(), from) {
        // What a state runs as the key enters it stepping the key on again
        // would never stop.
        (SlotRule::KeyTapOnly | SlotRule::KeyOnly, From::State(_)) => {
            Some("a state's action cannot change the key's state".into())
        }
        (SlotRule::KeyTapOnly | SlotRule::KeyOnly, From::Knob(_) | From::Timer) => {
            Some(format!("{} works only on a key", action.name()))
        }
        (SlotRule::KnobOnly, From::Key(_) | From::Timer | From::State(_)) => {
            Some(format!("{} works only on a knob", action.name()))
        }
        _ => None,
    }
}

/// Whether a built-in switches outputs or sets an app's volume, which it
/// refuses to do unless `audio::mixer` finds wpctl and pw-dump. The editor
/// says so beside it, from the catalog.
///
/// Every variant is named rather than caught by `_`, so a new built-in has
/// to be put on one side or the other.
fn needs_mixer(action: galdeck_model::BuiltIn) -> bool {
    use galdeck_model::BuiltIn::*;
    match action {
        NextOutput | PreviousOutput | SetOutput | AppVolumeUp | AppVolumeDown | AppMute
        | NextApp => true,
        VolumeUp | VolumeDown | VolumeMute | MicUp | MicDown | MicMute | PushToTalk | PlayPause
        | NextTrack | PreviousTrack | SeekForward | SeekBackward | NextPage | PreviousPage
        | HomePage | NextProfile | PreviousProfile | StartProfile | DeckBrighter | DeckDimmer
        | NextMode | TimerToggle | TimerReset | ScrollUp | ScrollDown | ScrollLeft
        | ScrollRight | ZoomIn | ZoomOut | ZoomReset | NextState | PreviousState => false,
    }
}

/// What a key's widget counts, as a countdown's length: `Ok(None)` for a
/// stopwatch. The error is what to tell someone who taps it anyway.
fn countdown_length(
    widget: Option<&galdeck_model::Widget>,
) -> Result<Option<Duration>, &'static str> {
    match widget.map(|w| (w.kind, w.duration())) {
        Some((galdeck_model::WidgetKind::Timer, Some(length))) => Ok(Some(length)),
        Some((galdeck_model::WidgetKind::Timer, None)) => Err("Timer has no valid duration"),
        Some((galdeck_model::WidgetKind::Stopwatch, _)) => Ok(None),
        _ => Err("No timer on this key"),
    }
}

/// A countdown as the editor describes it.
fn timer_info(countdown: &Countdown, now: Tick) -> galdeck_ipc::TimerInfo {
    let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    let state = if countdown.is_done() {
        "done"
    } else if countdown.running() {
        "running"
    } else if countdown.display(now).paused {
        "paused"
    } else {
        "stopped"
    };
    galdeck_ipc::TimerInfo {
        state: state.into(),
        remaining_ms: countdown.remaining(now).map(ms),
        elapsed_ms: ms(countdown.elapsed(now)),
    }
}

/// What the screen says when a timer finishes: its title, else its key's
/// label, else how long it ran.
fn done_text(widget: &galdeck_model::Widget, label: Option<&str>) -> String {
    let name = [widget.title.as_deref(), label, widget.duration.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|name| !name.is_empty());
    match name {
        Some(name) => format!("Timer done · {name}"),
        None => "Timer done".into(),
    }
}

/// How strongly a finished timer's key is tinted at `now`: fully in the
/// bright half of its flash and once the flash is over, not at all in the
/// dark half. `None` for a timer that has not finished.
fn alarm_share(countdown: &Countdown, now: Tick) -> Option<f32> {
    if !countdown.is_done() {
        return None;
    }
    let dark = countdown.flashing(now) && !countdown.flash_on(now);
    Some(if dark { 0.0 } else { ALARM_SHARE })
}

/// A countdown's time as a widget reading: the text, and for a timer the
/// seconds left, which a bar or gauge fills to against the timer's length.
fn countdown_reading(countdown: &Countdown, now: Tick) -> crate::widgets::Reading {
    crate::widgets::Reading::Value {
        text: countdown.display(now).text,
        value: countdown.remaining(now).map(|left| left.as_secs_f64()),
    }
}

/// What a key's tap does to a device, for showing that device's state on
/// the key: a mute key shows whether it is muted, and a push-to-talk key
/// whether the microphone is live.
#[derive(Clone, Debug, PartialEq, Eq)]
enum StateKey {
    Mute(crate::audio::AudioTarget),
    Talk,
}

impl StateKey {
    fn target(&self) -> crate::audio::AudioTarget {
        match self {
            StateKey::Mute(target) => target.clone(),
            StateKey::Talk => crate::audio::AudioTarget::Input,
        }
    }
}

/// Whether a key's tap, its own or its widget's, mutes a device or holds
/// the microphone open.
fn state_key(cfg: &KeyConfig) -> Option<StateKey> {
    use galdeck_model::BuiltIn;
    let tap = cfg.tap()?;
    let invocation = tap.as_built_in()?;
    match invocation.action {
        BuiltIn::VolumeMute => crate::audio::AudioTarget::parse(invocation.target.as_deref())
            .ok()
            .map(StateKey::Mute),
        BuiltIn::MicMute => Some(StateKey::Mute(crate::audio::AudioTarget::Input)),
        BuiltIn::PushToTalk => Some(StateKey::Talk),
        _ => None,
    }
}

/// A colour laid over a whole key to say what state it is in, and a glyph
/// in its corner to say which.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Overlay {
    color: Rgb,
    /// How far towards `color`, 0 to 1. Zero is still an overlay: a finished
    /// timer in the dark half of its flash owns its key all the same.
    share: f32,
    glyph: Option<draw::Glyph>,
}

/// `@warning` and `@critical` for a theme that does not define them.
const WARNING: Rgb = Rgb::new(235, 203, 139);
pub(crate) const CRITICAL: Rgb = Rgb::new(191, 97, 106);

/// The key at `which`, found in whatever profile and page it is in: a
/// timer's, or a key with states. The page is the first with its id, the
/// only one [`Engine::home_page`] names.
fn key_at_home<'a>(workspace: &'a Workspace, which: &KeyHome) -> Option<&'a KeyConfig> {
    workspace
        .profile(&which.profile)?
        .pages
        .iter()
        .find(|page| page.id == which.page)?
        .keys
        .iter()
        .find(|key| key.key == which.key)
}

/// Where each mode of a stack rests, given the colour each names (if any)
/// and the knob's own `base`: what a mode names, else the knob's own for the
/// first mode, else the first of [`MODE_RINGS`] far from every colour the
/// stack already shows, so no two modes look alike.
fn mode_rings(named: &[Option<Rgb>], base: Rgb) -> Vec<Rgb> {
    let distance = |a: Rgb, b: Rgb| {
        let d = |x: u8, y: u8| u32::from(x.abs_diff(y));
        d(a.r, b.r) + d(a.g, b.g) + d(a.b, b.b)
    };
    let mut shown: Vec<Rgb> = named.iter().flatten().copied().collect();
    shown.push(named.first().copied().flatten().unwrap_or(base));
    let mut rests = Vec::with_capacity(named.len());
    for (mode, own) in named.iter().enumerate() {
        let rest = match (mode, own) {
            (_, Some(own)) => *own,
            (0, None) => base,
            _ => {
                let nearest = |c: &Rgb| shown.iter().map(|s| distance(*c, *s)).min();
                let pick = MODE_RINGS
                    .iter()
                    .find(|c| nearest(c).is_none_or(|d| d > 60))
                    // Every one is near something shown: the one nearest
                    // to nothing.
                    .or_else(|| MODE_RINGS.iter().max_by_key(|c| nearest(c)))
                    .copied()
                    .unwrap_or(base);
                shown.push(pick);
                pick
            }
        };
        rests.push(rest);
    }
    rests
}

/// The sound level a knob's turn moves, if it moves one.
fn knob_audio_target(plan: &galdeck_model::EncoderPlan) -> Option<crate::audio::AudioTarget> {
    use galdeck_model::BuiltIn;
    let (action, _) = plan.cw.as_ref().or(plan.ccw.as_ref())?;
    let invocation = action.as_built_in()?;
    match invocation.action {
        BuiltIn::MicUp | BuiltIn::MicDown => Some(crate::audio::AudioTarget::Input),
        BuiltIn::VolumeUp | BuiltIn::VolumeDown => {
            crate::audio::AudioTarget::parse(invocation.target.as_deref()).ok()
        }
        _ => None,
    }
}

/// A knob's press, while it is down.
#[derive(Clone, Debug, Default)]
struct KnobState {
    down: bool,
    /// Turned while pressed: the press was a grip, not a click.
    turned: bool,
    hold_fired: bool,
    /// What the page the knob went down on binds, so a press that changes
    /// page is not answered a second time by the page it lands on.
    press: Option<galdeck_model::Action>,
    hold: Option<galdeck_model::Action>,
}

/// A knob mid-spin through pages or profiles, and what it was bound to when
/// the spin began.
#[derive(Clone, Debug)]
struct NavLatch {
    cw: Option<galdeck_model::Action>,
    ccw: Option<galdeck_model::Action>,
    until: Tick,
}

/// A message over the bottom of the screen.
#[derive(Clone, Debug)]
struct Osd {
    text: String,
    /// A bar to draw, 0 to 1, and whether it is muted.
    level: Option<(f32, bool)>,
    /// Goes as soon as the deck is touched, rather than only when its time
    /// is up.
    until_input: bool,
}

/// How long a message stays on the screen: a while, or until someone is
/// back at the deck to read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OsdHold {
    /// This long.
    For(Duration),
    /// Until the deck is next touched, and this long at most.
    UntilInput(Duration),
}

/// How long the on-screen message stays.
const OSD_HOLD: Duration = Duration::from_millis(1500);
/// How long "Timer done" stays when the timer's key is showing, and flashing
/// to say so too.
const TIMER_DONE_HOLD: Duration = Duration::from_secs(5);
/// The longest "Timer done" waits for someone to come back to the deck when
/// the timer's page is not showing, and the message is all there is.
const TIMER_DONE_WAIT: Duration = Duration::from_secs(60);
/// How far a finished timer's key is tinted towards `@critical`.
const ALARM_SHARE: f32 = 0.6;
/// How far a mute key showing a muted device, or a push-to-talk key over a
/// live microphone, is tinted.
const STATE_SHARE: f32 = 0.45;
/// How far a key whose state's command has just failed is tinted, for the
/// moment it says so.
const FAILED_SHARE: f32 = 0.3;
/// Most animation frames a page pre-renders. Past it, a key's animation is
/// left out and the key shows its first frame, still: twelve keys of the
/// longest animations would otherwise be close to four hundred JPEG encodes
/// on every page switch.
const MAX_PAGE_FRAMES: usize = 192;
/// Most names one `which` asks about, and the longest name looked for.
const MAX_WHICH: usize = 64;
const MAX_PROGRAM_NAME: usize = 64;
/// Most editors left waiting for the icon names while the themes are still
/// being found.
const MAX_ICON_NAME_WAITERS: usize = 16;
/// How often the levels behind mute keys and level rings are read again
/// while any is showing: muting from the desktop, a headset button or a call
/// app says nothing to the deck, and a mute key showing "muted" over a live
/// microphone is worse than one showing nothing.
const LEVEL_POLL: Duration = Duration::from_secs(2);
/// How long after a spin through pages a knob keeps spinning through pages,
/// whatever the page it landed on binds it to.
const NAV_LATCH: Duration = Duration::from_millis(600);
/// How long after one track, output or app change a knob waits before
/// allowing another: a spin changes one, not one per detent.
const SWITCH_DEBOUNCE: Duration = Duration::from_millis(300);
/// How long a knob is held to switch its mode. Longer than a key's hold,
/// because a knob with modes always has one, so a press meant to mute that
/// lingers would otherwise change what turning does; well short of the three
/// seconds Corsair's own software asks for.
const KNOB_MODE_HOLD: Duration = Duration::from_millis(650);
/// Where a knob's ring rests in a mode that names no colour of its own: the
/// first of these that looks like no colour the knob's other modes already
/// show -- the first mode's own included, which on Nord is this teal -- so
/// the ring says which mode is on without being turned. Six, because at most
/// two of them are near any one colour and a knob has at most four modes.
const MODE_RINGS: [Rgb; 6] = [
    Rgb::new(0x88, 0xc0, 0xd0),
    Rgb::new(0xeb, 0xcb, 0x8b),
    Rgb::new(0xb4, 0x8e, 0xad),
    Rgb::new(0xd0, 0x87, 0x70),
    Rgb::new(0xa3, 0xbe, 0x8c),
    Rgb::new(0x5e, 0x81, 0xac),
];
/// How long a level report still counts as the answer to a knob's question.
const LEVEL_REPORT_WINDOW: Duration = Duration::from_secs(3);
/// How long after a media action the media widgets look again.
const MEDIA_RESAMPLE: Duration = Duration::from_millis(150);
/// The dimmest a built-in will take the deck: dark enough to be dark, light
/// enough that the keys can still be read to turn it back up.
const MIN_DECK_BRIGHTNESS: u8 = 5;
/// Left control, for ctrl+wheel zoom.
const CTRL: u16 = 29;

/// "64%", or "muted".
fn percent_text(level: crate::audio::Level) -> String {
    if level.muted {
        "muted".into()
    } else {
        format!("{:.0}%", level.percent)
    }
}

/// A level as the on-screen bar draws it.
fn level_bar(level: crate::audio::Level) -> (f32, bool) {
    ((level.percent / 100.0) as f32, level.muted)
}

/// "Output · Headphones · 64%", or "Only output · Speaker" when there is
/// nothing else to switch to, so a turn that changes nothing says why.
fn output_text(display: &str, count: usize, level: Option<crate::audio::Level>) -> String {
    if count <= 1 {
        return format!("Only output · {display}");
    }
    match level {
        Some(level) => format!("Output · {display} · {}", percent_text(level)),
        None => format!("Output · {display}"),
    }
}

/// "Left knob", "Right knob".
fn knob_name(encoder: u8) -> String {
    match encoder {
        0 => "Left knob".into(),
        1 => "Right knob".into(),
        n => format!("Knob {n}"),
    }
}

/// "Left knob 2/3 · App volume".
fn mode_text(encoder: u8, mode: usize, count: usize, preset: galdeck_model::Preset) -> String {
    format!(
        "{} {}/{count} · {}",
        knob_name(encoder),
        mode + 1,
        preset.title()
    )
}

/// An app a knob has acted on, remembered so its next turn acts on it again.
#[derive(Clone, Debug, PartialEq)]
struct KnobApp {
    /// What the app is known by, as the services worker found it: what a
    /// job asks for it by.
    key: String,
    /// Its name to show, for saying when it has stopped playing.
    display: String,
}

/// Where `save_asset` or `fetch_asset` put a picture, as the protocol says it.
fn asset_response(stored: Result<std::path::PathBuf, String>) -> Response {
    match stored {
        Ok(path) => Response::Asset {
            path: path.display().to_string(),
        },
        Err(message) => Response::Error { message },
    }
}

/// What `audio_targets` found, as the protocol says it.
fn targets_response(found: crate::controls::Found) -> Response {
    let text = |value: String| (!value.is_empty()).then_some(value);
    match found {
        Ok(targets) => Response::AudioTargets {
            outputs: targets
                .outputs
                .into_iter()
                .map(|output| galdeck_ipc::OutputInfo {
                    display: output.display,
                    name: output.name,
                    nick: text(output.nick),
                    description: text(output.description),
                    usable: output.usable,
                    default: output.default,
                })
                .collect(),
            apps: targets
                .apps
                .into_iter()
                .map(|app| galdeck_ipc::AppInfo {
                    app: app.app,
                    display: app.display,
                    binary: text(app.binary),
                    running: app.running,
                })
                .collect(),
        },
        Err(message) => Response::Error { message },
    }
}

/// "Volume 45%", "Microphone muted".
fn level_text(target: &crate::audio::AudioTarget, percent: f64, muted: bool) -> String {
    let name = match target {
        crate::audio::AudioTarget::Output => "Volume".to_string(),
        crate::audio::AudioTarget::Input => "Microphone".to_string(),
        crate::audio::AudioTarget::Node(node) => node.clone(),
    };
    if muted {
        format!("{name} muted")
    } else {
        format!("{name} {percent:.0}%")
    }
}

fn ring_shows_name(shows: galdeck_model::RingShows) -> String {
    match shows {
        galdeck_model::RingShows::OutputLevel => "output_level",
        galdeck_model::RingShows::InputLevel => "input_level",
        galdeck_model::RingShows::DeckBrightness => "deck_brightness",
        galdeck_model::RingShows::PagePosition => "page_position",
        galdeck_model::RingShows::ProfilePosition => "profile_position",
        galdeck_model::RingShows::OutputPosition => "output_position",
        galdeck_model::RingShows::AppLevel => "app_level",
    }
    .into()
}

/// An action taken apart for an editor.
fn action_info(
    action: &galdeck_model::Action,
    origin: Option<galdeck_model::Layer>,
) -> galdeck_ipc::ActionInfo {
    use galdeck_model::Action;
    let (kind, command, built_in, keys, step, target) = match action {
        Action::Shell(command) => ("shell", Some(command.clone()), None, None, None, None),
        Action::Keys(chord) => ("keys", None, None, Some(chord.clone()), None, None),
        Action::BuiltIn(i) => (
            "builtin",
            None,
            Some(i.action.name().to_string()),
            None,
            i.step,
            i.target.clone(),
        ),
    };
    galdeck_ipc::ActionInfo {
        kind: kind.into(),
        command,
        action: built_in,
        keys,
        step,
        target,
        label: action.describe(),
        origin: origin.map(|layer| layer.name().to_string()),
    }
}

/// One of a knob's modes, for an editor, with the colour its ring rests at.
fn mode_info(entry: &galdeck_model::ModeEntry, ring: Option<Rgb>) -> galdeck_ipc::ModeInfo {
    galdeck_ipc::ModeInfo {
        preset: entry.preset.name().into(),
        title: entry.preset.title().into(),
        step: entry.step,
        target: entry.target.clone(),
        ring: ring.map(hex),
    }
}

/// A layer's own look, resolved for an editor: its ring colour, its
/// animation, and each of its modes with the colour it names, if any.
struct LayerLook {
    ring: Option<String>,
    animation: Option<AnimationInfo>,
    modes: Vec<galdeck_ipc::ModeInfo>,
}

/// One layer's entry for a knob, for an editor.
fn layer_info(
    layer: &str,
    file: &str,
    path: &str,
    config: &EncoderConfig,
    look: LayerLook,
) -> galdeck_ipc::EncoderLayerInfo {
    let info = |slot: &Option<galdeck_model::Action>| slot.as_ref().map(|a| action_info(a, None));
    let LayerLook {
        ring,
        animation,
        modes,
    } = look;
    galdeck_ipc::EncoderLayerInfo {
        layer: layer.into(),
        file: file.into(),
        path: path.into(),
        preset: config.preset.map(|p| p.name().to_string()),
        step: config.step,
        modes,
        target: config.target.clone(),
        gestures: galdeck_ipc::GestureInfo {
            press: info(&config.press),
            cw: info(&config.cw),
            ccw: info(&config.ccw),
            hold: info(&config.hold),
        },
        ring,
        animation,
    }
}

/// Text a widget shows before it has read anything, in a preview.
fn default_label(widget: &galdeck_model::Widget) -> &str {
    widget.placeholder.as_deref().unwrap_or(widget.kind.name())
}

/// Where a tile's grid cells land on a screen of `size`, less the gap.
///
/// Edges are rounded from exact fractions of the screen rather than a cell
/// size multiplied up: 720 does not divide by 7, and multiplying a rounded
/// width would leave the last column short by up to a pixel per column.
fn tile_area(
    cells: galdeck_model::Cells,
    grid: galdeck_model::LcdGrid,
    size: (u32, u32),
) -> draw::Area {
    let (width, height) = size;
    let edge = |index: u8, count: u8, size: u32| {
        (u32::from(index) * size + u32::from(count) / 2) / u32::from(count.max(1))
    };
    let (left, right) = (
        edge(cells.column, grid.columns, width),
        edge(cells.column + cells.columns, grid.columns, width),
    );
    let (top, bottom) = (
        edge(cells.row, grid.rows, height),
        edge(cells.row + cells.rows, grid.rows, height),
    );
    let half = TILE_GAP / 2;
    draw::Area::new(
        (left + half) as i32,
        (top + half) as i32,
        (right - left).saturating_sub(TILE_GAP),
        (bottom - top).saturating_sub(TILE_GAP),
    )
}

/// How round a tile's card is: as its widget or its look says, else the
/// usual, and never past half its shorter side.
fn tile_radius(widget: &galdeck_model::Widget, area: draw::Area) -> u32 {
    widget
        .radius()
        .unwrap_or(TILE_RADIUS)
        .min(area.width.min(area.height) / 2)
}

/// A sender that always wakes the core loop.
///
/// The loop sleeps until its next scheduled deadline, so a request that is
/// merely queued would not be looked at until that fires -- which is how a
/// ping ends up taking a quarter of a second. Bundling the waker with the
/// channel makes it impossible for a caller to forget.
#[derive(Clone)]
pub struct ControlSender {
    tx: Sender<ControlMsg>,
    waker: Waker,
}

impl ControlSender {
    pub fn new(tx: Sender<ControlMsg>, waker: Waker) -> Self {
        Self { tx, waker }
    }

    pub fn send(&self, msg: ControlMsg) -> Result<(), std::sync::mpsc::SendError<ControlMsg>> {
        self.tx.send(msg)?;
        self.waker.notify();
        Ok(())
    }
}

/// Everything the engine needs that is not configuration.
///
/// A struct rather than ten more parameters: the list had reached twelve and
/// every new one meant editing four call sites that did not otherwise care.
pub struct EngineParts {
    pub control_rx: Receiver<ControlMsg>,
    pub device_rx: Receiver<DeviceMsg>,
    pub paint_tx: SyncSender<Paint>,
    pub widget_rx: Receiver<Sample>,
    pub wake: WakeReceiver,
    pub deadline: Arc<DeadlineCell>,
    pub clock: Arc<dyn Clock>,
    pub shutdown: Arc<AtomicBool>,
    pub preview: Preview,
    pub widget_host: WidgetHost,
    pub plugin_host: PluginHost,
    pub plugin_rx: Receiver<PluginEvent>,
    pub parked: Arc<AtomicBool>,
    /// Draw keys at their calibrated rectangles rather than through the
    /// firmware's key path. Ignored while the calibration is only a template.
    pub zone_paint: bool,
    /// Where the calibration is read from. `None` takes the default path,
    /// which is under `$XDG_CONFIG_HOME`.
    ///
    /// Given explicitly rather than always resolved from the environment so
    /// that a test can say which calibration it is testing against. Reading
    /// it from an ambient path made whether the suite passed depend on
    /// whether the machine running it happened to own a Stream Deck.
    pub calibration_path: Option<PathBuf>,
}

/// A control request paired with its reply channel.
pub struct ControlMsg {
    pub request: Request,
    pub reply: Sender<Response>,
}

/// Where a key's pixels go, and how big they are.
///
/// The module has two ways to put an image on a key and they are not
/// interchangeable. The `02 07` key path takes an index and a fixed-size
/// image that the firmware places and clips itself; it is cheap, it needs no
/// geometry, and on a unit whose keys measure larger than
/// [`galdeck::ids::KEY_PIXELS`] it physically cannot reach the edges. The
/// `02 0c` region path takes a rectangle, which only a calibration knows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum KeyPlacement {
    /// The firmware places it. No calibration needed, and no way to fill a
    /// key bigger than the image.
    Firmware,
    /// A measured rectangle on the panel.
    Zone {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
}

impl KeyPlacement {
    fn size(self) -> (u32, u32) {
        match self {
            KeyPlacement::Firmware => galdeck::Button::size(),
            KeyPlacement::Zone { width, height, .. } => (u32::from(width), u32::from(height)),
        }
    }

    /// Wrap an encoded image in the target that addresses it this way.
    fn target(self, jpeg: Arc<[u8]>) -> KeyTarget {
        match self {
            KeyPlacement::Firmware => KeyTarget::Jpeg(jpeg),
            KeyPlacement::Zone {
                x,
                y,
                width,
                height,
            } => KeyTarget::Region {
                x,
                y,
                width,
                height,
                jpeg,
            },
        }
    }
}

/// Where the info screen is drawn.
///
/// Like a key, the screen has the firmware's fixed segment and the region
/// path. The segment is 720x384 at the origin whatever the glass shows; a
/// calibration measures what the glass actually shows, which is the area
/// tiles have to be laid out on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ScreenPlacement {
    /// The firmware's segment.
    Firmware,
    /// A measured screen. `visible` is what gets laid out and previewed;
    /// `frame` is what is written, in whole region blocks, and contains it.
    Measured {
        visible: PanelRect,
        frame: PanelRect,
    },
}

impl ScreenPlacement {
    /// The screen as a calibration describes it.
    ///
    /// The frame covers the measured screen by rounding its far edges out to
    /// whole blocks, so a 396-row screen is written 400 rows tall and the
    /// extra rows fall under the bezel. Rounding out must not reach the first
    /// key row, though: the screen repaints every second under a clock, and
    /// would keep painting over the tops of the keys. Where it would, the
    /// frame is cut to the block below instead and the screen is laid out on
    /// what is left.
    fn from_calibration(calibration: &PanelLayout) -> Self {
        // A template is arithmetic, as for keys, and its screen is the
        // segment anyway.
        if calibration.source == galdeck::layout::Source::Template {
            return ScreenPlacement::Firmware;
        }
        let measured = calibration.screen;
        if measured == PanelRect::new(0, 0, galdeck::Lcd::WIDTH, galdeck::Lcd::HEIGHT) {
            return ScreenPlacement::Firmware;
        }
        let block = galdeck::ids::REGION_MCU as u16;
        if measured.width < block || measured.height < block {
            return ScreenPlacement::Firmware;
        }
        let mut frame = measured.to_mcu_covering();
        let first_key = calibration
            .zones()
            .iter()
            .map(|zone| u32::from(zone.bounds.y))
            .filter(|&top| top >= u32::from(measured.y))
            .min();
        if let Some(first_key) = first_key {
            if frame.bottom() > first_key {
                frame.y = measured.y;
                frame.height = galdeck::Panel::to_mcu_floor(measured.height);
            }
        }
        let visible = PanelRect::new(
            measured.x,
            measured.y,
            measured
                .width
                .min((frame.right() - u32::from(measured.x)) as u16),
            measured
                .height
                .min((frame.bottom() - u32::from(measured.y)) as u16),
        );
        ScreenPlacement::Measured { visible, frame }
    }

    /// The size tiles, text and the background are laid out at.
    fn size(self) -> (u32, u32) {
        match self {
            ScreenPlacement::Firmware => {
                let (width, height) = galdeck::Lcd::size();
                (u32::from(width), u32::from(height))
            }
            ScreenPlacement::Measured { visible, .. } => {
                (u32::from(visible.width), u32::from(visible.height))
            }
        }
    }
}

/// Widget pictures by path and the size they were scaled to.
type WidgetImages = std::collections::HashMap<(PathBuf, u32, u32), Option<Arc<image::RgbaImage>>>;

/// What a prepared background was built from.
type SceneSource = (galdeck_model::Backdrop, Vec<Rgb>, crate::backdrop::Geometry);

/// The icon themes found for a reload, and the names in them once an editor
/// has asked for those: listing them reads every directory the themes list,
/// so it is done once per reload, on a thread, and only when asked.
#[derive(Clone)]
struct IconSet {
    themes: Arc<crate::icons::IconThemes>,
    names: Arc<std::sync::OnceLock<Vec<String>>>,
}

/// Themes found off the engine's thread, and the reload they were found for.
type FoundIcons = (u64, Arc<crate::icons::IconThemes>);

/// Whether a request can be answered where it was handled.
enum Reply {
    /// Send this and move on, which is every request but one. Boxed
    /// because a `Response` can carry a whole page layout, and the other
    /// variant is nothing at all.
    Now(Box<Response>),
    /// Say nothing yet. The answer is owed when the io thread confirms the
    /// device handle is closed, and until then the caller is left waiting on
    /// purpose -- that wait is the whole guarantee.
    WhenReleased,
}

pub struct Engine {
    config_dir: PathBuf,
    workspace: Workspace,
    font: Option<galdeck::Font>,
    /// Which profile is showing, and where in it.
    profile_id: String,
    page_index: usize,
    /// Pages visited, so a key bound to `back` can return.
    ///
    /// Bounded: a deck is navigated by hand, and an unbounded stack would
    /// grow for as long as the daemon runs.
    page_stack: Vec<usize>,
    /// Every config file, kept as the document it was written as so edits
    /// preserve comments and ordering.
    documents: std::collections::BTreeMap<String, ConfigDocument>,
    /// The current profile's theme, folded and with its palette resolved.
    /// Recomputed only when the profile or the config changes.
    theme: StyleLayer,
    palette: ResolvedPalette,
    /// The current profile's `[motion]`, with its theme's folded in.
    motion: galdeck_model::MotionStyle,
    /// When each key position was pressed, while the theme's answer to the
    /// press is still fading. Positions, not keys: forgotten when the page
    /// changes, since a position is another key then.
    pressed: [Option<Tick>; Buttons::COUNT as usize],
    /// Whether an alarm frame is due. Kept, rather than asked of the
    /// scheduler, so a widget sampling faster than the alarm moves cannot
    /// keep putting its next frame off.
    alarm_frames: bool,
    brightness: u8,
    /// What the io thread last told us about the device. The engine never
    /// touches it directly.
    device: DeviceStatus,
    control_rx: Receiver<ControlMsg>,
    device_rx: Receiver<DeviceMsg>,
    paint_tx: SyncSender<Paint>,
    deadline: Arc<DeadlineCell>,
    clock: Arc<dyn Clock>,
    wake: WakeReceiver,
    shutdown: Arc<AtomicBool>,
    /// Set while another process holds the device; read by the io thread.
    parked: Arc<AtomicBool>,
    /// Callers waiting to be told the device handle is actually closed.
    ///
    /// A list rather than one slot because nothing stops two clients asking
    /// at once, and the second must not be left hanging for a Disconnected
    /// that has already been consumed.
    pending_release: Vec<Sender<Response>>,
    /// Where the keys physically are on this unit, and where that was read
    /// from. Held rather than read per request so the UI's drawing and the
    /// daemon's own view cannot drift apart.
    calibration: PanelLayout,
    calibration_path: PathBuf,
    /// Why the saved calibration could not be read, when it could not.
    calibration_problem: Option<String>,
    scheduler: Scheduler<TimerKind>,
    /// What the deck is showing, and who is watching. Shared with the UI
    /// server so a browser sees the same JPEGs the panel was sent.
    preview: Preview,
    /// One serialized action runner per encoder; see [`RotationRunner`].
    rotation: Vec<RotationRunner>,
    /// Ring turn-feedback state, one per encoder.
    rings: Vec<RingFeedback>,
    /// Pre-rendered key animations, one slot per key.
    key_animations: Vec<Option<KeyAnimation>>,
    /// Ring animations, one slot per encoder.
    ring_animations: Vec<Option<RingAnimation>>,
    /// The widget runner, and what each widget on the page last read. Every
    /// slot with a widget has an entry, read or not, so the map is also the
    /// list of timers to cancel when the page goes.
    widget_host: WidgetHost,
    widget_rx: Receiver<Sample>,
    widget_state: std::collections::BTreeMap<Slot, SlotState>,
    /// Bumped whenever the page is applied, so a sample asked for by the page
    /// before cannot land on whatever occupies its slot now.
    widget_generation: u64,
    /// The background behind the page, prepared, and what it was prepared
    /// from: a page switch that keeps the same background -- the usual case,
    /// when it comes from the theme -- must not load the picture again.
    scene: Option<crate::backdrop::Scene>,
    scene_source: Option<SceneSource>,
    /// The frame of it showing now, and which frame that is.
    scene_frame: Option<Arc<image::RgbImage>>,
    scene_tick: u64,
    /// The screen has something new to show. Widgets and the background
    /// set this rather than painting, and the loop paints once a pass: three
    /// animated tiles ticking together would otherwise encode the whole
    /// screen three times over, ten times a second.
    screen_dirty: bool,
    /// Keystrokes, scrolling, sound and media: the work done outside the
    /// daemon, on workers of their own.
    controls: crate::controls::ControlHost,
    report_rx: Receiver<crate::controls::ControlReport>,
    /// Each knob's press, for hold and for a press that was really a grip.
    knobs: Vec<KnobState>,
    /// A knob mid-spin through pages keeps spinning through pages, whatever
    /// the page it lands on binds it to.
    nav_latch: Vec<Option<NavLatch>>,
    /// The deck key holding the microphone open for push-to-talk.
    talking: Option<u8>,
    /// The message over the bottom of the screen after a knob turns.
    osd: Option<Osd>,
    /// The last levels the sound server reported, to predict the next from
    /// and to show on mute keys. Only ever what was reported, never a guess:
    /// a key saying "muted" is a claim about the microphone.
    levels: std::collections::HashMap<crate::audio::AudioTarget, crate::audio::Level>,
    /// Devices whose last read failed, so a missing sound tool is said once
    /// rather than at every poll.
    unread: std::collections::HashSet<crate::audio::AudioTarget>,
    /// Every timer and stopwatch that has been started, on any page of any
    /// profile, so one keeps counting while its page is not showing. One at
    /// its start is not kept: it has nothing to remember.
    countdowns: std::collections::HashMap<KeyHome, Countdown>,
    /// Keys showing a finished timer's flash at the last flash tick, so the
    /// tick after its flash ends paints it at rest.
    flashing: Vec<u8>,
    /// Every key with states that has been pressed, read or set, on any page
    /// of any profile: the state it shows and what is under way for it. By
    /// where the key is, so it keeps its state while its page is not showing
    /// and through a reload. Not kept on disk: a key that can read its state
    /// reads it again, and one that cannot would only be guessing.
    key_states: std::collections::HashMap<KeyHome, KeyCycle>,
    /// Runs what keys with states run, off this thread.
    state_runner: crate::states::StateRunner,
    state_rx: Receiver<crate::states::StateReport>,
    /// The number the next of those jobs goes by.
    state_seq: u64,
    /// Which knob last asked about a level, so the answer lands on its ring.
    level_asker: Option<(u8, crate::audio::AudioTarget, Tick)>,
    /// When each knob last changed track, output or app.
    last_switch: Vec<Option<Tick>>,
    /// Which mode each knob's modes are in, by the knob, the layer that
    /// lists them and the list itself. Not per page: a knob's modes set once
    /// in galdeck.toml stay in the mode they were left in across every page
    /// and profile, including through pages that bind the knob to something
    /// else. A list that changes is a new list, and starts at its first.
    dial_modes:
        std::collections::HashMap<(u8, galdeck_model::Layer, Vec<galdeck_model::ModeEntry>), usize>,
    /// The app each knob last acted on, so turning it again moves the same
    /// app rather than whichever happens to be playing. Never in `knobs`,
    /// which forgets at every page.
    knob_apps: Vec<Option<KnobApp>>,
    /// Apps' levels as last reported, by what each app is known by.
    app_levels: std::collections::HashMap<String, crate::audio::Level>,
    /// The outputs and apps there are, looked up for an editor.
    audio_targets: crate::controls::AudioTargets,
    /// Place search for weather widgets, shared with the threads that run it.
    geocoder: Arc<crate::widgets::weather::Geocoder>,
    /// Widget pictures, scaled, by path and size. A media tile repaints
    /// every second and an animated background ten times that; decoding a
    /// PNG each time would be most of the work. `None` remembers a picture
    /// that could not be loaded, so it is not retried on every frame.
    widget_images: std::cell::RefCell<WidgetImages>,
    /// The icon themes a key's icon name is looked up in, once found. Found
    /// off this thread, since asking the desktop which one it uses can take
    /// a second; until then a named icon is left out.
    icons: Option<IconSet>,
    icons_tx: Sender<FoundIcons>,
    icons_rx: Receiver<FoundIcons>,
    /// The reload whose themes are wanted. Ones found for an earlier reload
    /// that arrive late are not.
    icons_wanted: u64,
    /// Whether themes are being found now. One search at a time: the
    /// desktop is asked which theme it uses at most once at a time, and a
    /// second ask while the first waits would be told nothing.
    icons_finding: bool,
    /// Editors that asked for the icon names before there were themes to
    /// list them from.
    icon_names_waiting: Vec<Sender<Response>>,
    /// Icons drawn to fit their keys, so neither a repaint nor an animation
    /// frame decodes a file again.
    icon_cache: std::cell::RefCell<crate::icons::IconCache>,
    /// Wakes the loop from the threads the engine starts.
    waker: Waker,
    /// Plugins, and what they have put on their keys.
    plugin_host: PluginHost,
    plugin_rx: Receiver<PluginEvent>,
    plugin_text: Vec<Option<String>>,
    plugin_color: Vec<Option<Rgb>>,
    /// Which keys each plugin currently has, so it can be told when they go.
    plugin_visible: std::collections::BTreeMap<String, Vec<u8>>,
    /// Turns edges into taps, holds and double taps.
    input: InputMachine,
    /// Launches key actions, and reaps them centrally rather than one
    /// parked thread at a time.
    actions: ActionRunner,
    /// Flat black key images, one per distinct key size in use.
    ///
    /// Encoded once and reused; see `blank_target` for why it is an image at
    /// all. Keyed by size because a calibrated deck draws keys at their
    /// measured rectangle, and the firmware path's square is a different
    /// picture from a 176x176 one.
    blank_keys: std::collections::BTreeMap<(u32, u32), Arc<[u8]>>,
    /// Whether to draw keys at their calibrated rectangles.
    ///
    /// On unless the operator says otherwise, but only takes effect where
    /// there is a measured calibration to draw at -- see `key_placement`.
    zone_paint: bool,
    /// The keyboard lighting thread, told what the profile showing asks of
    /// the keyboard whenever that changes. `None` until main attaches one.
    lighting: Option<crate::lighting::LightingHandle>,
    /// When the screen was last sent, and which background frame it showed,
    /// so the next repaint can wait its turn; see `SCREEN_FRAME_MIN`.
    screen_painted: Option<(Tick, u64)>,
}

/// What the io thread has told us about the device.
#[derive(Default, Debug)]
struct DeviceStatus {
    connected: bool,
    firmware: Option<String>,
    serial: Option<String>,
}

/// Something that wants to happen later.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum TimerKind {
    /// A ring's turn or click feedback has run its course.
    RingRest { encoder: u8 },
    /// The next frame of a key's animation is due.
    KeyFrame { key: u8 },
    /// The next frame of a ring's animation is due.
    RingFrame { encoder: u8 },
    /// A widget is due to be sampled again.
    WidgetTick { slot: Slot },
    /// A knob has been held down long enough to count as held.
    KnobHold { encoder: u8 },
    /// The on-screen message has been up long enough.
    OsdEnd,
    /// The next frame of an animated background is due.
    BackdropFrame,
    /// A gesture on this key needs deciding: a hold has matured, or a
    /// double-tap window has closed.
    Gesture { key: u8 },
    /// The first running timer to finish is due to. One deadline for all of
    /// them, on any page, set again whenever one starts, stops or goes.
    CountdownDue,
    /// A finished timer's key on the page is due to change its flash.
    CountdownFlash,
    /// The levels behind the mute keys and level rings showing are due to
    /// be read again.
    LevelPoll,
    /// A key with states on the page is due to have its state read again.
    StateStatus { key: u8 },
    /// Something about a key with states is due: the cue that its command
    /// is still running, the end of the flash saying it failed, a read to
    /// see it took. One deadline for all of them, on any page, as with
    /// `CountdownDue`.
    KeyStates,
    /// The next step of a key's answer to being pressed, or its end.
    PressFrame { key: u8 },
    /// The next step of the pulse on widgets past their threshold.
    AlarmFrame,
    /// A repaint of the screen held back by `SCREEN_FRAME_MIN` may go.
    ScreenFrame,
}

/// A key animation, with every frame already rendered and encoded.
///
/// Playing one is a channel send: the CPU was spent once, when the page was
/// applied. Re-encoding per frame would be about a millisecond each, which
/// twelve animated keys would turn into a third of a core.
struct KeyAnimation {
    frames: Vec<Arc<[u8]>>,
    interval: Duration,
    next: u8,
}

/// A ring animation. Not pre-rendered, because a ring frame is four colours
/// and no encoding at all -- which also frees it from stepping through a
/// key's few frames: each frame is worked out when it is due, from how far
/// into the cycle that is, so the colours move instead of jumping.
struct RingAnimation {
    animation: Animation,
    base: Rgb,
    to: Rgb,
    interval: Duration,
    /// When the cycle began. Every frame's phase and due time are measured
    /// from it, so a late frame neither shows a stale colour nor pushes the
    /// ones after it later.
    start: Tick,
}

impl RingAnimation {
    /// The colours at `now`.
    fn frame(&self, now: Tick) -> [Rgb; galdeck::Ring::SEGMENTS as usize] {
        let phase = crate::ring::animation_phase(&self.animation, self.start, now);
        crate::ring::animation_frame(self.animation.kind, phase, self.base, self.to)
    }

    /// When the frame after `now` is due.
    fn next_frame(&self, now: Tick) -> Tick {
        crate::ring::next_animation_frame(self.start, self.interval, now)
    }
}

/// Which deck the daemon drives.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DeviceMode {
    /// Use the keyboard if it is present, and keep retrying if it is not.
    #[default]
    Auto,
    /// A deck that exists only in memory. Everything above the seam behaves
    /// identically, which is what lets the configuration UI be developed and
    /// tested on a machine with no keyboard attached.
    Virtual,
}

impl Engine {
    pub fn new(config_dir: PathBuf, workspace: Workspace, parts: EngineParts) -> Result<Self> {
        let EngineParts {
            control_rx,
            device_rx,
            paint_tx,
            widget_rx,
            wake,
            deadline,
            clock,
            shutdown,
            preview,
            widget_host,
            plugin_host,
            plugin_rx,
            parked,
            zone_paint,
            calibration_path,
        } = parts;
        let font = render::load_font(workspace.global.font.as_deref());
        let brightness = workspace.global.brightness;
        let (controls, report_rx) =
            crate::controls::ControlHost::new(widget_host.waker(), workspace.global.virtual_input);
        let (state_runner, state_rx) = crate::states::StateRunner::new(widget_host.waker());
        let (icons_tx, icons_rx) = std::sync::mpsc::channel();
        let waker = widget_host.waker();
        let profile_id = workspace
            .start_profile()
            .map(str::to_string)
            .unwrap_or_default();
        let mut engine = Engine {
            config_dir,
            workspace,
            font,
            profile_id,
            page_index: 0,
            page_stack: Vec::new(),
            documents: Default::default(),
            theme: StyleLayer::default(),
            palette: ResolvedPalette::default(),
            motion: Default::default(),
            pressed: Default::default(),
            alarm_frames: false,
            brightness,
            device: DeviceStatus::default(),
            control_rx,
            device_rx,
            paint_tx,
            deadline,
            clock,
            wake,
            shutdown,
            parked,
            pending_release: Vec::new(),
            calibration: PanelLayout::TEMPLATE,
            calibration_path: calibration_path.unwrap_or_else(PanelLayout::default_path),
            calibration_problem: None,
            scheduler: Scheduler::new(),
            preview,
            rotation: Encoders::indices().map(|_| RotationRunner::new()).collect(),
            rings: Encoders::indices().map(|_| RingFeedback::new()).collect(),
            key_animations: (0..Buttons::COUNT).map(|_| None).collect(),
            ring_animations: Encoders::indices().map(|_| None).collect(),
            widget_host,
            widget_rx,
            widget_state: Default::default(),
            widget_generation: 0,
            scene: None,
            scene_source: None,
            scene_frame: None,
            scene_tick: 0,
            screen_dirty: false,
            controls,
            report_rx,
            knobs: Encoders::indices().map(|_| KnobState::default()).collect(),
            nav_latch: Encoders::indices().map(|_| None).collect(),
            talking: None,
            osd: None,
            levels: Default::default(),
            unread: Default::default(),
            countdowns: Default::default(),
            flashing: Vec::new(),
            key_states: Default::default(),
            state_runner,
            state_rx,
            state_seq: 0,
            level_asker: None,
            last_switch: Encoders::indices().map(|_| None).collect(),
            dial_modes: Default::default(),
            knob_apps: Encoders::indices().map(|_| None).collect(),
            app_levels: Default::default(),
            audio_targets: Default::default(),
            geocoder: Arc::new(crate::widgets::weather::Geocoder::default()),
            widget_images: Default::default(),
            icons: None,
            icons_tx,
            icons_rx,
            icons_wanted: 0,
            icons_finding: false,
            icon_names_waiting: Vec::new(),
            icon_cache: Default::default(),
            waker,
            plugin_host,
            plugin_rx,
            plugin_text: (0..Buttons::COUNT).map(|_| None).collect(),
            plugin_color: (0..Buttons::COUNT).map(|_| None).collect(),
            plugin_visible: Default::default(),
            input: InputMachine::new(),
            actions: ActionRunner::new(),
            blank_keys: Default::default(),
            zone_paint,
            lighting: None,
            screen_painted: None,
        };
        engine.enter_profile();
        engine.load_documents();
        engine.load_calibration();
        engine.prepare_virtual_input();
        engine.find_icons();
        Ok(engine)
    }

    /// Read every config file as an editable document.
    ///
    /// Kept alongside the parsed model rather than derived from it: the model
    /// is what the daemon runs on, the documents are what the user wrote, and
    /// only the latter can be edited without losing comments.
    fn load_documents(&mut self) {
        self.documents.clear();
        for name in config_file_names(&self.config_dir) {
            match ConfigDocument::load(&self.config_dir.join(&name)) {
                Ok(document) => {
                    self.documents.insert(name, document);
                }
                Err(diagnostics) => {
                    log::warn!("{name}: {}", diagnostics.render());
                }
            }
        }
    }

    /// An animation's settings, with its colour resolved through the palette
    /// so an editor shows what it will look like rather than `@accent`.
    fn animation_info(&self, animation: &Animation) -> AnimationInfo {
        AnimationInfo {
            kind: format!("{:?}", animation.kind).to_lowercase(),
            period_ms: animation.period_ms(),
            to: animation.to.as_ref().and_then(|color| {
                self.palette
                    .resolve(color, "animation.to", &mut Diagnostics::new())
                    .map(hex)
            }),
            frames: animation.frames(),
        }
    }

    /// The current page, resolved, with the config path of every control.
    fn layout(&self) -> Option<Layout> {
        let profile = self.current_profile()?;
        let page = self.current_page()?;
        let mut out = Diagnostics::new();
        let grid = Workspace::grid_for(profile, page);

        let keys = page
            .keys
            .iter()
            .enumerate()
            .map(|(index, key)| {
                let (style, from) = self.workspace.style_for(
                    &self.theme,
                    &self.palette,
                    profile,
                    page,
                    Some(&key.style),
                    "key",
                    &mut out,
                );
                let cycle = self
                    .home_of(key.key)
                    .and_then(|home| self.key_states.get(&home));
                KeyInfo {
                    key: key.key,
                    index,
                    label: key.label.clone(),
                    text: self.label_for(&self.look_of(key)),
                    widget: key.widget.as_ref().map(|w| self.widget_info(w)),
                    animation: key.animation.as_ref().map(|a| self.animation_info(a)),
                    icon: key.icon.as_ref().map(ToString::to_string),
                    exec: key.exec.as_ref().and_then(|a| a.shell()).map(String::from),
                    tap: key.exec.as_ref().map(|a| action_info(a, None)).or_else(|| {
                        key.implicit_tap().map(|a| galdeck_ipc::ActionInfo {
                            kind: "implicit".into(),
                            ..action_info(&a, None)
                        })
                    }),
                    hold: key.hold.as_ref().map(|a| action_info(a, None)).or_else(|| {
                        key.implicit_hold().map(|a| galdeck_ipc::ActionInfo {
                            kind: "implicit".into(),
                            ..action_info(&a, None)
                        })
                    }),
                    double: key.double.as_ref().map(|a| action_info(a, None)),
                    page: key.page.clone(),
                    profile: key.profile.clone(),
                    back: key.back,
                    background: hex(style.key_bg),
                    background_is_own: from.key_bg == galdeck_model::StyleSource::Cell,
                    label_color: hex(style.key_label_color),
                    label_color_is_own: from.key_label_color == galdeck_model::StyleSource::Cell,
                    timer: self
                        .countdown_of(key)
                        .map(|countdown| timer_info(&countdown, self.clock.now())),
                    states: key.states.iter().map(|s| self.state_info(s)).collect(),
                    state: self
                        .home_of(key.key)
                        .and_then(|home| self.shown_state(&home, key)),
                    state_known: cycle.is_some_and(|cycle| cycle.known),
                    status: key.status.clone(),
                    status_interval_ms: key.status_interval_ms,
                    status_result: cycle
                        .and_then(|cycle| cycle.last_read.as_ref())
                        .map(|read| galdeck_ipc::StatusResultInfo {
                            output: read.output.clone(),
                            error: read.error.clone(),
                            ok: read.ok,
                            age_ms: u64::try_from(
                                self.clock.now().duration_since(read.at).as_millis(),
                            )
                            .unwrap_or(u64::MAX),
                        }),
                }
            })
            .collect();

        let profile_file = format!("profiles/{}.toml", self.profile_id);
        let encoders = Encoders::indices()
            .map(|encoder| {
                let plan = self.encoder_plan(encoder);
                let own = page.encoders.iter().position(|e| e.encoder == encoder);
                let mut layers = Vec::new();
                let entries = [
                    (
                        "global",
                        "galdeck.toml".to_string(),
                        "encoders",
                        &self.workspace.global.encoders,
                    ),
                    (
                        "profile",
                        profile_file.clone(),
                        "encoders",
                        &profile.encoders,
                    ),
                ];
                // A layer's own colour, animation and modes, as written but
                // with colours resolved, for the editor.
                let own_look = |config: &EncoderConfig| LayerLook {
                    ring: config.style.ring.as_ref().and_then(|color| {
                        self.palette
                            .resolve(color, "ring", &mut Diagnostics::new())
                            .map(hex)
                    }),
                    animation: config.animation.as_ref().map(|a| self.animation_info(a)),
                    modes: config
                        .modes
                        .iter()
                        .map(|entry| mode_info(entry, self.mode_color(entry)))
                        .collect(),
                };
                for (layer, file, array, list) in entries {
                    if let Some(i) = list.iter().position(|e| e.encoder == encoder) {
                        layers.push(layer_info(
                            layer,
                            &file,
                            &format!("{array}[{i}]"),
                            &list[i],
                            own_look(&list[i]),
                        ));
                    }
                }
                if let Some(i) = own {
                    layers.push(layer_info(
                        "page",
                        &profile_file,
                        &format!("pages[{}].encoders[{i}]", self.page_index),
                        &page.encoders[i],
                        own_look(&page.encoders[i]),
                    ));
                }
                let (base, _) = self.workspace.style_for(
                    &self.theme,
                    &self.palette,
                    profile,
                    page,
                    None,
                    "encoder",
                    &mut Diagnostics::new(),
                );
                let (style, from) = self.workspace.style_for(
                    &self.theme,
                    &self.palette,
                    profile,
                    page,
                    Some(&plan.style),
                    "encoder",
                    &mut out,
                );
                let tagged = |slot: &Option<(galdeck_model::Action, galdeck_model::Layer)>| {
                    slot.as_ref()
                        .map(|(action, layer)| action_info(action, Some(*layer)))
                };
                let shell = |action: &Option<galdeck_model::Action>| {
                    action.as_ref().and_then(|a| a.shell()).map(String::from)
                };
                let page_entry = own.map(|i| &page.encoders[i]);
                // A hold no layer wrote is the one modes bring, which an
                // editor shows as what holding does unless told otherwise,
                // as it shows a timer key's.
                let hold_written = layers.iter().any(|layer| layer.gestures.hold.is_some());
                let hold = tagged(&plan.hold).map(|info| {
                    if hold_written {
                        info
                    } else {
                        galdeck_ipc::ActionInfo {
                            kind: "implicit".into(),
                            ..info
                        }
                    }
                });
                EncoderInfo {
                    encoder,
                    index: own,
                    press: page_entry.and_then(|e| shell(&e.press)),
                    cw: page_entry.and_then(|e| shell(&e.cw)),
                    ccw: page_entry.and_then(|e| shell(&e.ccw)),
                    ring: hex(style.ring),
                    ring_is_own: from.ring == galdeck_model::StyleSource::Cell,
                    animation: plan.animation.as_ref().map(|a| self.animation_info(a)),
                    resolved: galdeck_ipc::GestureInfo {
                        press: tagged(&plan.press),
                        cw: tagged(&plan.cw),
                        ccw: tagged(&plan.ccw),
                        hold,
                    },
                    turn_preset: plan
                        .turn_preset
                        .map(|(preset, _)| preset.name().to_string()),
                    ring_shows: plan.ring().map(ring_shows_name),
                    layers,
                    base_ring: Some(hex(base.ring)),
                    modes: plan
                        .modes
                        .iter()
                        .flat_map(|stack| {
                            let rests = self.mode_rests(&stack.entries, style.ring);
                            stack.entries.iter().zip(rests)
                        })
                        .map(|(entry, rest)| mode_info(entry, Some(rest)))
                        .collect(),
                    mode: plan.modes.as_ref().map(|_| plan.mode),
                }
            })
            .collect();

        Some(Layout {
            profile: self.profile_id.clone(),
            file: format!("profiles/{}.toml", self.profile_id),
            page: page.id.clone(),
            page_index: self.page_index.min(profile.pages.len().saturating_sub(1)),
            pages: profile.pages.iter().map(|p| p.id.clone()).collect(),
            keys,
            encoders,
            can_go_back: !self.page_stack.is_empty(),
            lcd_text: page.lcd_text.clone(),
            lcd: page
                .lcd
                .iter()
                .enumerate()
                .map(|(index, tile)| (index, tile, tile.cells(grid)))
                .map(|(index, tile, cells)| TileInfo {
                    index,
                    column: cells.column,
                    row: cells.row,
                    columns: cells.columns,
                    rows: cells.rows,
                    widget: self.widget_info(&tile.widget),
                    text: u8::try_from(index)
                        .ok()
                        .and_then(|t| self.widget_state.get(&Slot::Tile(t)))
                        .and_then(|state| state.reading.as_ref()?.label()),
                })
                .collect(),
            background: self.backdrop_info(profile, page),
            lcd_grid: Some({
                let (width, height) = ScreenPlacement::from_calibration(&self.calibration).size();
                LcdGrid {
                    columns: grid.columns,
                    rows: grid.rows,
                    width: width as u16,
                    height: height as u16,
                }
            }),
            lcd_grid_origin: if page.lcd_columns.is_some() || page.lcd_rows.is_some() {
                Some("page".into())
            } else if profile.lcd_columns.is_some() || profile.lcd_rows.is_some() {
                Some("profile".into())
            } else {
                None
            },
            outputs: self.workspace.global.outputs.clone(),
            warnings: self.icon_warnings(page),
        })
    }

    /// One of a key's states, for an editor: as written, with the colours it
    /// sets of its own resolved.
    fn state_info(&self, state: &galdeck_model::v2::KeyState) -> galdeck_ipc::KeyStateInfo {
        let own = |color: Option<&galdeck_model::ColorRef>| {
            color
                .and_then(|color| {
                    self.palette
                        .resolve(color, "states.style", &mut Diagnostics::new())
                })
                .map(hex)
        };
        galdeck_ipc::KeyStateInfo {
            name: state.name.clone(),
            matches: state.matches.clone(),
            label: state.label.clone(),
            icon: state.icon.as_ref().map(ToString::to_string),
            exec: state.exec.as_ref().map(|a| action_info(a, None)),
            background: own(state.style.key_bg.as_ref()),
            label_color: own(state.style.key_label_color.as_ref()),
            animation: state.animation.as_ref().map(|a| self.animation_info(a)),
        }
    }

    /// The page's background, for an editor, and where it was set.
    fn backdrop_info(&self, profile: &Profile, page: &Page) -> Option<galdeck_ipc::BackdropInfo> {
        let backdrop = self.workspace.background_for(profile, page)?;
        let origin = if page.background.is_some() {
            "page"
        } else if profile.background.is_some() {
            "profile"
        } else {
            "theme"
        };
        let token = |color: &galdeck_model::ColorRef| match color {
            galdeck_model::ColorRef::Literal(rgb) => hex(*rgb),
            galdeck_model::ColorRef::Token(name) => format!("@{name}"),
        };
        Some(galdeck_ipc::BackdropInfo {
            origin: origin.into(),
            theme: (origin == "theme").then(|| profile.theme.clone()).flatten(),
            span: format!("{:?}", backdrop.span).to_lowercase(),
            image: backdrop.image.as_ref().map(|p| p.display().to_string()),
            animation: backdrop.animation.map(|m| m.name().to_string()),
            colors: backdrop.colors.iter().map(token).collect(),
            colors_hex: self
                .backdrop_colors(backdrop)
                .into_iter()
                .map(hex)
                .collect(),
            fps: backdrop.fps(),
            speed: backdrop.speed(),
            dim: backdrop.dim(),
        })
    }

    /// A widget's settings, for an editor.
    fn widget_info(&self, widget: &galdeck_model::Widget) -> WidgetInfo {
        let lower = |value: String| value.to_lowercase();
        WidgetInfo {
            kind: widget.kind.name().to_string(),
            interval_ms: widget.interval_ms(),
            format: widget.format.clone(),
            timezone: widget.timezone.clone(),
            place: widget.place.clone(),
            warn: widget.warn,
            critical: widget.critical,
            command: widget.command.clone(),
            placeholder: widget.placeholder.clone(),
            view: lower(format!("{:?}", widget.view())),
            own_view: widget.view.map(|view| lower(format!("{view:?}"))),
            look_view: widget
                .look
                .view
                .filter(|view| view.suits(widget.kind))
                .map(|view| lower(format!("{view:?}"))),
            title: widget.title.clone(),
            source: widget.source.clone(),
            units: widget.units.map(|units| lower(format!("{units:?}"))),
            latitude: widget.latitude,
            longitude: widget.longitude,
            max: widget.max,
            history: widget.history,
            color: widget.color.as_ref().map(|color| match color {
                galdeck_model::ColorRef::Literal(rgb) => hex(*rgb),
                galdeck_model::ColorRef::Token(name) => format!("@{name}"),
            }),
            color_hex: widget
                .color
                .as_ref()
                .and_then(|color| {
                    self.palette
                        .resolve(color, "widget.color", &mut Diagnostics::new())
                })
                .map(hex),
            background: widget.background.as_ref().map(|color| match color {
                galdeck_model::ColorRef::Literal(rgb) => hex(*rgb),
                galdeck_model::ColorRef::Token(name) => format!("@{name}"),
            }),
            background_hex: widget
                .background
                .as_ref()
                .and_then(|color| {
                    self.palette
                        .resolve(color, "widget.background", &mut Diagnostics::new())
                })
                .map(hex),
            opacity: widget.opacity,
            image: widget.image.as_ref().map(|p| p.display().to_string()),
            duration: widget.duration.clone(),
            on_done: widget.on_done.as_ref().map(|a| action_info(a, None)),
        }
    }

    fn config_snapshot(&self) -> ConfigSnapshot {
        let (_, diagnostics) = Workspace::load(&self.config_dir);
        ConfigSnapshot {
            dir: self.config_dir.clone(),
            files: self
                .documents
                .iter()
                .map(|(name, document)| ConfigFile {
                    name: name.clone(),
                    text: document.text(),
                    generation: document.generation(),
                })
                .collect(),
            diagnostics,
        }
    }

    /// Start a theme file, then load it as any saved edit is loaded.
    fn create_theme(
        &mut self,
        id: &str,
        name: Option<&str>,
        extends: Option<&str>,
        copy: Option<&str>,
    ) -> Response {
        match crate::theme_editor::create(
            &self.config_dir,
            &self.workspace,
            id,
            name,
            extends,
            copy,
        ) {
            Ok(file) => {
                log::info!("created {file}");
                self.reload()
            }
            Err(message) => Response::Error { message },
        }
    }

    /// Draw a theme with unsaved edits to its file, for the editor.
    fn preview_theme(&self, id: &str, patches: &[Patch]) -> Response {
        let file = format!("themes/{id}.toml");
        let (staged, diagnostics) = match self.stage(&file, patches, None) {
            Ok(staged) => staged,
            Err(message) => return Response::Error { message },
        };
        let overrides = std::collections::BTreeMap::from([(file, staged.text)]);
        let (workspace, _) = Workspace::load_with_overrides(&self.config_dir, &overrides);
        let Some(workspace) = workspace else {
            return Response::Error {
                message: "the config does not load with these edits".into(),
            };
        };
        Response::ThemePreview(Box::new(crate::theme_editor::preview(
            &workspace,
            id,
            self.backdrop_geometry(),
            self.font.as_ref(),
            diagnostics,
        )))
    }

    /// Answer a press on the key itself, as the theme's `[motion] press`
    /// says: a flash or a dip, drawn again in steps as it fades.
    fn start_press(&mut self, key: u8, at: Tick) {
        let answers = self
            .motion
            .press
            .as_ref()
            .is_some_and(|press| press.kind != galdeck_model::PressKind::None);
        if !answers || key >= Buttons::COUNT || !self.device.connected {
            return;
        }
        self.pressed[usize::from(key)] = Some(at);
        self.scheduler.cancel_kind(TimerKind::PressFrame { key });
        self.repaint_key(key);
        self.scheduler
            .after(at, PRESS_STEP, TimerKind::PressFrame { key });
    }

    /// The next step of a press fading, or, once it has, the key as it is.
    fn advance_press(&mut self, key: u8) {
        let now = self.clock.now();
        let fading = self.press_share(key, now).is_some();
        if !fading {
            self.pressed[usize::from(key)] = None;
        }
        self.repaint_key(key);
        if fading {
            self.scheduler
                .after(now, PRESS_STEP, TimerKind::PressFrame { key });
        }
    }

    /// How far towards the press colour key `key` is at `now`, while its
    /// press is fading. From the clock, so a repaint for any other reason
    /// draws the same step rather than hurrying it along.
    fn press_share(&self, key: u8, now: Tick) -> Option<f32> {
        let at = (*self.pressed.get(usize::from(key))?)?;
        let elapsed = u32::try_from(now.duration_since(at).as_millis()).unwrap_or(u32::MAX);
        self.motion.press.as_ref()?.share_at(elapsed)
    }

    /// What a press goes towards: black for a dip, else the press's own
    /// colour, else the theme's `@accent`, else white.
    fn press_color(&self) -> Rgb {
        let Some(press) = &self.motion.press else {
            return Rgb::WHITE;
        };
        if press.kind == galdeck_model::PressKind::Dim {
            return Rgb::BLACK;
        }
        press
            .color
            .as_ref()
            .and_then(|color| {
                self.palette
                    .resolve(color, "motion.press.color", &mut Diagnostics::new())
            })
            .unwrap_or_else(|| self.token_color("@accent", Rgb::WHITE))
    }

    /// Forget every press still fading, without drawing: the keys are about
    /// to be drawn anyway, or there is no deck to draw them on.
    fn forget_presses(&mut self) {
        for key in Buttons::indices() {
            self.scheduler.cancel_kind(TimerKind::PressFrame { key });
        }
        self.pressed = Default::default();
    }

    /// How far towards its alarm colour a widget past its threshold is now,
    /// if the theme's `[motion] alarm` moves it: stepping through that
    /// animation's frames by the clock. `None` when alarms do not move, as
    /// for a kind the model warns an alarm cannot be.
    fn alarm_pulse(&self) -> Option<f32> {
        let alarm = self.moving_alarm()?;
        let step = u64::from(alarm.frame_interval_ms()).max(1);
        let frame = (self.clock.now().0 / 1000 / step) % u64::from(alarm.frames());
        Some(alarm.mix_for_frame(u8::try_from(frame).unwrap_or(0)))
    }

    /// The theme's alarm animation, if it is one an alarm can be.
    fn moving_alarm(&self) -> Option<&Animation> {
        self.motion.alarm.as_ref().filter(|alarm| {
            !alarm.kind.is_ring_only() && alarm.kind != galdeck_model::AnimationKind::Rainbow
        })
    }

    /// Keep an alarm pulse moving while any widget showing is past its
    /// threshold and the theme says alarms move.
    fn watch_alarms(&mut self) {
        if self.alarm_frames {
            return;
        }
        let Some(interval) = self
            .moving_alarm()
            .map(|alarm| Duration::from_millis(u64::from(alarm.frame_interval_ms())))
        else {
            return;
        };
        let alarming = self
            .widget_state
            .values()
            .any(|state| state.level != galdeck_model::Level::Normal);
        if alarming {
            self.alarm_frames = true;
            let now = self.clock.now();
            self.scheduler.after(now, interval, TimerKind::AlarmFrame);
        }
    }

    /// Draw every widget past its threshold at the pulse's next step.
    fn advance_alarms(&mut self) {
        self.alarm_frames = false;
        if !self.device.connected {
            return;
        }
        let alarming: Vec<Slot> = self
            .widget_state
            .iter()
            .filter(|(_, state)| state.level != galdeck_model::Level::Normal)
            .map(|(slot, _)| *slot)
            .collect();
        for slot in alarming {
            match slot {
                Slot::Key(key) => self.repaint_key(key),
                Slot::Tile(_) => self.screen_dirty = true,
            }
        }
        self.watch_alarms();
    }

    /// Work out what an edit would do, without doing it.
    fn stage(
        &self,
        file: &str,
        patches: &[Patch],
        generation: Option<u64>,
    ) -> Result<(galdeck_model::Staged, Vec<galdeck_ipc::Diagnostic>), String> {
        let Some(document) = self.documents.get(file) else {
            return Err(format!("no such config file {file:?}"));
        };
        let staged = match document.preview(patches, generation) {
            Ok(staged) => staged,
            Err(diagnostics) => {
                return Ok((
                    galdeck_model::Staged {
                        text: document.text(),
                        generation: document.generation(),
                    },
                    diagnostics.sorted(),
                ))
            }
        };

        // Validate the whole workspace the edit would produce, not just the
        // file it touched: a theme that stops defining a colour breaks every
        // key that referenced it, and that is exactly what a user needs told
        // before saving.
        let overrides = std::collections::BTreeMap::from([(file.to_string(), staged.text.clone())]);
        let (_, diagnostics) = Workspace::load_with_overrides(&self.config_dir, &overrides);
        Ok((staged, diagnostics))
    }

    /// Recompute everything that depends on which profile is showing.
    fn enter_profile(&mut self) {
        let mut out = Diagnostics::new();
        let theme_id = self
            .workspace
            .profile(&self.profile_id)
            .and_then(|p| p.theme.clone());
        let (theme, palette) = self.workspace.theme_for(theme_id.as_deref(), &mut out);
        for diagnostic in out.iter() {
            log::warn!("{}: {}", diagnostic.path, diagnostic.message);
        }
        self.theme = theme;
        self.palette = palette;
        self.send_lighting();
        self.motion = self
            .workspace
            .profile(&self.profile_id)
            .map(|profile| self.workspace.motion_for(profile))
            .unwrap_or_default();
        self.page_index = self
            .workspace
            .profile(&self.profile_id)
            .map(Profile::home_index)
            .unwrap_or(0);
        self.page_stack.clear();
    }

    /// Connect the keyboard lighting thread, and tell it what the profile
    /// showing asks for.
    pub fn attach_lighting(&mut self, lighting: crate::lighting::LightingHandle) {
        self.lighting = Some(lighting);
        self.send_lighting();
    }

    /// Tell the lighting thread what the profile showing asks of the
    /// keyboard: `None` leaves it alone. Every profile switch and reload
    /// comes through `enter_profile`, which calls this; the thread ignores
    /// lighting it already shows.
    fn send_lighting(&self) {
        let Some(handle) = &self.lighting else {
            return;
        };
        let mut out = Diagnostics::new();
        let lighting = self
            .workspace
            .profile(&self.profile_id)
            .and_then(|profile| self.workspace.lighting_for(profile, &mut out));
        for diagnostic in out.iter() {
            log::warn!("{}: {}", diagnostic.path, diagnostic.message);
        }
        handle.set(lighting);
    }

    /// Draw lighting that is not saved, against `theme`'s palette, or the
    /// profile showing's when there is no `theme`.
    fn preview_lighting(
        &self,
        text: &str,
        theme: Option<&str>,
        seconds: Option<f32>,
        fps: Option<u8>,
        presses: &[galdeck_ipc::LightPress],
    ) -> Response {
        let mut out = Diagnostics::new();
        let palette = match theme {
            Some(id) => self.workspace.theme_for(Some(id), &mut out).1,
            None => self.palette.clone(),
        };
        let mut preview = crate::lighting::editor::preview(text, &palette, seconds, fps, presses);
        preview.diagnostics.splice(0..0, out.iter().cloned());
        Response::LightingPreview(Box::new(preview))
    }

    pub fn run(&mut self) {
        log::info!("engine started, config: {}", self.config_dir.display());
        while !self.shutdown.load(Ordering::Relaxed) {
            let now = self.clock.now();
            for (_, kind) in self.scheduler.due(now) {
                self.fire(kind);
            }
            while let Ok(msg) = self.device_rx.try_recv() {
                self.on_device(msg);
            }
            while let Ok(sample) = self.widget_rx.try_recv() {
                self.on_widget_sample(sample);
            }
            while let Ok(event) = self.plugin_rx.try_recv() {
                self.on_plugin_event(event);
            }
            while let Ok(report) = self.report_rx.try_recv() {
                self.on_control_report(report);
            }
            while let Ok(report) = self.state_rx.try_recv() {
                self.on_state_report(report);
            }
            while let Ok((reload, themes)) = self.icons_rx.try_recv() {
                self.on_icons(reload, themes);
            }
            // Non-blocking; a plugin that has exited can then be started again
            // the next time one of its keys appears.
            self.plugin_host.reap();
            self.service_control();
            if self.screen_dirty {
                self.paint_screen_when_due();
            }

            // Publish before sleeping: the io thread sizes its poll from this,
            // and a deadline registered after it has already gone to sleep
            // would not be noticed until its next pass.
            let now = self.clock.now();
            let next = self.scheduler.next_deadline(now);
            self.deadline.publish(next.map(|d| now.saturating_add(d)));

            // A floor of one millisecond, because a deadline that is already
            // due would otherwise turn the loop into a spin.
            let wait = next.unwrap_or(CORE_IDLE).max(Duration::from_millis(1));
            if !self.wake.wait(wait) {
                break;
            }
        }

        // Asked to stop before the process exits, so a plugin gets a chance to
        // tidy up rather than being killed outright.
        self.plugin_host.shutdown();
        // A microphone opened for push-to-talk is closed on the way out, and
        // here rather than on a worker: the process may be gone before a
        // worker gets its turn.
        if self.talking.take().is_some() {
            if let Err(e) = crate::audio::set_mute(&crate::audio::AudioTarget::Input, true) {
                log::warn!("closing the microphone on the way out: {e}");
            }
        }
        log::info!("engine stopped");
    }

    fn fire(&mut self, kind: TimerKind) {
        match kind {
            TimerKind::RingRest { encoder } => self.paint_ring(encoder),
            TimerKind::KeyFrame { key } => self.advance_key_animation(key),
            TimerKind::RingFrame { encoder } => self.advance_ring_animation(encoder),
            TimerKind::WidgetTick { slot } => self.tick_widget(slot),
            TimerKind::KnobHold { encoder } => self.knob_held(encoder),
            TimerKind::OsdEnd => {
                self.osd = None;
                self.screen_dirty = true;
            }
            TimerKind::BackdropFrame => self.advance_backdrop(),
            TimerKind::Gesture { key } => {
                let now = self.clock.now();
                let bindings = self.bindings_for(key);
                let decision = self.input.timeout(key, now, bindings);
                self.act_on(key, decision);
            }
            TimerKind::CountdownDue => self.finish_countdowns(),
            TimerKind::CountdownFlash => self.flash_timers(),
            TimerKind::LevelPoll => self.poll_levels(),
            TimerKind::StateStatus { key } => self.poll_state(key),
            TimerKind::KeyStates => self.key_states_due(),
            TimerKind::PressFrame { key } => self.advance_press(key),
            TimerKind::AlarmFrame => self.advance_alarms(),
            // Nothing to do here: the loop paints a dirty screen once its
            // turn has come, and this only wakes it for that.
            TimerKind::ScreenFrame => {}
        }
    }

    /// What gestures this key has been given.
    fn bindings_for(&self, key: u8) -> Bindings {
        match self.key_config(key) {
            Some(cfg) => Bindings {
                // A tap is anything the key does on a plain press, including
                // navigating rather than running something.
                has_tap: cfg.tap().is_some()
                    || cfg.page.is_some()
                    || cfg.profile.is_some()
                    || cfg.back
                    || cfg.plugin.is_some(),
                has_hold: cfg.hold_action().is_some(),
                has_double: cfg.double.is_some(),
            },
            None => Bindings::default(),
        }
    }

    /// Carry out what the input machine decided.
    fn act_on(&mut self, key: u8, decision: Decision) {
        match decision {
            Decision::Fire(gesture) => self.fire_gesture(key, gesture),
            Decision::Wait(at) => {
                self.scheduler.cancel_kind(TimerKind::Gesture { key });
                self.scheduler.at(at, TimerKind::Gesture { key });
            }
            Decision::Idle => {}
        }
    }

    fn fire_gesture(&mut self, key: u8, gesture: Gesture) {
        let Some(cfg) = self.key_config(key).cloned() else {
            return;
        };
        match gesture {
            Gesture::Hold => {
                if let Some(action) = cfg.hold_action() {
                    self.perform(&action, From::Key(key), 1);
                }
            }
            Gesture::Double => {
                if let Some(action) = &cfg.double {
                    self.perform(action, From::Key(key), 1);
                }
            }
            Gesture::Tap => {
                if let Some(action) = cfg.tap() {
                    self.perform(&action, From::Key(key), 1);
                }
                if let Some(binding) = cfg.plugin {
                    self.plugin_host
                        .send(&binding.id, galdeck_plugin::ToPlugin::Press { key });
                }
                // At most one navigation, in the order a key would sensibly
                // declare them.
                if let Some(name) = cfg.profile {
                    self.switch_profile(&name);
                } else if let Some(name) = cfg.page {
                    self.switch_page(&name);
                } else if cfg.back {
                    self.go_back();
                }
            }
        }
    }

    /// The widget in a slot of the current page.
    fn widget_at(&self, slot: Slot) -> Option<&galdeck_model::Widget> {
        let page = self.current_page()?;
        match slot {
            Slot::Key(key) => page.keys.iter().find(|k| k.key == key)?.widget.as_ref(),
            Slot::Tile(tile) => Some(&page.lcd.get(tile as usize)?.widget),
        }
    }

    /// Sample a widget and schedule its next refresh.
    fn tick_widget(&mut self, slot: Slot) {
        let Some(widget) = self.widget_at(slot).cloned() else {
            return;
        };
        // Counted from taps on the key, not sampled.
        if widget.kind.is_timer() {
            if self.tick_countdown(slot, &widget) {
                match slot {
                    Slot::Key(key) => self.repaint_key(key),
                    Slot::Tile(_) => self.screen_dirty = true,
                }
            }
            return;
        }
        let now = self.clock.now();
        // An animated view is drawn on every tick but read at its own pace:
        // tubes glowing over a CPU reading must not read the CPU ten times a
        // second, let alone run a command. Between readings a tick is only
        // the next frame.
        let read_every = Duration::from_millis(u64::from(widget.sample_interval_ms()));
        let read_recently = widget.sample_interval_ms() > widget.interval_ms()
            && self
                .widget_state
                .get(&slot)
                .and_then(|state| state.sampled_at)
                .is_some_and(|at| now.duration_since(at) < read_every);
        if read_recently {
            match slot {
                Slot::Key(key) => self.repaint_key(key),
                Slot::Tile(_) => self.screen_dirty = true,
            }
        } else {
            if let Some(state) = self.widget_state.get_mut(&slot) {
                state.sampled_at = Some(now);
            }
            // Blocking kinds answer through the channel instead of returning here.
            if let Some(reading) = self
                .widget_host
                .sample(slot, self.widget_generation, &widget)
            {
                self.on_widget_sample(Sample {
                    slot,
                    generation: self.widget_generation,
                    reading: Some(reading),
                });
            }
        }
        let interval = Duration::from_millis(u64::from(widget.interval_ms()));
        self.scheduler
            .after(now, interval, TimerKind::WidgetTick { slot });
    }

    /// Put a timer's or stopwatch's time in its slot, and come back when
    /// the time shown next changes: once a second while it runs, never while
    /// it waits. Returns whether what the slot shows changed.
    fn tick_countdown(&mut self, slot: Slot, widget: &galdeck_model::Widget) -> bool {
        self.scheduler.cancel_kind(TimerKind::WidgetTick { slot });
        let countdown = match slot {
            Slot::Key(key) => self.key_config(key).and_then(|cfg| self.countdown_of(cfg)),
            // Nothing on the screen can start one, so it shows its start.
            Slot::Tile(_) => countdown_length(Some(widget)).ok().map(Countdown::new),
        };
        let Some(countdown) = countdown else {
            return false;
        };
        let now = self.clock.now();
        if let Some(at) = countdown.next_change(now) {
            self.scheduler.at(at, TimerKind::WidgetTick { slot });
        }
        let reading = countdown_reading(&countdown, now);
        self.widget_state
            .get_mut(&slot)
            .is_some_and(|state| state.update(Some(reading), widget))
    }

    /// Act on something a plugin said.
    ///
    /// A plugin may only touch keys the config currently gives it. Without
    /// that check any plugin could paint over any key, which would make the
    /// binding in the config a suggestion rather than a grant.
    fn on_plugin_event(&mut self, event: PluginEvent) {
        match event.message {
            galdeck_plugin::FromPlugin::Ready { name } => {
                log::info!("plugin {:?} ready ({name})", event.plugin);
            }
            galdeck_plugin::FromPlugin::Log { message } => {
                log::info!("[{}] {message}", event.plugin);
            }
            galdeck_plugin::FromPlugin::SetText { key, text } => {
                if !self.plugin_owns(&event.plugin, key) {
                    return;
                }
                if self.plugin_text[key as usize].as_deref() != Some(text.as_str()) {
                    self.plugin_text[key as usize] = Some(text);
                    self.repaint_key(key);
                }
            }
            galdeck_plugin::FromPlugin::SetColor { key, color } => {
                if !self.plugin_owns(&event.plugin, key) {
                    return;
                }
                // A plugin may name a theme colour, so it can stay inside the
                // user's palette rather than inventing its own.
                let resolved = galdeck_model::ColorRef::parse(&color)
                    .ok()
                    .and_then(|reference| {
                        self.palette
                            .resolve(&reference, "plugin.color", &mut Diagnostics::new())
                    });
                let Some(rgb) = resolved else {
                    log::debug!(
                        "plugin {:?} sent an unusable colour {color:?}",
                        event.plugin
                    );
                    return;
                };
                if self.plugin_color[key as usize] != Some(rgb) {
                    self.plugin_color[key as usize] = Some(rgb);
                    self.repaint_key(key);
                }
            }
        }
    }

    fn plugin_owns(&self, plugin: &str, key: u8) -> bool {
        key < Buttons::COUNT
            && self
                .plugin_visible
                .get(plugin)
                .is_some_and(|keys| keys.contains(&key))
    }

    /// Tell plugins which of their keys are on the page now.
    ///
    /// The disappear half matters as much as the appear: a plugin whose page
    /// is not showing should be able to stop working entirely, rather than
    /// polling something for a key nobody can see.
    fn update_plugin_visibility(&mut self, page: &galdeck_model::v2::Page) {
        let mut now: std::collections::BTreeMap<String, Vec<u8>> = Default::default();
        for cfg in &page.keys {
            let Some(binding) = &cfg.plugin else { continue };
            if cfg.key >= Buttons::COUNT {
                continue;
            }
            if !self.plugin_host.known(&binding.id) {
                log::warn!(
                    "key {} is bound to unknown plugin {:?}",
                    cfg.key,
                    binding.id
                );
                continue;
            }
            now.entry(binding.id.clone()).or_default().push(cfg.key);
        }

        // Gone.
        let previous = std::mem::take(&mut self.plugin_visible);
        for (id, keys) in &previous {
            for key in keys {
                if !now.get(id).is_some_and(|current| current.contains(key)) {
                    self.plugin_host
                        .send(id, galdeck_plugin::ToPlugin::Disappear { key: *key });
                    self.plugin_text[*key as usize] = None;
                    self.plugin_color[*key as usize] = None;
                }
            }
        }

        // Arrived. Set before sending, so a plugin that answers immediately is
        // not rejected for touching a key we have not recorded yet.
        self.plugin_visible = now;
        let arrivals: Vec<(String, u8, std::collections::BTreeMap<String, String>)> = page
            .keys
            .iter()
            .filter_map(|cfg| {
                let binding = cfg.plugin.as_ref()?;
                let was_visible = previous
                    .get(&binding.id)
                    .is_some_and(|keys| keys.contains(&cfg.key));
                (!was_visible && self.plugin_host.known(&binding.id))
                    .then(|| (binding.id.clone(), cfg.key, binding.options.clone()))
            })
            .collect();
        for (id, key, options) in arrivals {
            self.plugin_host
                .send(&id, galdeck_plugin::ToPlugin::Appear { key, options });
        }
    }

    /// Take a widget's new reading and repaint just what shows it.
    fn on_widget_sample(&mut self, sample: Sample) {
        if sample.generation != self.widget_generation {
            return;
        }
        let Some(widget) = self.widget_at(sample.slot).cloned() else {
            return;
        };
        let Some(state) = self.widget_state.get_mut(&sample.slot) else {
            return;
        };
        if !state.update(sample.reading, &widget) {
            // Nothing moved. A clock showing the same minute must not cost a
            // JPEG encode every second.
            return;
        }
        match sample.slot {
            Slot::Key(key) => self.repaint_key(key),
            Slot::Tile(_) => self.screen_dirty = true,
        }
        // It may just have crossed a threshold.
        self.watch_alarms();
    }

    /// Re-render one key, without touching the rest of the page.
    fn repaint_key(&mut self, key: u8) {
        if !self.device.connected {
            return;
        }
        let target = self.key_target(key);
        match &target {
            KeyTarget::Jpeg(jpeg) | KeyTarget::Region { jpeg, .. } => {
                self.preview.set_key(key, Some(Arc::clone(jpeg)));
            }
            _ => self.preview.set_key(key, None),
        }
        self.send(Paint::Key { index: key, target });
    }

    /// What a key shows, encoded and ready to send.
    fn key_target(&mut self, index: u8) -> KeyTarget {
        let placement = self.key_placement(index);
        match self.key_picture(index, placement.size()) {
            Some((canvas, fallback)) => match canvas.to_jpeg(galdeck::ids::DEFAULT_JPEG_QUALITY) {
                Ok(jpeg) => placement.target(jpeg.into()),
                Err(e) => {
                    log::warn!("encoding key {index} failed: {e}");
                    KeyTarget::Color(fallback)
                }
            },
            None => self.blank_target(placement),
        }
    }

    /// Draw a key, with the colour to fall back on if encoding it fails.
    ///
    /// `None` for a key with nothing on it and nothing behind it. A key with
    /// nothing on it still shows its slice of a background, so a picture
    /// spanning the keys is not full of black holes.
    fn key_picture(&self, index: u8, size: (u32, u32)) -> Option<(galdeck::Canvas, Rgb)> {
        let behind = self.backdrop_key(index, size);
        let Some(cfg) = self.shown_key(index) else {
            return behind.map(|canvas| (canvas, Rgb::BLACK));
        };
        let overlay = self.key_overlay(&cfg);
        Some(self.look_picture(&cfg, size, behind, overlay))
    }

    /// Draw `cfg`, a key as it looks, over its slice of the background if
    /// there is one, with `overlay` laid over it.
    fn look_picture(
        &self,
        cfg: &KeyConfig,
        size: (u32, u32),
        behind: Option<galdeck::Canvas>,
        overlay: Option<Overlay>,
    ) -> (galdeck::Canvas, Rgb) {
        let index = cfg.key;
        let mut style = self.style_for(Some(&cfg.style), &format!("keys[{index}].style"));
        let plugin = self.plugin_color.get(index as usize).copied().flatten();
        if let Some(color) = plugin {
            style.key_bg = color;
        }
        // A key that asks for a colour of its own gets it, background or
        // not; the theme's key colour gives way to a background.
        let base = match behind {
            Some(canvas) if !Self::has_own_background(cfg) && plugin.is_none() => canvas,
            _ => galdeck::Canvas::filled(size.0, size.1, style.key_bg),
        };
        (self.key_canvas(cfg, &style, base, overlay), style.key_bg)
    }

    /// Key `key` of the page showing as it looks now: in the state it shows,
    /// if it has states, and otherwise as configured.
    fn shown_key(&self, key: u8) -> Option<KeyConfig> {
        self.key_config(key).map(|cfg| self.look_of(cfg))
    }

    /// `cfg`, a key of the page showing, as it looks now. A key with states
    /// shows the one it is in, or its own look while it does not know which
    /// that is; either way the look has no states of its own, so is drawn as
    /// any other key is.
    fn look_of(&self, cfg: &KeyConfig) -> KeyConfig {
        if cfg.states.is_empty() {
            return cfg.clone();
        }
        let shown = self
            .home_of(cfg.key)
            .and_then(|home| self.shown_state(&home, cfg));
        match shown {
            Some(state) => cfg.in_state(&state),
            None => own_look(cfg),
        }
    }

    /// The state the key at `home`, `cfg`, shows: what it last moved to or
    /// was read as, else, for a key with no way to read it, its first.
    fn shown_state(&self, home: &KeyHome, cfg: &KeyConfig) -> Option<String> {
        match self.key_states.get(home) {
            Some(cycle) => cycle.shown.clone().or_else(|| KeyCycle::new(cfg).shown),
            None => KeyCycle::new(cfg).shown,
        }
    }

    fn has_own_background(cfg: &KeyConfig) -> bool {
        cfg.style.key_bg.is_some()
    }

    /// Draw a key over `canvas`: a graph or card when its widget draws as
    /// one, otherwise the icon and label.
    fn key_canvas(
        &self,
        cfg: &KeyConfig,
        style: &ResolvedStyle,
        mut canvas: galdeck::Canvas,
        overlay: Option<Overlay>,
    ) -> galdeck::Canvas {
        let (width, height) = (canvas.width(), canvas.height());
        let area = draw::Area::new(0, 0, width, height);
        let mut surface = match &cfg.widget {
            Some(widget) => self
                .widget_backing(&mut canvas, area, widget, None, 1.0, 0, "key.widget")
                .unwrap_or(style.key_bg),
            None => style.key_bg,
        };
        // Over whatever the key sits on, the widget's own background
        // included, and under its text, which is mixed against the result.
        if let Some(overlay) = &overlay {
            draw::blend_round_rect(&mut canvas, area, 0, overlay.color, overlay.share);
            surface = surface.lerp(overlay.color, overlay.share);
        }
        let mut canvas = match cfg
            .widget
            .as_ref()
            .filter(|w| w.is_graphic() || w.kind.is_timer())
        {
            Some(widget) => {
                let colors = self.widget_colors(
                    widget,
                    surface,
                    style.key_label_color,
                    "key.widget",
                    Some(Slot::Key(cfg.key)),
                );
                if widget.kind.is_timer() {
                    self.draw_countdown(&mut canvas, area, cfg, widget, colors);
                } else {
                    let fallback = widget.placeholder.as_deref().or(cfg.label.as_deref());
                    draw::widget(
                        &mut canvas,
                        area,
                        widget,
                        self.widget_state.get(&Slot::Key(cfg.key)),
                        colors,
                        self.font.as_ref(),
                        fallback,
                    );
                }
                canvas
            }
            None => {
                let label = self.label_for(cfg);
                // A widget shown as a label turns its label amber or red.
                let mut style = *style;
                if let Some(alarm) = self.alarm_color(Slot::Key(cfg.key)) {
                    style.key_label_color = alarm;
                }
                let size = (canvas.width(), canvas.height());
                let icon = self.key_icon(cfg, &style, size, label.is_some());
                render::key_over_with_icon(
                    canvas,
                    &style,
                    icon.as_deref(),
                    label.as_deref(),
                    self.font.as_ref(),
                )
            }
        };
        if let Some(glyph) = overlay.and_then(|overlay| overlay.glyph) {
            draw::muted_glyph(&mut canvas, area, glyph, style.key_label_color, surface);
        }
        canvas
    }

    /// A timer or stopwatch key: the time large, and small above it the
    /// widget's title or else the key's label, so two timers side by side
    /// say which is which. Dimmed while paused.
    fn draw_countdown(
        &self,
        canvas: &mut galdeck::Canvas,
        area: draw::Area,
        cfg: &KeyConfig,
        widget: &galdeck_model::Widget,
        colors: draw::Colors,
    ) {
        let now = self.clock.now();
        let countdown = self.countdown_of(cfg);
        let paused = countdown.as_ref().is_some_and(|c| c.display(now).paused);
        let colors = if paused { colors.dimmed() } else { colors };
        // Worked out now rather than taken from the slot's last tick: a page
        // switch paints its keys before their widgets have ticked, and the
        // slot may still hold a reading from the page before.
        let state = countdown.map(|countdown| {
            let mut state = crate::widgets::SlotState::default();
            state.update(Some(countdown_reading(&countdown, now)), widget);
            state
        });
        let captioned = galdeck_model::Widget {
            title: widget.title.clone().or_else(|| cfg.label.clone()),
            ..widget.clone()
        };
        // A timer with no length it can run shows no time rather than its
        // caption twice.
        let fallback = widget.placeholder.as_deref().unwrap_or("-:--");
        draw::widget(
            canvas,
            area,
            &captioned,
            state.as_ref(),
            colors,
            self.font.as_ref(),
            Some(fallback),
        );
    }

    /// What a key's state lays over it. One thing at a time, the one that
    /// matters most:
    ///
    /// 0. the theme's answer to a press, while it fades -- the direct answer
    ///    to what a finger just did, and over in a moment, after which what
    ///    it covered shows again;
    /// 1. a finished timer, which is waiting for someone;
    /// 2. a live microphone, then a muted device -- only from what the sound
    ///    server reported, never from a guess: a key saying "muted" is a
    ///    claim about the microphone, and until a report says so the key
    ///    says nothing;
    /// 3. what a key with states says about its own: that its command just
    ///    failed, then that it is still running, then that the state could
    ///    not be read -- the order [`KeyCycle::badge`] picks in.
    ///
    /// A key with states has no timer and taps no mute, so the last never
    /// meets the first two in a config without errors. A new kind of overlay
    /// goes into this list where it ranks, as a step of its own below. While
    /// any shows, the key's animation waits underneath it.
    fn key_overlay(&self, cfg: &KeyConfig) -> Option<Overlay> {
        let now = self.clock.now();
        if let Some(share) = self.press_share(cfg.key, now) {
            return Some(Overlay {
                color: self.press_color(),
                share,
                glyph: None,
            });
        }
        let alarm = self
            .home_of(cfg.key)
            .and_then(|which| self.countdowns.get(&which))
            .and_then(|countdown| alarm_share(countdown, now));
        if let Some(share) = alarm {
            return Some(Overlay {
                color: self.token_color("@critical", CRITICAL),
                share,
                glyph: None,
            });
        }
        if let Some(overlay) = self.device_overlay(cfg) {
            return Some(overlay);
        }
        self.state_badge(cfg.key, now)
    }

    /// What a mute or push-to-talk key lays over itself: a live microphone
    /// tinted amber, a muted device red and struck through.
    fn device_overlay(&self, cfg: &KeyConfig) -> Option<Overlay> {
        let state = state_key(cfg)?;
        let level = self.levels.get(&state.target())?;
        match state {
            StateKey::Talk => (!level.muted).then(|| Overlay {
                color: self.token_color("@warning", WARNING),
                share: STATE_SHARE,
                glyph: None,
            }),
            StateKey::Mute(target) => level.muted.then(|| Overlay {
                color: self.token_color("@critical", CRITICAL),
                share: STATE_SHARE,
                glyph: Some(if target == crate::audio::AudioTarget::Input {
                    draw::Glyph::Microphone
                } else {
                    draw::Glyph::Speaker
                }),
            }),
        }
    }

    /// What key `key` of the page showing says about its state, if it has
    /// states: a red tint and a warning in its corner for a moment after its
    /// command failed, a bar along its bottom while one runs, a "?" while
    /// its state could not be read.
    fn state_badge(&self, key: u8, now: Tick) -> Option<Overlay> {
        let home = self.home_of(key)?;
        let badge = self.key_states.get(&home)?.badge(now)?;
        Some(match badge {
            crate::states::Badge::Failed => Overlay {
                color: self.token_color("@critical", CRITICAL),
                share: FAILED_SHARE,
                glyph: Some(draw::Glyph::Warning),
            },
            crate::states::Badge::Pending => Overlay {
                color: Rgb::BLACK,
                share: 0.0,
                glyph: Some(draw::Glyph::Pending),
            },
            crate::states::Badge::Unknown => Overlay {
                color: Rgb::BLACK,
                share: 0.0,
                glyph: Some(draw::Glyph::Unknown),
            },
        })
    }

    /// Lay a widget's own background -- a colour, a picture, or both -- over
    /// `area`, and say what colour it now sits on, for mixing its text.
    ///
    /// `default` is the colour to use when the widget names none, at
    /// `default_opacity` unless it names an opacity.
    #[allow(clippy::too_many_arguments)]
    fn widget_backing(
        &self,
        canvas: &mut galdeck::Canvas,
        area: draw::Area,
        widget: &galdeck_model::Widget,
        default: Option<Rgb>,
        default_opacity: f32,
        radius: u32,
        path: &str,
    ) -> Option<Rgb> {
        // Its own, or its look's: a theme may give every widget a card.
        let own = widget
            .background()
            .and_then(|color| self.palette.resolve(color, path, &mut Diagnostics::new()));
        let color = own.or(default);
        let opacity = widget
            .opacity
            .or(widget.look.opacity)
            .map_or(default_opacity, |_| widget.opacity());
        if let Some(color) = color {
            draw::blend_round_rect(canvas, area, radius, color, opacity);
        }
        if let Some(image) = widget
            .image
            .as_deref()
            .and_then(|p| self.widget_image(p, area.width, area.height))
        {
            draw::cover_image(canvas, &image, area, widget.opacity());
        }
        color
    }

    /// A widget's picture, loaded once per path and size.
    fn widget_image(
        &self,
        path: &std::path::Path,
        width: u32,
        height: u32,
    ) -> Option<Arc<image::RgbaImage>> {
        let key = (path.to_path_buf(), width, height);
        let mut cache = self.widget_images.borrow_mut();
        if let Some(found) = cache.get(&key) {
            return found.clone();
        }
        // Bounded: a config being edited can name a new size on every save.
        if cache.len() >= 64 {
            cache.clear();
        }
        let loaded = image::open(path)
            .map_err(|e| log::warn!("widget image {}: {e}", path.display()))
            .ok()
            .map(|image| {
                // Scaled to cover once here, so drawing is a straight copy.
                let image =
                    image.resize_to_fill(width, height, image::imageops::FilterType::Triangle);
                Arc::new(image.to_rgba8())
            });
        cache.insert(key, loaded.clone());
        loaded
    }

    /// A widget's colours: the surface it sits on, the text colour, and its
    /// own accent if it names one.
    fn widget_colors(
        &self,
        widget: &galdeck_model::Widget,
        background: Rgb,
        foreground: Rgb,
        path: &str,
        slot: Option<Slot>,
    ) -> draw::Colors {
        let mut accent = widget
            .color()
            .and_then(|color| self.palette.resolve(color, path, &mut Diagnostics::new()))
            .unwrap_or(foreground);
        let mut foreground = foreground;
        // Past a threshold, what shows the reading turns amber or red: the
        // fill of a graph, bar or dial, or the text itself.
        if let Some(alarm) = slot.and_then(|slot| self.alarm_color(slot)) {
            // All the way there, unless the theme says alarms move: then
            // between its usual colours and the alarm's, as they do.
            let share = self.alarm_pulse().unwrap_or(1.0);
            accent = accent.lerp(alarm, share);
            if !widget.is_graphic() {
                foreground = foreground.lerp(alarm, share);
            }
        }
        draw::Colors {
            background,
            foreground,
            accent,
        }
    }

    /// The colour a slot's reading should be drawn in if it has crossed a
    /// threshold: the theme's `@warning` or `@critical`, else amber or red.
    fn alarm_color(&self, slot: Slot) -> Option<Rgb> {
        match self.widget_state.get(&slot)?.level {
            galdeck_model::Level::Normal => None,
            galdeck_model::Level::Warn => Some(self.token_color("@warning", WARNING)),
            galdeck_model::Level::Critical => Some(self.token_color("@critical", CRITICAL)),
        }
    }

    /// A theme colour by its token, or `fallback` for a theme without it.
    fn token_color(&self, token: &str, fallback: Rgb) -> Rgb {
        galdeck_model::ColorRef::parse(token)
            .ok()
            .and_then(|color| {
                // Quietly: a theme without the tokens is not a mistake.
                self.palette
                    .resolve(&color, "alarm", &mut Diagnostics::new())
            })
            .unwrap_or(fallback)
    }

    /// What a key should show: its widget's text if it has produced any, then
    /// the widget's placeholder, then the key's own label.
    fn label_for(&self, cfg: &KeyConfig) -> Option<String> {
        if cfg.plugin.is_some() {
            if let Some(text) = self.plugin_text[cfg.key as usize].clone() {
                return Some(text);
            }
        }
        if cfg.widget.is_some() {
            if let Some(text) = self
                .widget_state
                .get(&Slot::Key(cfg.key))
                .and_then(|state| state.reading.as_ref()?.label())
            {
                return Some(text);
            }
            if let Some(placeholder) = cfg.widget.as_ref().and_then(|w| w.placeholder.clone()) {
                return Some(placeholder);
            }
        }
        cfg.label.clone()
    }

    /// Show the next frame of a key's animation and schedule the one after.
    fn advance_key_animation(&mut self, key: u8) {
        let Some(animation) = self.key_animations[key as usize].as_mut() else {
            return;
        };
        let frame = Arc::clone(&animation.frames[animation.next as usize]);
        animation.next = (animation.next + 1) % animation.frames.len() as u8;
        let interval = animation.interval;

        // Frames were drawn when the page was applied, without what the
        // key's state lays over it; while that is up, the key shows it and
        // the animation waits underneath.
        let covered = self
            .shown_key(key)
            .is_some_and(|cfg| self.key_overlay(&cfg).is_some());
        if !covered {
            self.preview.set_key(key, Some(Arc::clone(&frame)));
            // Resolved per frame rather than captured when the animation was
            // built: a calibration can change underneath a running animation,
            // and a frame drawn at the old rectangle would be visibly stale.
            let placement = self.key_placement(key);
            self.send(Paint::Key {
                index: key,
                target: placement.target(frame),
            });
        }
        let now = self.clock.now();
        self.scheduler
            .after(now, interval, TimerKind::KeyFrame { key });
    }

    fn advance_ring_animation(&mut self, encoder: u8) {
        let Some(animation) = self.ring_animations[encoder as usize].as_ref() else {
            return;
        };
        let now = self.clock.now();
        let colors = animation.frame(now);
        // A knob being turned writes LEDs of its own; see
        // `ANIMATION_FRAME_BUSY`. Timed from the start as ever, so the
        // animation is where it should be when it speeds up again.
        let next = if self.rings.iter().any(RingFeedback::is_active) {
            now.saturating_add(crate::ring::ANIMATION_FRAME_BUSY)
        } else {
            animation.next_frame(now)
        };

        // Turn and click feedback wins: an animation must not hide what the
        // knob is doing. The rest colour it returns to is the animation's, so
        // the next frame picks straight back up.
        if !self.rings[encoder as usize].is_active() {
            self.rings[encoder as usize].rest(colors[0]);
            self.preview.set_ring(encoder, colors);
            self.send(Paint::Ring { encoder, colors });
        }
        self.scheduler.at(next, TimerKind::RingFrame { encoder });
    }

    /// Schedule the widgets on this page, and forget the ones that left.
    fn start_widgets(&mut self, page: &galdeck_model::v2::Page) {
        for slot in self.widget_state.keys() {
            self.scheduler
                .cancel_kind(TimerKind::WidgetTick { slot: *slot });
        }
        // Readings from the page we just left would otherwise show on
        // whatever key happens to share its position here.
        self.widget_state.clear();
        self.widget_generation += 1;

        let keys = page
            .keys
            .iter()
            .filter(|cfg| cfg.widget.is_some() && cfg.key < Buttons::COUNT)
            .map(|cfg| Slot::Key(cfg.key));
        // A page with more tiles than this has bigger problems than widgets.
        let tiles = (0..page.lcd.len().min(usize::from(u8::MAX))).map(|t| Slot::Tile(t as u8));
        let now = self.clock.now();
        for slot in keys.chain(tiles) {
            self.widget_state.insert(slot, SlotState::default());
            // Fire immediately rather than after one interval: a clock that
            // takes a second to appear looks broken.
            self.scheduler.at(now, TimerKind::WidgetTick { slot });
        }
        let live = &self.widget_state;
        self.widget_host.retain(|slot| live.contains_key(&slot));
    }

    /// Render and encode every frame of the animations on this page.
    ///
    /// Done once, here, rather than per frame. The cost is real -- eight
    /// frames is eight JPEG encodes -- but it is paid on a page switch instead
    /// of thirty times a second forever. A key with states has only the
    /// state it shows animated; another state's frames are drawn when the
    /// key moves to it.
    fn build_animations(&mut self) {
        for encoder in Encoders::indices() {
            self.scheduler.cancel_kind(TimerKind::RingFrame { encoder });
            self.ring_animations[encoder as usize] = None;
        }
        for key in Buttons::indices() {
            self.scheduler.cancel_kind(TimerKind::KeyFrame { key });
            self.key_animations[key as usize] = None;
        }

        let now = self.clock.now();
        let mut encoded_frames = 0usize;
        for key in Buttons::indices() {
            encoded_frames += self.build_key_animation(key, MAX_PAGE_FRAMES - encoded_frames);
        }

        for encoder in Encoders::indices() {
            let plan = self.encoder_plan(encoder);
            // The knob's own, else the theme's for rings at rest -- but never
            // over a ring that has something to say: a level, a position, or
            // the colour that is the only sign of which mode a knob is in.
            let animation = plan.animation.clone().or_else(|| {
                (plan.ring().is_none() && plan.modes.is_none())
                    .then(|| self.motion.rings.clone())
                    .flatten()
            });
            let Some(animation) = &animation else {
                continue;
            };
            let cfg = EncoderConfig {
                encoder,
                ..Default::default()
            };
            let style = self.style_for(Some(&plan.style), "encoder");
            let to = animation
                .to
                .as_ref()
                .and_then(|color| {
                    self.palette
                        .resolve(color, "animation.to", &mut Diagnostics::new())
                })
                .unwrap_or(Rgb::WHITE);
            let interval = crate::ring::animation_interval(animation);
            self.ring_animations[cfg.encoder as usize] = Some(RingAnimation {
                animation: animation.clone(),
                base: style.ring,
                to,
                interval,
                start: now,
            });
            self.scheduler.after(
                now,
                interval,
                TimerKind::RingFrame {
                    encoder: cfg.encoder,
                },
            );
        }

        if encoded_frames > MAX_PRERENDERED_FRAMES {
            log::warn!(
                "this page pre-renders {encoded_frames} animation frames; page switches will be slow"
            );
        }
    }

    /// Render and encode every frame of key `key`'s animation, as the key
    /// looks now, and start it, replacing any it had. At most `room` frames:
    /// an animation that needs more is left out, and the key stays still.
    /// Returns how many frames it took.
    fn build_key_animation(&mut self, key: u8, room: usize) -> usize {
        // Only the deck's own keys are drawn, however many a page names.
        if key >= Buttons::COUNT {
            return 0;
        }
        self.scheduler.cancel_kind(TimerKind::KeyFrame { key });
        self.key_animations[key as usize] = None;
        let Some(cfg) = self.shown_key(key) else {
            return 0;
        };
        let Some(animation) = &cfg.animation else {
            return 0;
        };
        if animation.kind.is_ring_only() {
            return 0;
        }
        let count = animation.frames();
        if usize::from(count) > room {
            log::warn!(
                "key {key}'s animation would take this page past {MAX_PAGE_FRAMES} \
                 pre-rendered frames; it is left still"
            );
            return 0;
        }
        let style = self.style_for(Some(&cfg.style), "key");
        let to = animation
            .to
            .as_ref()
            .and_then(|color| {
                self.palette
                    .resolve(color, "animation.to", &mut Diagnostics::new())
            })
            .unwrap_or(Rgb::WHITE);

        let size = self.key_placement(key).size();
        let label = cfg.label.as_deref();
        // Drawn once for every frame: only the background moves.
        let icon = self.key_icon(&cfg, &style, size, label.is_some());
        let mut frames = Vec::with_capacity(count as usize);
        for index in 0..count {
            let mut frame_style = style;
            frame_style.key_bg = animation.color_for_frame(index, style.key_bg, to);
            let canvas = render::key_with_icon(
                size,
                &frame_style,
                icon.as_deref(),
                label,
                self.font.as_ref(),
            );
            match canvas.to_jpeg(galdeck::ids::DEFAULT_JPEG_QUALITY) {
                Ok(jpeg) => frames.push(Arc::from(jpeg)),
                Err(e) => {
                    log::warn!("encoding a frame for key {key} failed: {e}");
                    break;
                }
            }
        }
        if frames.len() < 2 {
            return 0;
        }
        let taken = frames.len();
        let interval = Duration::from_millis(u64::from(animation.frame_interval_ms()));
        self.key_animations[key as usize] = Some(KeyAnimation {
            frames,
            interval,
            next: 0,
        });
        let now = self.clock.now();
        self.scheduler
            .after(now, interval, TimerKind::KeyFrame { key });
        taken
    }

    /// Draw key `key`'s animation again, as it looks now: it has moved to
    /// another state, or its icon has been found. In the room the rest of
    /// the page's animations leave.
    fn rebuild_key_animation(&mut self, key: u8) {
        let others: usize = self
            .key_animations
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != usize::from(key))
            .filter_map(|(_, animation)| animation.as_ref())
            .map(|animation| animation.frames.len())
            .sum();
        self.build_key_animation(key, MAX_PAGE_FRAMES.saturating_sub(others));
    }

    /// Key `cfg`'s icon, drawn to fit it: a file it names, or a name looked
    /// up in the icon themes, recoloured with the label's colour when it is
    /// a symbolic one. `None` for no icon, or one that cannot be found or
    /// drawn -- said once, in the log -- when the key shows its label alone.
    fn key_icon(
        &self,
        cfg: &KeyConfig,
        style: &ResolvedStyle,
        size: (u32, u32),
        labelled: bool,
    ) -> Option<Arc<image::RgbaImage>> {
        use crate::icons::Origin;
        let icon_box = render::icon_box(size, style, labelled);
        let (path, origin) = match cfg.icon.as_ref()? {
            galdeck_model::IconRef::Path(path) => (crate::icons::expand_home(path), Origin::File),
            galdeck_model::IconRef::Name(name) => (
                self.icons.as_ref()?.themes.resolve(name, icon_box)?,
                Origin::Theme,
            ),
        };
        self.icon_cache
            .borrow_mut()
            .get(&path, origin, icon_box, style.key_label_color)
    }

    fn on_device(&mut self, msg: DeviceMsg) {
        match msg {
            DeviceMsg::Connected { firmware, serial } => {
                self.preview.publish(galdeck_ipc::Event::DeviceConnected {
                    firmware: firmware.clone(),
                    serial: serial.clone(),
                });
                self.device = DeviceStatus {
                    connected: true,
                    firmware: Some(firmware),
                    serial: Some(serial),
                };
                self.paint_page();
            }
            DeviceMsg::Disconnected => {
                self.device = DeviceStatus::default();
                self.end_talk();
                // Nothing shows them now. The page reads them again when it
                // is painted on the deck's return.
                self.stop_state_reads();
                // The release will never come.
                self.forget_knobs();
                // Nor would a press answered before the unplug look like one
                // when the deck comes back.
                self.forget_presses();
                self.preview.publish(galdeck_ipc::Event::DeviceDisconnected);
                // The handle is gone, which is exactly what a release was
                // waiting to hear. Answered here whether the device was
                // parked or merely unplugged: either way there is nothing
                // left to collide with, which is all the caller asked.
                for reply in self.pending_release.drain(..) {
                    let _ = reply.send(Response::Ok);
                }
            }
            DeviceMsg::ModeReentry => {
                // The firmware wiped what it was showing, so nothing we
                // believe about the rings still holds.
                log::info!("re-applying page after software-mode re-entry");
                for ring in &mut self.rings {
                    ring.forget();
                }
                self.paint_page();
            }
            DeviceMsg::Input(event, at) => self.handle_event(event, at),
        }
    }

    /// Hand one paint to the io thread.
    ///
    /// A full channel means the device is further behind than a whole page of
    /// paint, which only happens when it has stopped responding. Dropping is
    /// right: the io thread will reconnect and everything gets repainted.
    fn send(&self, paint: Paint) {
        match self.paint_tx.try_send(paint) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => log::debug!("paint queue full, dropping"),
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    fn current_profile(&self) -> Option<&Profile> {
        self.workspace.profile(&self.profile_id)
    }

    fn current_page(&self) -> Option<&Page> {
        let profile = self.current_profile()?;
        profile
            .pages
            .get(self.page_index.min(profile.pages.len().saturating_sub(1)))
    }

    fn key_config(&self, key: u8) -> Option<&KeyConfig> {
        self.current_page()?.keys.iter().find(|k| k.key == key)
    }

    /// The resolved style for one cell of the current page.
    fn style_for(&self, cell: Option<&StyleLayer>, path: &str) -> ResolvedStyle {
        let (Some(profile), Some(page)) = (self.current_profile(), self.current_page()) else {
            return ResolvedStyle::BUILTIN;
        };
        let mut out = Diagnostics::new();
        let (style, _) = self.workspace.style_for(
            &self.theme,
            &self.palette,
            profile,
            page,
            cell,
            path,
            &mut out,
        );
        // Anything wrong here was already reported when the config loaded;
        // repeating it once per repaint would drown the log.
        style
    }

    /// Push the current page's full state to the device.
    /// Describe the current page to the io thread.
    ///
    /// This sends what every surface *should* show and lets the device mirror
    /// decide what actually differs. A page switch that changes one key
    /// therefore costs one key image rather than twelve images, eight ring
    /// segments and a full LCD frame.
    fn paint_page(&mut self) {
        if !self.device.connected {
            return;
        }
        // A press still fading belonged to whatever key was at its position.
        // Forgotten before the keys are drawn, or one would keep its flash.
        self.forget_presses();
        self.send(Paint::Brightness(self.brightness));
        self.preview.set_brightness(self.brightness);

        // Cloned up front so the borrow of the workspace ends before the
        // paints go out; a page is a dozen small structs.
        let Some(page) = self.current_page().cloned() else {
            return;
        };

        // Before the keys, which take their slices of it.
        self.start_backdrop(&page);

        for index in Buttons::indices() {
            let target = self.key_target(index);
            match &target {
                KeyTarget::Jpeg(jpeg) | KeyTarget::Region { jpeg, .. } => {
                    self.preview.set_key(index, Some(Arc::clone(jpeg)));
                }
                _ => self.preview.set_key(index, None),
            }
            self.send(Paint::Key { index, target });
        }

        for index in Encoders::indices() {
            let plan = self.encoder_plan(index);
            // The page paints the ring, so the feedback state rests there --
            // but it does not get to claim the hardware shows it. Only the
            // mirror, on the far side of a real write, says that.
            self.rest_ring(index, &plan);
            self.paint_ring(index);
        }
        self.watch_levels();
        self.restart_flash();

        self.input.forget();
        self.forget_knobs();
        // The key holding the microphone open may not be there any more.
        self.end_talk();
        for key in Buttons::indices() {
            self.scheduler.cancel_kind(TimerKind::Gesture { key });
        }
        self.build_animations();
        self.start_widgets(&page);
        self.start_state_reads(&page);
        self.update_plugin_visibility(&page);
        self.paint_lcd();
    }

    /// Paint the marked screen if its turn has come, or else leave it marked
    /// and make sure the loop comes back when it has. The mark is cleared
    /// only by painting, so whatever was asked for last is what ends up
    /// shown.
    fn paint_screen_when_due(&mut self) {
        let paced_by_background = self.scene.as_ref().is_some_and(|scene| {
            scene.is_animated()
                && scene.span().covers_lcd()
                && scene.interval() <= SCREEN_RIDES_BACKGROUND
        });
        let due = match self.screen_painted {
            None => true,
            // The background's next frame is close, and takes this with it.
            Some((_, frame)) if paced_by_background => frame != self.scene_tick,
            Some((at, _)) => {
                let due = at.saturating_add(SCREEN_FRAME_MIN);
                let now = self.clock.now();
                if now < due && !self.scheduler.contains_kind(TimerKind::ScreenFrame) {
                    self.scheduler.at(due, TimerKind::ScreenFrame);
                }
                now >= due
            }
        };
        if due {
            self.screen_dirty = false;
            self.paint_lcd();
        }
    }

    /// Draw the info screen: its tiles if the page lays any out, otherwise
    /// the page's text.
    ///
    /// Always the whole screen. A tile could be sent as a region, but a
    /// full frame is one encode of a few milliseconds even at a media tile's
    /// once a second, and the device mirror drops a frame that has not
    /// changed.
    fn paint_lcd(&mut self) {
        if !self.device.connected {
            return;
        }
        let Some(page) = self.current_page() else {
            return;
        };
        let style = self.style_for(None, "lcd.style");
        let placement = ScreenPlacement::from_calibration(&self.calibration);
        let size = placement.size();
        let behind = self.backdrop_lcd(size);
        let over_picture = behind.is_some();
        let base = behind.unwrap_or_else(|| galdeck::Canvas::filled(size.0, size.1, style.lcd_bg));
        let mut screen = if page.lcd.is_empty() {
            let text = page.lcd_text.clone().unwrap_or_else(|| page.id.clone());
            render::lcd_over(base, &style, &text, self.font.as_ref())
        } else {
            let mut canvas = base.clone();
            // Tiles sit on a card a shade off the background, so a layout of
            // several reads as separate things rather than one busy screen.
            // Over a picture the card is translucent, so the picture is still
            // there to be seen.
            let card = style.lcd_bg.lerp(style.lcd_text_color, 0.08);
            let card_opacity = if over_picture { 0.55 } else { 1.0 };
            let grid = self
                .current_profile()
                .map(|profile| Workspace::grid_for(profile, page))
                .unwrap_or_default();
            for (index, tile) in page.lcd.iter().enumerate() {
                if !tile.fits(grid) {
                    continue;
                }
                let area = tile_area(tile.cells(grid), grid, size);
                let path = format!("lcd[{index}].widget");
                let radius = tile_radius(&tile.widget, area);
                let surface = self
                    .widget_backing(
                        &mut canvas,
                        area,
                        &tile.widget,
                        Some(card),
                        card_opacity,
                        radius,
                        &path,
                    )
                    .unwrap_or(card);
                let colors = self.widget_colors(
                    &tile.widget,
                    surface,
                    style.lcd_text_color,
                    &path,
                    Some(Slot::Tile(index as u8)),
                );
                draw::widget(
                    &mut canvas,
                    area,
                    &tile.widget,
                    self.widget_state.get(&Slot::Tile(index as u8)),
                    colors,
                    self.font.as_ref(),
                    tile.widget.placeholder.as_deref(),
                );
                draw::restore_corners(&mut canvas, area, radius, &base);
            }
            canvas
        };
        if let Some(osd) = &self.osd {
            draw::osd(
                &mut screen,
                &osd.text,
                osd.level,
                draw::Colors {
                    background: style.lcd_bg,
                    foreground: style.lcd_text_color,
                    accent: style.lcd_text_color,
                },
                self.font.as_ref(),
            );
        }
        let jpeg = match screen.to_jpeg(galdeck::ids::DEFAULT_JPEG_QUALITY) {
            Ok(jpeg) => Arc::<[u8]>::from(jpeg),
            Err(e) => {
                log::warn!("encoding the lcd failed: {e}");
                return;
            }
        };
        // The preview is the screen as it is seen, not the rounded frame.
        self.preview.set_lcd(Arc::clone(&jpeg));
        let (jpeg, at) = match placement {
            ScreenPlacement::Firmware => (jpeg, None),
            ScreenPlacement::Measured { visible, frame } if visible == frame => (jpeg, Some(frame)),
            // A second encode, but only on a screen whose edges are not whole
            // blocks. The rows it adds are under the bezel, so the plain
            // background is all they need.
            ScreenPlacement::Measured { visible, frame } => {
                let mut padded = galdeck::Canvas::filled(
                    u32::from(frame.width),
                    u32::from(frame.height),
                    style.lcd_bg,
                );
                padded.blit(
                    &screen,
                    i32::from(visible.x) - i32::from(frame.x),
                    i32::from(visible.y) - i32::from(frame.y),
                );
                match padded.to_jpeg(galdeck::ids::DEFAULT_JPEG_QUALITY) {
                    Ok(padded) => (padded.into(), Some(frame)),
                    Err(e) => {
                        log::warn!("encoding the lcd failed: {e}");
                        return;
                    }
                }
            }
        };
        self.screen_painted = Some((self.clock.now(), self.scene_tick));
        self.send(Paint::Lcd { jpeg, at });
    }

    /// Where the screen and each key's image sit on the panel.
    ///
    /// From the calibration, template or not: an uncalibrated deck still has
    /// its surfaces roughly where the template puts them, and a background
    /// that is continuous to within a few pixels is better than none.
    fn backdrop_geometry(&self) -> crate::backdrop::Geometry {
        use crate::backdrop::Rect;
        let screen = match ScreenPlacement::from_calibration(&self.calibration) {
            ScreenPlacement::Measured { visible, .. } => visible,
            ScreenPlacement::Firmware => self.calibration.screen,
        };
        let keys = Buttons::indices()
            .map(|index| match self.key_placement(index) {
                KeyPlacement::Zone {
                    x,
                    y,
                    width,
                    height,
                } => Rect::new(
                    i32::from(x),
                    i32::from(y),
                    u32::from(width),
                    u32::from(height),
                ),
                // The firmware centres its fixed square in the key -- an
                // assumption, but the one the calibration screen draws too.
                KeyPlacement::Firmware => {
                    let (width, height) = galdeck::Button::size();
                    match self.calibration.zone_at(index) {
                        Some(zone) => {
                            let b = zone.bounds;
                            Rect::new(
                                i32::from(b.x) + (i32::from(b.width) - width as i32) / 2,
                                i32::from(b.y) + (i32::from(b.height) - height as i32) / 2,
                                width,
                                height,
                            )
                        }
                        None => Rect::new(0, 0, width, height),
                    }
                }
            })
            .collect();
        crate::backdrop::Geometry {
            lcd: Rect::new(
                i32::from(screen.x),
                i32::from(screen.y),
                u32::from(screen.width),
                u32::from(screen.height),
            ),
            keys,
        }
    }

    /// Prepare the page's background and start it moving, if it moves.
    fn start_backdrop(&mut self, page: &Page) {
        self.scheduler.cancel_kind(TimerKind::BackdropFrame);
        let wanted = self
            .current_profile()
            .and_then(|profile| self.workspace.background_for(profile, page))
            .filter(|backdrop| !backdrop.is_empty())
            .cloned();
        let Some(backdrop) = wanted else {
            self.scene = None;
            self.scene_source = None;
            self.scene_frame = None;
            return;
        };

        let source: SceneSource = (
            backdrop.clone(),
            self.backdrop_colors(&backdrop),
            self.backdrop_geometry(),
        );
        if self.scene_source.as_ref() != Some(&source) {
            let started = std::time::Instant::now();
            self.scene =
                match crate::backdrop::Scene::load(&source.0, source.1.clone(), source.2.clone()) {
                    Ok(scene) => {
                        log::debug!("background prepared in {:?}", started.elapsed());
                        Some(scene)
                    }
                    Err(problem) => {
                        log::warn!("background not shown: {problem}");
                        None
                    }
                };
            self.scene_source = Some(source);
            self.scene_tick = 0;
        }
        let Some(scene) = &self.scene else {
            self.scene_frame = None;
            return;
        };
        self.scene_frame = Some(scene.render(self.scene_tick));
        if scene.is_animated() {
            let now = self.clock.now();
            self.scheduler
                .after(now, scene.interval(), TimerKind::BackdropFrame);
        }
    }

    /// The colours an animated background is drawn in: the configured ones,
    /// else a set taken from the theme so it matches without being asked.
    fn backdrop_colors(&self, backdrop: &galdeck_model::Backdrop) -> Vec<Rgb> {
        let lcd_bg = self.style_for(None, "background").lcd_bg;
        crate::backdrop::colors_for(backdrop, &self.palette, lcd_bg)
    }

    /// Show the next frame of the background on everything it covers.
    fn advance_backdrop(&mut self) {
        let Some(scene) = &self.scene else {
            return;
        };
        let interval = scene.interval();
        let span = scene.span();
        if self.device.connected {
            self.scene_tick = self.scene_tick.wrapping_add(1);
            self.scene_frame = Some(scene.render(self.scene_tick));
            if span.covers_lcd() {
                self.screen_dirty = true;
            }
            if span.covers_keys() {
                for index in Buttons::indices() {
                    if self.key_follows_backdrop(index) {
                        self.repaint_key(index);
                    }
                }
            }
        }
        let now = self.clock.now();
        self.scheduler
            .after(now, interval, TimerKind::BackdropFrame);
    }

    /// Whether a key's picture changes with the background.
    fn key_follows_backdrop(&self, index: u8) -> bool {
        if self.key_animations[index as usize].is_some()
            || self.plugin_color[index as usize].is_some()
        {
            return false;
        }
        self.shown_key(index)
            .is_none_or(|cfg| !Self::has_own_background(&cfg))
    }

    fn backdrop_key(&self, index: u8, size: (u32, u32)) -> Option<galdeck::Canvas> {
        let (scene, frame) = (self.scene.as_ref()?, self.scene_frame.as_ref()?);
        scene.key(frame, index, size)
    }

    fn backdrop_lcd(&self, size: (u32, u32)) -> Option<galdeck::Canvas> {
        let (scene, frame) = (self.scene.as_ref()?, self.scene_frame.as_ref()?);
        scene.lcd(frame, size)
    }

    /// Draw a widget the config does not have, with made-up readings, for
    /// an editor's gallery.
    fn render_widget_preview(
        &self,
        fields: std::collections::BTreeMap<String, galdeck_model::Value>,
        width: u32,
        height: u32,
    ) -> Response {
        let (width, height) = (width.clamp(16, 720), height.clamp(16, 720));
        let table: toml::Table = fields
            .into_iter()
            .map(|(key, value)| (key, toml_value(value)))
            .collect();
        let mut widget: galdeck_model::Widget = match toml::Value::Table(table).try_into() {
            Ok(widget) => widget,
            Err(e) => {
                return Response::Error {
                    message: format!("not a widget: {e}"),
                }
            }
        };
        // In this page's look, as it would be if it were placed here.
        if let Some(profile) = self.current_profile() {
            widget.look = self
                .workspace
                .widget_look(profile, self.current_page(), widget.kind);
        }
        let state = crate::widgets::demo::state(&widget);
        let style = self.style_for(None, "preview");
        let area = draw::Area::new(0, 0, width, height);
        // Square is a key; anything else is a piece of the screen, drawn on a
        // card the way a tile would be.
        let on_key = width == height;
        let (background, foreground) = if on_key {
            (style.key_bg, style.key_label_color)
        } else {
            (style.lcd_bg, style.lcd_text_color)
        };
        let mut canvas = galdeck::Canvas::filled(width, height, background);
        let card = (!on_key).then(|| style.lcd_bg.lerp(style.lcd_text_color, 0.08));
        let radius = if on_key {
            0
        } else {
            tile_radius(&widget, area)
        };
        let surface = self
            .widget_backing(&mut canvas, area, &widget, card, 1.0, radius, "preview")
            .unwrap_or(background);
        let colors = self.widget_colors(&widget, surface, foreground, "preview", None);
        // A timer on a key draws its caption over the time, as it will on
        // the deck.
        if widget.is_graphic() || widget.kind.is_timer() || !on_key {
            draw::widget(
                &mut canvas,
                area,
                &widget,
                Some(&state),
                colors,
                self.font.as_ref(),
                Some(default_label(&widget)),
            );
        } else {
            let text = state.reading.as_ref().and_then(|r| r.label());
            canvas = render::key_over_with_icon(
                canvas,
                &style,
                None,
                text.as_deref(),
                self.font.as_ref(),
            );
        }
        match canvas.to_jpeg(90) {
            Ok(jpeg) => Response::Image {
                url: format!("data:image/jpeg;base64,{}", crate::base64::encode(&jpeg)),
            },
            Err(e) => Response::Error {
                message: format!("encoding the preview: {e}"),
            },
        }
    }

    /// Keep an uploaded picture in the config directory's `assets/`.
    fn save_asset(&self, name: &str, data: &str) -> Response {
        let Some(bytes) = crate::base64::decode(data) else {
            return Response::Error {
                message: "that is not base64".into(),
            };
        };
        asset_response(crate::assets::store(
            &crate::assets::dir(&self.config_dir),
            name,
            &bytes,
        ))
    }

    // ------------------------------------------------------------ actions

    /// The knob `encoder` on the current page, every layer folded in, in the
    /// mode it is in.
    ///
    /// Folded once to find which modes own the knob's turn here, and again
    /// in the mode those are in: which list it is decides where its mode is
    /// kept, so it takes the first fold to know.
    fn encoder_plan(&self, encoder: u8) -> galdeck_model::EncoderPlan {
        let (Some(profile), Some(page)) = (self.current_profile(), self.current_page()) else {
            return Default::default();
        };
        let global = &self.workspace.global.encoders;
        let plan = Workspace::encoder_for(profile, page, global, encoder);
        match self.dial_mode(encoder, &plan) {
            0 => plan,
            mode => Workspace::encoder_for_mode(profile, page, global, encoder, mode),
        }
    }

    /// Which mode a knob's modes are in: the first until it is switched.
    fn dial_mode(&self, encoder: u8, plan: &galdeck_model::EncoderPlan) -> usize {
        plan.modes
            .as_ref()
            .and_then(|stack| {
                self.dial_modes
                    .get(&(encoder, stack.layer, stack.entries.clone()))
            })
            .copied()
            .unwrap_or(0)
    }

    /// Keep through a reload only the modes of lists that are still written
    /// somewhere, so a list that changes and later changes back starts again
    /// from its first mode, and the map holds no more than the config does.
    fn keep_dial_modes(&mut self) {
        use galdeck_model::Layer;
        let workspace = &self.workspace;
        let global = workspace
            .global
            .encoders
            .iter()
            .map(|config| (Layer::Global, config));
        let profiles = workspace.profiles.values().flat_map(|profile| {
            let own = profile
                .encoders
                .iter()
                .map(|config| (Layer::Profile, config));
            let pages = profile
                .pages
                .iter()
                .flat_map(|page| page.encoders.iter().map(|config| (Layer::Page, config)));
            own.chain(pages)
        });
        let written: std::collections::HashSet<(u8, Layer, &[galdeck_model::ModeEntry])> = global
            .chain(profiles)
            .filter_map(|(layer, config)| Some((config.encoder, layer, config.mode_stack()?)))
            .collect();
        self.dial_modes.retain(|(encoder, layer, entries), _| {
            written.contains(&(*encoder, *layer, entries.as_slice()))
        });
    }

    /// Whether anything in the config types keys or moves the wheel.
    fn config_uses_virtual_input(&self) -> bool {
        let global = &self.workspace.global.encoders;
        self.workspace.profiles.values().any(|profile| {
            let encoders = global.iter().chain(&profile.encoders);
            encoders
                .chain(profile.pages.iter().flat_map(|page| &page.encoders))
                .any(|encoder| {
                    // Every mode's, not only the one the knob is in.
                    encoder
                        .all_gestures()
                        .iter()
                        .any(galdeck_model::Action::needs_virtual_input)
                })
                || profile.pages.iter().flat_map(|page| &page.keys).any(|key| {
                    key.actions()
                        .any(galdeck_model::Action::needs_virtual_input)
                })
        })
    }

    /// Create the virtual keyboard ahead of need, when the config has any use
    /// for it: the desktop takes up to a second to notice a new keyboard, and
    /// a keystroke sent before then goes nowhere.
    fn prepare_virtual_input(&mut self) {
        if self.workspace.global.virtual_input && self.config_uses_virtual_input() {
            self.controls.input(crate::controls::InputJob::Prepare);
        }
    }

    /// Do what an action says, `times` over for a knob's detents.
    fn perform(&mut self, action: &galdeck_model::Action, from: From, times: u32) {
        use galdeck_model::Action;
        match action {
            Action::Shell(command) => self.actions.run(command, None),
            Action::Keys(chord) => self.press_keys(chord, times),
            Action::BuiltIn(invocation) => self.perform_built_in(invocation, from, times),
        }
    }

    fn press_keys(&mut self, chord: &str, times: u32) {
        if !self.workspace.global.virtual_input {
            log::debug!("keystroke {chord:?} ignored: virtual_input is off");
            return;
        }
        match galdeck_model::keys::parse_chord(chord) {
            Ok(codes) => {
                self.controls.input(crate::controls::InputJob::Chord {
                    codes,
                    times: times.clamp(1, MAX_DETENTS_PER_EVENT) as u8,
                    queued: std::time::Instant::now(),
                });
            }
            Err(problem) => log::warn!("keystroke {chord:?}: {problem}"),
        }
    }

    fn wheel(&mut self, horizontal: bool, notches: i32, modifiers: Vec<u16>) {
        if !self.workspace.global.virtual_input {
            log::debug!("scrolling ignored: virtual_input is off");
            return;
        }
        self.controls.input(crate::controls::InputJob::Wheel {
            horizontal,
            notches,
            modifiers,
            queued: std::time::Instant::now(),
        });
    }

    fn perform_built_in(&mut self, invocation: &galdeck_model::Invocation, from: From, times: u32) {
        use crate::audio::AudioTarget;
        use crate::controls::ServiceJob;
        use crate::widgets::media::MediaCommand;
        use galdeck_model::BuiltIn;

        // Doing nothing without a word would look like a broken key.
        if let Some(why) = misplaced(invocation.action, from) {
            self.show_osd(why, None);
            return;
        }
        let n = f64::from(times.max(1));
        let step = invocation.step();
        let target = || match AudioTarget::parse(invocation.target.as_deref()) {
            Ok(target) => Some(target),
            Err(problem) => {
                log::warn!("{}: {problem}", invocation.action.name());
                None
            }
        };
        let media = |engine: &mut Self, command| {
            engine.controls.service(ServiceJob::Media {
                command,
                target: invocation.target.clone(),
            });
        };
        match invocation.action {
            BuiltIn::VolumeUp | BuiltIn::VolumeDown | BuiltIn::MicUp | BuiltIn::MicDown => {
                let target = match invocation.action {
                    BuiltIn::MicUp | BuiltIn::MicDown => AudioTarget::Input,
                    _ => match target() {
                        Some(target) => target,
                        None => return,
                    },
                };
                let sign = if matches!(invocation.action, BuiltIn::VolumeUp | BuiltIn::MicUp) {
                    1.0
                } else {
                    -1.0
                };
                let delta = sign * step * n;
                self.controls.service(ServiceJob::Volume {
                    target: target.clone(),
                    delta,
                });
                self.predict_level(from, target, delta);
            }
            BuiltIn::VolumeMute | BuiltIn::MicMute => {
                let target = if invocation.action == BuiltIn::MicMute {
                    AudioTarget::Input
                } else {
                    match target() {
                        Some(target) => target,
                        None => return,
                    }
                };
                self.note_level_asker(from, target.clone());
                self.controls.service(ServiceJob::Mute { target, to: None });
            }
            // Handled on the key's own edges; see `handle_event`.
            BuiltIn::PushToTalk => {}
            BuiltIn::PlayPause => media(self, MediaCommand::PlayPause),
            BuiltIn::NextTrack | BuiltIn::PreviousTrack => {
                if self.too_soon(from) {
                    return;
                }
                let command = if invocation.action == BuiltIn::NextTrack {
                    MediaCommand::Next
                } else {
                    MediaCommand::Previous
                };
                media(self, command);
            }
            BuiltIn::SeekForward | BuiltIn::SeekBackward => {
                let sign = if invocation.action == BuiltIn::SeekForward {
                    1.0
                } else {
                    -1.0
                };
                let micros = (sign * step * n * 1_000_000.0) as i64;
                media(self, MediaCommand::Seek(micros));
            }
            BuiltIn::NextPage | BuiltIn::PreviousPage => {
                let by = if invocation.action == BuiltIn::NextPage {
                    1
                } else {
                    -1
                } * times.max(1) as i64;
                self.step_page(by, from);
            }
            BuiltIn::HomePage => {
                let home = self.current_profile().map(Profile::home_index);
                if let Some(home) = home {
                    self.go_to_page(home, from);
                }
            }
            BuiltIn::NextProfile | BuiltIn::PreviousProfile => {
                let by = if invocation.action == BuiltIn::NextProfile {
                    1
                } else {
                    -1
                } * times.max(1) as i64;
                self.step_profile(by, from);
            }
            BuiltIn::StartProfile => {
                if let Some(start) = self.workspace.start_profile().map(str::to_string) {
                    self.switch_profile(&start);
                    self.show_osd(format!("Profile · {start}"), None);
                }
            }
            BuiltIn::DeckBrighter | BuiltIn::DeckDimmer => {
                let sign = if invocation.action == BuiltIn::DeckBrighter {
                    1.0
                } else {
                    -1.0
                };
                let wanted = f64::from(self.brightness) + sign * step * n;
                // Never quite dark: at zero every key goes black and the knob
                // that did it is the only way back.
                let percent = wanted.round().clamp(f64::from(MIN_DECK_BRIGHTNESS), 100.0) as u8;
                self.set_brightness(percent);
                if let From::Knob(encoder) = from {
                    let now = self.clock.now();
                    self.rings[encoder as usize].level(f32::from(percent) / 100.0, false, now);
                    self.paint_ring(encoder);
                }
                self.show_osd(
                    format!("Deck brightness {percent}%"),
                    Some((f32::from(percent) / 100.0, false)),
                );
            }
            BuiltIn::ScrollUp
            | BuiltIn::ScrollDown
            | BuiltIn::ScrollLeft
            | BuiltIn::ScrollRight => {
                let notches = (step * n).round() as i32;
                let (horizontal, sign) = match invocation.action {
                    BuiltIn::ScrollUp => (false, 1),
                    BuiltIn::ScrollDown => (false, -1),
                    BuiltIn::ScrollRight => (true, 1),
                    _ => (true, -1),
                };
                self.wheel(horizontal, sign * notches, Vec::new());
            }
            BuiltIn::ZoomIn | BuiltIn::ZoomOut => {
                let sign = if invocation.action == BuiltIn::ZoomIn {
                    1
                } else {
                    -1
                };
                self.wheel(false, sign * times.max(1) as i32, vec![CTRL]);
            }
            BuiltIn::ZoomReset => self.press_keys("ctrl+0", 1),
            // Anywhere but a key was turned away above.
            BuiltIn::TimerToggle | BuiltIn::TimerReset => {
                if let From::Key(key) = from {
                    if invocation.action == BuiltIn::TimerToggle {
                        self.toggle_timer(key);
                    } else {
                        self.reset_timer(key);
                    }
                }
            }
            // Anywhere but a key was turned away above.
            BuiltIn::NextState | BuiltIn::PreviousState => {
                if let From::Key(key) = from {
                    self.step_state(key, invocation.action == BuiltIn::NextState);
                }
            }
            // Anywhere but a knob was turned away above.
            BuiltIn::NextMode => {
                if let From::Knob(encoder) = from {
                    self.next_mode(encoder);
                }
            }
            BuiltIn::NextOutput | BuiltIn::PreviousOutput => {
                // One output per knob event however far the knob went, then
                // quiet for a moment: every hop moves every stream on the
                // machine, and a spin summed would land wherever the
                // remainder said.
                if !self.mixer_ready("output") || self.too_soon(from) {
                    return;
                }
                let step = if invocation.action == BuiltIn::NextOutput {
                    1
                } else {
                    -1
                };
                self.controls.service(ServiceJob::Output {
                    step,
                    order: self.workspace.global.outputs.clone(),
                    from: knob(from),
                });
            }
            BuiltIn::SetOutput => {
                if !self.mixer_ready("output") {
                    return;
                }
                let Some(name) = invocation.target.clone() else {
                    self.show_failure("output", "set_output names no output");
                    return;
                };
                self.controls.service(ServiceJob::OutputSet {
                    name,
                    order: self.workspace.global.outputs.clone(),
                    from: knob(from),
                });
            }
            BuiltIn::AppVolumeUp | BuiltIn::AppVolumeDown => {
                if !self.mixer_ready("app volume") {
                    return;
                }
                let sign = if invocation.action == BuiltIn::AppVolumeUp {
                    1.0
                } else {
                    -1.0
                };
                self.controls.service(ServiceJob::AppVolume {
                    app: self.app_for(invocation, from),
                    delta: sign * step * n,
                    from: knob(from),
                });
            }
            BuiltIn::AppMute => {
                if !self.mixer_ready("app volume") {
                    return;
                }
                self.controls.service(ServiceJob::AppMute {
                    app: self.app_for(invocation, from),
                    from: knob(from),
                });
            }
            // Only from a knob, which the check above made sure of: it moves
            // on from the app that knob has.
            BuiltIn::NextApp => {
                let From::Knob(encoder) = from else {
                    return;
                };
                if !self.mixer_ready("app volume") || self.too_soon(from) {
                    return;
                }
                let current = self.knob_apps[encoder as usize]
                    .as_ref()
                    .map(|app| app.key.clone());
                self.controls.service(ServiceJob::AppStep {
                    current,
                    from: Some(encoder),
                });
            }
        }
    }

    /// Whether a knob changed track, output or app too recently to change
    /// another, and if not, note that it is changing one now. A spin changes
    /// one, not one per detent. Keys and the editor are never too soon.
    fn too_soon(&mut self, from: From) -> bool {
        let From::Knob(encoder) = from else {
            return false;
        };
        let Some(last) = self.last_switch.get_mut(encoder as usize) else {
            return false;
        };
        let now = self.clock.now();
        if last.is_some_and(|at| now.duration_since(at) < SWITCH_DEBOUNCE) {
            return true;
        }
        *last = Some(now);
        false
    }

    /// Whether outputs and apps can be acted on here, saying why not on the
    /// screen when they cannot: a knob that does nothing without a word looks
    /// broken.
    fn mixer_ready(&mut self, what: &str) -> bool {
        match crate::audio::mixer() {
            Ok(()) => true,
            Err(why) => {
                self.show_failure(what, &why);
                false
            }
        }
    }

    /// Which app a per-app built-in acts on: the one it names, else the one
    /// its knob last acted on, else none, which the worker takes as the app
    /// playing.
    fn app_for(&self, invocation: &galdeck_model::Invocation, from: From) -> Option<String> {
        invocation.target.clone().or_else(|| match from {
            From::Knob(encoder) => self
                .knob_apps
                .get(encoder as usize)?
                .as_ref()
                .map(|app| app.key.clone()),
            _ => None,
        })
    }

    // ------------------------------------------------------------ modes

    /// Switch a knob to its next mode, going round, and show which it is in.
    fn next_mode(&mut self, encoder: u8) {
        let plan = self.encoder_plan(encoder);
        let Some(stack) = plan.modes else {
            self.show_osd(format!("{} has no modes", knob_name(encoder)), None);
            return;
        };
        let count = stack.entries.len();
        let mode = (plan.mode + 1) % count;
        let preset = stack.entries[mode].preset;
        let key = (encoder, stack.layer, stack.entries);
        if mode == 0 {
            self.dial_modes.remove(&key);
        } else {
            self.dial_modes.insert(key, mode);
        }
        // A spin through pages that was still going is the old mode's.
        if let Some(latch) = self.nav_latch.get_mut(encoder as usize) {
            *latch = None;
        }
        self.preview
            .publish(galdeck_ipc::Event::ModeChanged { encoder, mode });
        self.show_osd(mode_text(encoder, mode, count, preset), None);

        // The ring rests at the new mode's colour, with its place among the
        // modes lit on it for a moment first.
        let plan = self.encoder_plan(encoder);
        let now = self.clock.now();
        self.rest_ring(encoder, &plan);
        if let Some(ring) = self.rings.get_mut(encoder as usize) {
            ring.position(mode, now);
        }
        self.paint_ring(encoder);
        // The new mode may turn a level the page was not showing, or stop
        // turning one it was.
        self.watch_levels();
    }

    // ------------------------------------------------------------ timers

    /// Key `key` of the page showing, by name: where its timer and its
    /// state are kept. `None` when no page is showing, or the page showing
    /// has an id an earlier page of the profile has too.
    fn home_of(&self, key: u8) -> Option<KeyHome> {
        Some(KeyHome {
            profile: self.profile_id.clone(),
            page: self.home_page()?.to_string(),
            key,
        })
    }

    /// The id of the page showing, unless an earlier page has it too.
    ///
    /// The model only warns of two pages with one id (W0103), and
    /// `next_page` reaches the second by its place in the list. But what is
    /// kept under a [`KeyHome`] is found again in the first page with its
    /// id, as a switch by id finds it, so a key of the second must not name
    /// its home by the same id: a tap on it would run the first page's
    /// key's command. Its keys have no states or timers instead, and say so.
    fn home_page(&self) -> Option<&str> {
        let page = self.current_page()?;
        let first = self.current_profile()?.page(&page.id)?;
        std::ptr::eq(first, page).then_some(page.id.as_str())
    }

    /// Why a key of the page showing has no state or timer of its own: its
    /// page has an earlier page's id. See [`Engine::home_page`].
    fn shared_page_text(&self) -> String {
        let id = self.current_page().map_or("", |page| page.id.as_str());
        format!("Rename this page: an earlier page is also \"{id}\"")
    }

    /// The countdown behind a key of the page showing: its own if it has
    /// been started, else a fresh one at its start -- as it always is on a
    /// page with an earlier page's id, where it cannot be started. `None`
    /// for a key with no timer, or with a timer that has no length it can
    /// run.
    fn countdown_of(&self, cfg: &KeyConfig) -> Option<Countdown> {
        let length = countdown_length(cfg.widget.as_ref()).ok()?;
        let started = self
            .home_of(cfg.key)
            .and_then(|which| self.countdowns.get(&which));
        Some(started.cloned().unwrap_or_else(|| Countdown::new(length)))
    }

    /// Whether a key kept by name -- a timer's, or one with states -- is on
    /// the page showing: not while a later page with the same id shows.
    fn is_showing(&self, which: &KeyHome) -> bool {
        which.profile == self.profile_id && self.home_page() == Some(which.page.as_str())
    }

    /// The timer on a key of the page showing, and its length; or, when it
    /// has none, say so and give nothing.
    fn timer_at(&mut self, key: u8) -> Option<(KeyHome, Option<Duration>)> {
        let length = countdown_length(self.key_config(key).and_then(|cfg| cfg.widget.as_ref()));
        match (length, self.home_of(key)) {
            (Ok(length), Some(which)) => Some((which, length)),
            (Err(why), _) => {
                self.show_osd(why.into(), None);
                None
            }
            // The key has a timer, so a page is showing: one with an
            // earlier page's id.
            (Ok(_), None) => {
                self.show_osd(self.shared_page_text(), None);
                None
            }
        }
    }

    /// A tap on a timer key: start, pause or resume it, or put a finished
    /// one back to its start.
    fn toggle_timer(&mut self, key: u8) {
        let Some((which, length)) = self.timer_at(key) else {
            return;
        };
        // Finished first, so one that ran out a moment ago runs its
        // `on_done` and is then reset by this tap, rather than pausing at
        // 0:00 with its `on_done` never run.
        self.finish_countdowns();
        let now = self.clock.now();
        let fresh = Countdown::new(length);
        let countdown = self
            .countdowns
            .entry(which.clone())
            .or_insert_with(|| fresh.clone());
        countdown.toggle(now);
        if *countdown == fresh {
            self.countdowns.remove(&which);
        }
        self.arm_countdowns();
        self.show_countdown(&which);
    }

    /// A hold on a timer key: back to its start, unless it is counting down.
    fn reset_timer(&mut self, key: u8) {
        let Some((which, _)) = self.timer_at(key) else {
            return;
        };
        self.finish_countdowns();
        let now = self.clock.now();
        if let Some(countdown) = self.countdowns.get_mut(&which) {
            if countdown.reset(now) == crate::countdown::Reset::Refused {
                // A slow tap reaches a hold, and must not wipe out twenty
                // minutes of a running timer.
                self.show_osd("Tap to pause, hold to reset".into(), None);
                return;
            }
            self.countdowns.remove(&which);
        }
        self.arm_countdowns();
        self.show_countdown(&which);
    }

    /// Wake when the first running timer runs out. One deadline for every
    /// timer, whatever page it is on, since only the soonest matters.
    fn arm_countdowns(&mut self) {
        self.scheduler.cancel_kind(TimerKind::CountdownDue);
        if let Some(end) = self.countdowns.values().filter_map(Countdown::end).min() {
            self.scheduler.at(end, TimerKind::CountdownDue);
        }
    }

    /// Finish every timer that has run out, on any page.
    fn finish_countdowns(&mut self) {
        let now = self.clock.now();
        let mut finished: Vec<KeyHome> = self
            .countdowns
            .iter_mut()
            .filter_map(|(which, countdown)| countdown.finish_if_due(now).then(|| which.clone()))
            .collect();
        finished.sort();
        self.arm_countdowns();
        for which in finished {
            self.timer_done(&which);
        }
    }

    /// A timer has run out: say so, flash its key if it is showing, and do
    /// what it was set to do.
    fn timer_done(&mut self, which: &KeyHome) {
        let Some(cfg) = key_at_home(&self.workspace, which).cloned() else {
            return;
        };
        let Some(widget) = cfg.widget.as_ref() else {
            return;
        };
        log::info!(
            "the timer on key {} of {}/{} is done",
            which.key,
            which.profile,
            which.page
        );
        self.preview.publish(galdeck_ipc::Event::TimerDone {
            profile: which.profile.clone(),
            page: which.page.clone(),
            key: which.key,
        });
        // First, since it may be anything: a page switch decides whether the
        // timer's key is showing, and a message of its own -- "Page 2/3" --
        // must not be what is left on the screen in place of this one.
        if let Some(action) = &widget.on_done {
            self.perform(action, From::Timer, 1);
        }
        // Out of sight, the message waits for someone to come back to the
        // deck: a timer is for when you are looking at something else.
        let showing = self.is_showing(which);
        let hold = if showing {
            OsdHold::For(TIMER_DONE_HOLD)
        } else {
            OsdHold::UntilInput(TIMER_DONE_WAIT)
        };
        self.show_osd_for(done_text(widget, cfg.label.as_deref()), None, hold);
        if showing {
            self.show_countdown(which);
            if !self.scheduler.contains_kind(TimerKind::CountdownFlash) {
                self.restart_flash();
            }
        }
    }

    /// Show a timer's key as it is now, if its page is showing. Painted
    /// whether or not the time changed: pausing dims it, and finishing or
    /// resetting changes what lies over it.
    fn show_countdown(&mut self, which: &KeyHome) {
        if !self.is_showing(which) {
            return;
        }
        let slot = Slot::Key(which.key);
        if let Some(widget) = self.widget_at(slot).cloned() {
            self.tick_countdown(slot, &widget);
        }
        self.repaint_key(which.key);
    }

    /// Keys of the page showing whose finished timer is flashing at `now`.
    fn flashing_keys(&self, now: Tick) -> Vec<u8> {
        let Some(page) = self.current_page() else {
            return Vec::new();
        };
        page.keys
            .iter()
            .filter(|cfg| cfg.key < Buttons::COUNT)
            .filter(|cfg| {
                self.home_of(cfg.key)
                    .and_then(|which| self.countdowns.get(&which))
                    .is_some_and(|countdown| countdown.flashing(now))
            })
            .map(|cfg| cfg.key)
            .collect()
    }

    /// Start the flash tick for the keys flashing on the page showing, or
    /// stop it when none is.
    ///
    /// The tick only repaints. Whether a key is lit is worked out from the
    /// clock at each paint, so a key repainted for any other reason agrees
    /// with it, and changes at most twice a second however often it paints.
    fn restart_flash(&mut self) {
        self.scheduler.cancel_kind(TimerKind::CountdownFlash);
        let now = self.clock.now();
        self.flashing = self.flashing_keys(now);
        if !self.flashing.is_empty() {
            let phase = crate::countdown::FLASH_PHASE;
            self.scheduler
                .every(now.saturating_add(phase), phase, TimerKind::CountdownFlash);
        }
    }

    /// Repaint the flashing keys, and the ones whose flash has just ended,
    /// which now rest in the alarm colour until tapped.
    fn flash_timers(&mut self) {
        let now = self.clock.now();
        let flashing = self.flashing_keys(now);
        let mut keys = std::mem::replace(&mut self.flashing, flashing.clone());
        keys.extend(flashing);
        keys.sort_unstable();
        keys.dedup();
        for key in keys {
            self.repaint_key(key);
        }
        if self.flashing.is_empty() {
            self.scheduler.cancel_kind(TimerKind::CountdownFlash);
        }
    }

    /// Keep through a reload only the timers whose key still counts the same
    /// thing: a timer of the same length, or a stopwatch. Anything else is a
    /// different timer now, and starts from nothing.
    fn keep_countdowns(&mut self) {
        let workspace = &self.workspace;
        self.countdowns.retain(|which, countdown| {
            let widget = key_at_home(workspace, which).and_then(|cfg| cfg.widget.as_ref());
            countdown_length(widget) == Ok(countdown.length())
        });
        self.arm_countdowns();
    }

    // ------------------------------------------------------------ states

    /// What a key with states is showing, as far as anyone watching it can
    /// tell: to see what a change changed.
    fn state_seen(&self, home: &KeyHome) -> StateSeen {
        let shown = key_at_home(&self.workspace, home).and_then(|cfg| self.shown_state(home, cfg));
        let cycle = self.key_states.get(home);
        StateSeen {
            shown,
            known: cycle.is_some_and(|cycle| cycle.known),
            badge: cycle.and_then(|cycle| cycle.badge(self.clock.now())),
        }
    }

    /// What is kept of the key at `home`, `cfg`, from now on if not before.
    fn cycle_mut(&mut self, home: &KeyHome, cfg: &KeyConfig) -> &mut KeyCycle {
        self.key_states
            .entry(home.clone())
            .or_insert_with(|| KeyCycle::new(cfg))
    }

    /// Show what has changed about the key at `home` since `before`: draw
    /// it again if its page is showing -- its animation too, when it is in
    /// another state -- and tell anyone watching when its state changed.
    fn state_changed(&mut self, home: &KeyHome, before: StateSeen) {
        let after = self.state_seen(home);
        if after == before {
            return;
        }
        if after.shown != before.shown || after.known != before.known {
            self.preview.publish(galdeck_ipc::Event::KeyStateChanged {
                profile: home.profile.clone(),
                page: home.page.clone(),
                key: home.key,
                state: after.shown.clone(),
                known: after.known,
            });
        }
        if !self.is_showing(home) {
            return;
        }
        if after.shown != before.shown {
            self.rebuild_key_animation(home.key);
        }
        self.repaint_key(home.key);
    }

    /// A tap, or `next_state` or `previous_state`, on key `key` of the page
    /// showing: on to its next state, going round, or back to the one
    /// before.
    fn step_state(&mut self, key: u8, forward: bool) {
        let Some(cfg) = self.key_config(key).cloned() else {
            return;
        };
        if cfg.states.is_empty() {
            self.show_osd("No states on this key".into(), None);
            return;
        }
        let Some(home) = self.home_of(key) else {
            self.show_osd(self.shared_page_text(), None);
            return;
        };
        let from = self.shown_state(&home, &cfg);
        if let Some(to) = crate::states::neighbour(&cfg, from.as_deref(), forward) {
            self.enter_state(&home, &cfg, to);
        }
    }

    /// Move the key at `home`, `cfg`, to state `to` because someone asked:
    /// show it at once and say so, then run what entering it runs -- or,
    /// while the key's last command is still running, once that is done.
    fn enter_state(&mut self, home: &KeyHome, cfg: &KeyConfig, to: String) {
        let before = self.state_seen(home);
        let start = self.cycle_mut(home, cfg).press(to.clone());
        self.show_osd(crate::states::state_text(cfg, &to), None);
        self.state_changed(home, before);
        if start {
            self.start_enter(home, &to);
        }
        self.arm_key_states();
    }

    /// Run what the key at `home` runs as it enters `state`.
    ///
    /// A shell command goes to the state runner, and the key waits for it.
    /// Anything else is done here and counts as done at once: a keystroke or
    /// a built-in has no exit status to wait for.
    fn start_enter(&mut self, home: &KeyHome, state: &str) {
        use crate::states::{Outcome, StateJob, StateKind};
        let action = key_at_home(&self.workspace, home)
            .and_then(|cfg| cfg.state(state))
            .and_then(|state| state.exec.clone());
        let now = self.clock.now();
        self.state_seq += 1;
        let seq = self.state_seq;
        let Some(cycle) = self.key_states.get_mut(home) else {
            return;
        };
        cycle.begin(seq, state.to_string(), now);
        let done = Outcome::Exited {
            code: 0,
            stdout_first_line: None,
            stderr_first_line: None,
        };
        let outcome = match action {
            Some(galdeck_model::Action::Shell(command)) => {
                log::info!("exec: {command}");
                let job = StateJob {
                    home: home.clone(),
                    seq,
                    kind: StateKind::Enter { command },
                };
                if self.state_runner.spawn(job) {
                    self.arm_key_states();
                    return;
                }
                // The runner still has a command for this key that the key
                // itself has forgotten, as it does after a reload took the
                // key's states away while one ran and another put them back.
                Outcome::Failed("its last command is still running".into())
            }
            Some(action) => {
                self.perform(&action, From::State(home.key), 1);
                done
            }
            None => done,
        };
        self.on_entered(home, seq, &outcome);
    }

    /// What the key at `home` ran as it entered a state has finished, or
    /// has been running long enough to count as launched.
    fn on_entered(&mut self, home: &KeyHome, seq: u64, outcome: &crate::states::Outcome) {
        use crate::states::{Entered, Outcome};
        let Some(cfg) = key_at_home(&self.workspace, home)
            .filter(|cfg| !cfg.states.is_empty())
            .cloned()
        else {
            // It has lost its states since, and there is nothing to show.
            self.key_states.remove(home);
            return;
        };
        let succeeded = matches!(outcome, Outcome::Exited { code: 0, .. } | Outcome::Launched);
        let before = self.state_seen(home);
        let now = self.clock.now();
        let Some(cycle) = self.key_states.get_mut(home) else {
            return;
        };
        match cycle.entered(seq, succeeded, now) {
            Entered::Stale => return,
            Entered::Next(state) => {
                self.state_changed(home, before);
                self.start_enter(home, &state);
            }
            Entered::Settled => {
                if cfg.status.is_some() {
                    cycle.expect_reads(now);
                }
                if *outcome == Outcome::Launched {
                    log::info!(
                        "key {} on {}/{}: its command is still running, and is taken as done",
                        home.key,
                        home.profile,
                        home.page
                    );
                }
                self.state_changed(home, before);
            }
            Entered::Reverted => {
                let text = crate::states::failure_text(&cfg, home.key, outcome);
                log::warn!("key {} on {}/{}: {text}", home.key, home.profile, home.page);
                self.show_osd(text, None);
                self.state_changed(home, before);
            }
        }
        self.arm_key_states();
    }

    /// Something a key with states ran has reported.
    fn on_state_report(&mut self, report: crate::states::StateReport) {
        match report.kind {
            crate::states::StateKind::Enter { .. } => {
                self.on_entered(&report.home, report.seq, &report.outcome);
            }
            crate::states::StateKind::Status { .. } => {
                self.on_state_read(&report.home, report.seq, &report.outcome);
            }
        }
    }

    /// Read which state the key at `home` is in, unless it is being read
    /// already or has a command running or waiting, whose change a read
    /// would only race.
    fn read_state(&mut self, home: &KeyHome) {
        use crate::states::{StateJob, StateKind};
        let Some(cfg) = key_at_home(&self.workspace, home)
            .filter(|cfg| !cfg.states.is_empty())
            .cloned()
        else {
            return;
        };
        let Some(command) = cfg.status.clone().filter(|c| !c.trim().is_empty()) else {
            return;
        };
        if !self.cycle_mut(home, &cfg).may_read() {
            return;
        }
        self.state_seq += 1;
        let seq = self.state_seq;
        let job = StateJob {
            home: home.clone(),
            seq,
            kind: StateKind::Status { command },
        };
        if self.state_runner.spawn(job) {
            self.cycle_mut(home, &cfg).reading(seq);
        }
    }

    /// A read of the key at `home` has answered.
    ///
    /// Taken only if nobody has changed the key since it began and no
    /// command of the key's is running or waiting; see [`KeyCycle::read`].
    /// A change it finds is shown and told, but not put on the screen: the
    /// key changing is the news, and nobody at the deck asked for it.
    fn on_state_read(&mut self, home: &KeyHome, seq: u64, outcome: &crate::states::Outcome) {
        let cfg = key_at_home(&self.workspace, home)
            .filter(|cfg| !cfg.states.is_empty() && cfg.status.is_some())
            .cloned();
        let now = self.clock.now();
        let before = self.state_seen(home);
        let Some(cycle) = self.key_states.get_mut(home) else {
            return;
        };
        let Some(cfg) = cfg else {
            // It cannot be read any more; only the read is forgotten.
            if cycle.reading.is_some_and(|(reading, _)| reading == seq) {
                cycle.reading = None;
            }
            return;
        };
        let (found, last) = crate::states::judge(&cfg, outcome, now);
        let failing = cycle.failures > 0;
        let why = last.error.clone().or_else(|| {
            last.output
                .as_ref()
                .map(|output| format!("it printed {output:?}, which is none of its states"))
        });
        if !cycle.read(seq, found, last, now) {
            return;
        }
        // Once a run of failures, not at every read: the key's "?" says the
        // rest, and a read that keeps failing is spaced out anyway.
        if cycle.failures > 0 && !failing {
            log::warn!(
                "key {} on {}/{}: its state could not be read: {}",
                home.key,
                home.profile,
                home.page,
                why.unwrap_or_default()
            );
        }
        self.state_changed(home, before);
    }

    /// Key `key`'s read is due: read it, and come back after its interval,
    /// or longer while its reads keep finding nothing.
    fn poll_state(&mut self, key: u8) {
        if !self.device.connected {
            return;
        }
        let Some(cfg) = self
            .key_config(key)
            .filter(|cfg| !cfg.states.is_empty() && cfg.status.is_some())
            .cloned()
        else {
            return;
        };
        let Some(home) = self.home_of(key) else {
            return;
        };
        self.read_state(&home);
        let every = Duration::from_millis(u64::from(cfg.status_interval_ms()));
        let interval = self
            .key_states
            .get(&home)
            .map_or(every, |cycle| cycle.read_interval(every));
        let now = self.clock.now();
        self.scheduler
            .after(now, interval, TimerKind::StateStatus { key });
    }

    /// Read the state of every key on this page that can read it, now and
    /// every so often while the page shows; stop reading the page before's.
    fn start_state_reads(&mut self, page: &Page) {
        self.stop_state_reads();
        let now = self.clock.now();
        for cfg in &page.keys {
            if cfg.key < Buttons::COUNT && !cfg.states.is_empty() && cfg.status.is_some() {
                self.scheduler
                    .at(now, TimerKind::StateStatus { key: cfg.key });
            }
        }
    }

    /// Stop reading the states of the keys on the page: the page or the deck
    /// is going.
    fn stop_state_reads(&mut self) {
        for key in Buttons::indices() {
            self.scheduler.cancel_kind(TimerKind::StateStatus { key });
        }
    }

    /// Act on whatever about a key with states has come due, on any page.
    fn key_states_due(&mut self) {
        let now = self.clock.now();
        let mut repaint = Vec::new();
        let mut read = Vec::new();
        for (home, cycle) in &mut self.key_states {
            let due = cycle.due(now);
            if due.repaint {
                repaint.push(home.clone());
            }
            if due.read {
                read.push(home.clone());
            }
        }
        for home in read {
            self.read_state(&home);
        }
        for home in repaint {
            if self.is_showing(&home) {
                self.repaint_key(home.key);
            }
        }
        self.arm_key_states();
    }

    /// Wake when the first thing about a key with states is due.
    fn arm_key_states(&mut self) {
        self.scheduler.cancel_kind(TimerKind::KeyStates);
        if let Some(at) = self
            .key_states
            .values()
            .filter_map(KeyCycle::next_deadline)
            .min()
        {
            self.scheduler.at(at, TimerKind::KeyStates);
        }
    }

    /// Keep through a reload only the keys that still have states, each as
    /// far as it still makes sense; see [`KeyCycle::keep`]. Nothing is run:
    /// a key showing a state again is not entering it.
    fn keep_key_states(&mut self) {
        let workspace = &self.workspace;
        self.key_states.retain(|home, cycle| {
            match key_at_home(workspace, home).filter(|cfg| !cfg.states.is_empty()) {
                Some(cfg) => {
                    cycle.keep(cfg);
                    true
                }
                None => false,
            }
        });
        self.arm_key_states();
    }

    /// Put key `key` of the page showing in state `state`, for an editor:
    /// with `run`, as a tap to it would, running what the state runs;
    /// without, on the key and nowhere else.
    fn set_key_state(&mut self, key: u8, state: &str, run: bool) -> Response {
        let error = |message: String| Response::Error { message };
        // A key past the deck's last loads, with an error, and is never
        // drawn: nor is it put in a state, which would draw it.
        let Some(cfg) = self
            .key_config(key)
            .filter(|_| key < Buttons::COUNT)
            .cloned()
        else {
            return error(format!("there is no key {key} on this page"));
        };
        if cfg.states.is_empty() {
            return error(format!("key {key} has no states"));
        }
        let Some(name) = cfg.state(state).map(|state| state.name.clone()) else {
            return error(format!("key {key} has no state called {state:?}"));
        };
        // The key was found, so a page is showing: one with an earlier
        // page's id.
        let Some(home) = self.home_of(key) else {
            return error(self.shared_page_text());
        };
        if run {
            self.enter_state(&home, &cfg, name);
        } else {
            let before = self.state_seen(&home);
            self.cycle_mut(&home, &cfg).show(name);
            self.state_changed(&home, before);
            self.arm_key_states();
        }
        Response::Ok
    }

    /// Draw key `key` of the page showing as it looks in `state`, or in none,
    /// for an editor's preview of each. Without what its state lays over it:
    /// the preview is of the look.
    fn render_key_state(&self, key: u8, state: Option<&str>) -> Response {
        let error = |message: String| Response::Error { message };
        let Some(cfg) = self.key_config(key).filter(|_| key < Buttons::COUNT) else {
            return error(format!("there is no key {key} on this page"));
        };
        let look = match state {
            Some(name) if cfg.state(name).is_none() => {
                return error(format!("key {key} has no state called {name:?}"));
            }
            Some(name) => cfg.in_state(name),
            None => own_look(cfg),
        };
        let size = self.key_placement(key).size();
        let (canvas, _) = self.look_picture(&look, size, self.backdrop_key(key, size), None);
        match canvas.to_jpeg(90) {
            Ok(jpeg) => Response::Image {
                url: format!("data:image/jpeg;base64,{}", crate::base64::encode(&jpeg)),
            },
            Err(e) => error(format!("encoding the preview: {e}")),
        }
    }

    // ------------------------------------------------------------ icons

    /// Find the icon themes for the config as it is now, on a thread of
    /// their own: asking the desktop which theme it uses can take a second.
    /// The themes found before stay in use until these arrive. While a
    /// search is under way this only asks for another once it is done.
    fn find_icons(&mut self) {
        self.icons_wanted += 1;
        if !self.icons_finding {
            self.search_icons();
        }
    }

    /// Start the search for the themes of the reload that wants them.
    fn search_icons(&mut self) {
        let wanted = self.icons_wanted;
        let configured = self.workspace.global.icon_theme.clone();
        let tx = self.icons_tx.clone();
        let waker = self.waker.clone();
        let spawned = std::thread::Builder::new()
            .name("galdeck-icons".into())
            .spawn(move || {
                let themes = crate::icons::IconThemes::discover(configured.as_deref());
                if tx.send((wanted, Arc::new(themes))).is_ok() {
                    waker.notify();
                }
            });
        match spawned {
            Ok(_) => self.icons_finding = true,
            Err(e) => {
                log::warn!("looking for icon themes on a thread: {e}; looking here");
                let configured = self.workspace.global.icon_theme.as_deref();
                let themes = crate::icons::IconThemes::discover(configured);
                self.on_icons(wanted, Arc::new(themes));
            }
        }
    }

    /// The icon themes have been found: draw the keys that name an icon
    /// again, and answer the editors waiting for the icon names. Themes
    /// found for a config that has been reloaded since are not the ones
    /// wanted, and the search starts again.
    fn on_icons(&mut self, found_for: u64, themes: Arc<crate::icons::IconThemes>) {
        self.icons_finding = false;
        if found_for != self.icons_wanted {
            self.search_icons();
            return;
        }
        let icons = IconSet {
            themes,
            names: Default::default(),
        };
        for reply in self.icon_names_waiting.drain(..) {
            answer_icon_names(&icons, reply);
        }
        self.icons = Some(icons);
        for key in Buttons::indices() {
            let named = self
                .shown_key(key)
                .is_some_and(|cfg| matches!(cfg.icon, Some(galdeck_model::IconRef::Name(_))));
            if named {
                if self.key_animations[key as usize].is_some() {
                    self.rebuild_key_animation(key);
                }
                self.repaint_key(key);
            }
        }
    }

    /// Warnings about the icons the keys on the page name that cannot be
    /// drawn, each at its place in the config: a name no icon theme has, and
    /// a file, named or found for a name, that is refused or fails to load.
    /// Nothing about names until the themes are found.
    fn icon_warnings(&self, page: &Page) -> Vec<galdeck_ipc::Diagnostic> {
        use crate::icons::{Missing, Origin};
        use galdeck_model::IconRef;
        let mut warnings = Vec::new();
        for (index, cfg) in page.keys.iter().enumerate() {
            let at = format!("pages[{}].keys[{index}]", self.page_index);
            // Where each name is written, and the look it is drawn in: a
            // state that names none draws the key's, which is warned about
            // once, at the key.
            let own = std::iter::once((format!("{at}.icon"), &cfg.icon, own_look(cfg)));
            let states = cfg.states.iter().enumerate().map(|(s, state)| {
                let look = cfg.in_state(&state.name);
                (format!("{at}.states[{s}].icon"), &state.icon, look)
            });
            for (path, written, look) in own.chain(states) {
                let Some(written) = written else {
                    continue;
                };
                // A name that cannot be one is the config's own error.
                if written
                    .name()
                    .is_some_and(|name| !IconRef::is_valid_name(name))
                {
                    continue;
                }
                let style = self.style_for(Some(&look.style), "key");
                let size = self.key_placement(cfg.key).size();
                let icon_box = render::icon_box(size, &style, self.label_for(&look).is_some());
                let (file, origin, what) = match written {
                    IconRef::Path(file) => (
                        crate::icons::expand_home(file),
                        Origin::File,
                        format!("the picture {}", file.display()),
                    ),
                    IconRef::Name(name) => {
                        let Some(icons) = &self.icons else {
                            continue;
                        };
                        let what = |file: &std::path::Path| {
                            format!("the icon {name:?} ({})", file.display())
                        };
                        match icons.themes.lookup(name, icon_box) {
                            Ok(file) => {
                                let what = what(&file);
                                (file, Origin::Theme, what)
                            }
                            Err(Missing::NotFound) => {
                                let themes = icons.themes.theme_names().join(", ");
                                warnings.push(IconRef::not_found(name, &path, &themes));
                                continue;
                            }
                            Err(Missing::Refused { path: file, reason }) => {
                                warnings.push(IconRef::cannot_draw(&what(&file), &path, &reason));
                                continue;
                            }
                        }
                    }
                };
                // Drawn as the key draws it, into the cache the key draws
                // from, so an icon looked at here is not drawn again when
                // its state is shown.
                let drawn = self.icon_cache.borrow_mut().draw(
                    &file,
                    origin,
                    icon_box,
                    style.key_label_color,
                );
                if let Err(reason) = drawn {
                    warnings.push(IconRef::cannot_draw(&what, &path, &reason));
                }
            }
        }
        warnings
    }

    // ------------------------------------------------------------ levels

    /// The devices whose levels the page shows: on mute and push-to-talk
    /// keys, and on the rings of knobs that turn them.
    fn level_targets(&self) -> Vec<crate::audio::AudioTarget> {
        let keys = self
            .current_page()
            .map(|page| {
                page.keys
                    .iter()
                    .filter(|cfg| cfg.key < Buttons::COUNT)
                    .filter_map(|cfg| state_key(cfg).map(|state| state.target()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let rings = Encoders::indices()
            .filter_map(|encoder| knob_audio_target(&self.encoder_plan(encoder)));
        let mut targets = Vec::new();
        for target in keys.into_iter().chain(rings) {
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        targets
    }

    /// Read the levels the page shows now, and again every so often while
    /// it shows any: something other than the deck may have changed them
    /// since, and will again.
    fn watch_levels(&mut self) {
        self.scheduler.cancel_kind(TimerKind::LevelPoll);
        if self.read_levels() {
            let now = self.clock.now();
            self.scheduler.every(
                now.saturating_add(LEVEL_POLL),
                LEVEL_POLL,
                TimerKind::LevelPoll,
            );
        }
    }

    /// Ask for the levels the page shows, and say whether it shows any.
    fn read_levels(&mut self) -> bool {
        let targets = self.level_targets();
        let any = !targets.is_empty();
        for target in targets {
            self.controls
                .service(crate::controls::ServiceJob::Read { target });
        }
        any
    }

    /// Read the levels the page shows again, while there are any to show and
    /// a deck to show them on.
    fn poll_levels(&mut self) {
        if !self.device.connected || !self.read_levels() {
            self.scheduler.cancel_kind(TimerKind::LevelPoll);
        }
    }

    /// Take a device's level as the sound server reported it, and show it
    /// wherever it shows: level rings, mute keys and volume widgets.
    fn on_level(&mut self, target: crate::audio::AudioTarget, level: crate::audio::Level) {
        self.unread.remove(&target);
        let was = self.levels.insert(target.clone(), level);
        // A ring changes colour only with mute, and rests as if live until
        // it knows.
        if was.is_some_and(|known| known.muted) != level.muted {
            self.rest_level_rings(&target);
        }
        // A key shows nothing until it knows, so learning is a change too.
        if was.map(|known| known.muted) != Some(level.muted) {
            self.repaint_state_keys(&target);
        }
        // Volume widgets showing this device agree with the knob at once,
        // rather than at their next sample.
        let slots: Vec<Slot> = self.widget_state.keys().copied().collect();
        for slot in slots {
            let Some(widget) = self.widget_at(slot).cloned() else {
                continue;
            };
            let matches = widget.kind == galdeck_model::WidgetKind::Volume
                && crate::audio::AudioTarget::parse(widget.source.as_deref()).ok()
                    == Some(target.clone());
            if !matches {
                continue;
            }
            let reading = crate::widgets::volume_reading(level.percent, level.muted);
            let changed = self
                .widget_state
                .get_mut(&slot)
                .is_some_and(|state| state.correct(Some(reading), &widget));
            if changed {
                match slot {
                    Slot::Key(key) => self.repaint_key(key),
                    Slot::Tile(_) => self.screen_dirty = true,
                }
            }
        }
    }

    /// Forget a device's level, which a failure has made unknown: its keys
    /// go back to showing nothing, and its rings to their own colour.
    fn forget_level(&mut self, target: &crate::audio::AudioTarget) {
        let Some(was) = self.levels.remove(target) else {
            return;
        };
        if was.muted {
            self.rest_level_rings(target);
        }
        self.repaint_state_keys(target);
    }

    /// Repaint the keys of the page showing that show `target`'s state.
    fn repaint_state_keys(&mut self, target: &crate::audio::AudioTarget) {
        let keys: Vec<u8> = self
            .current_page()
            .map(|page| {
                page.keys
                    .iter()
                    .filter(|cfg| cfg.key < Buttons::COUNT)
                    .filter(|cfg| state_key(cfg).is_some_and(|state| state.target() == *target))
                    .map(|cfg| cfg.key)
                    .collect()
            })
            .unwrap_or_default();
        for key in keys {
            self.repaint_key(key);
        }
    }

    /// Remember which knob asked about a level, so the answer lands on its
    /// ring and not on whichever knob happens to be turned next.
    fn note_level_asker(&mut self, from: From, target: crate::audio::AudioTarget) {
        if let From::Knob(encoder) = from {
            self.level_asker = Some((encoder, target, self.clock.now()));
        }
    }

    /// Show where a volume change will land before the sound server says, so
    /// the ring moves with the hand; the report corrects it a moment later.
    fn predict_level(&mut self, from: From, target: crate::audio::AudioTarget, delta: f64) {
        self.note_level_asker(from, target.clone());
        let From::Knob(encoder) = from else {
            return;
        };
        let Some(known) = self.levels.get(&target).copied() else {
            return;
        };
        let percent = (known.percent + delta).clamp(0.0, crate::controls::CEILING);
        // Turning up unmutes; turning down leaves mute alone.
        let muted = known.muted && delta < 0.0;
        let now = self.clock.now();
        self.rings[encoder as usize].level((percent / 100.0) as f32, muted, now);
        self.paint_ring(encoder);
        self.show_osd(
            level_text(&target, percent, muted),
            Some(((percent / 100.0) as f32, muted)),
        );
    }

    /// Move `by` pages through the profile, wrapping, without leaving a trail
    /// for `back`: a knob is a dial, not a series of visits.
    fn step_page(&mut self, by: i64, from: From) {
        let Some(count) = self.current_profile().map(|p| p.pages.len()) else {
            return;
        };
        if count == 0 {
            return;
        }
        let index = (self.page_index as i64 + by).rem_euclid(count as i64) as usize;
        self.go_to_page(index, from);
    }

    fn go_to_page(&mut self, index: usize, from: From) {
        let Some(profile) = self.current_profile() else {
            return;
        };
        let count = profile.pages.len();
        let Some(id) = profile.pages.get(index).map(|p| p.id.clone()) else {
            return;
        };
        if index != self.page_index {
            self.page_index = index;
            self.paint_page();
            self.preview.publish(galdeck_ipc::Event::PageChanged {
                profile: self.profile_id.clone(),
                page: id.clone(),
            });
        }
        if let From::Knob(encoder) = from {
            let now = self.clock.now();
            self.rings[encoder as usize].position(index, now);
            self.paint_ring(encoder);
        }
        self.show_osd(format!("Page {}/{count} · {id}", index + 1), None);
    }

    /// Move `by` profiles, in the order they are listed (alphabetical by
    /// file name), wrapping.
    fn step_profile(&mut self, by: i64, from: From) {
        let ids: Vec<String> = self.workspace.profiles.keys().cloned().collect();
        if ids.is_empty() {
            return;
        }
        let here = ids
            .iter()
            .position(|id| *id == self.profile_id)
            .unwrap_or(0);
        let index = (here as i64 + by).rem_euclid(ids.len() as i64) as usize;
        let id = ids[index].clone();
        self.switch_profile(&id);
        if let From::Knob(encoder) = from {
            let now = self.clock.now();
            self.rings[encoder as usize].position(index, now);
            self.paint_ring(encoder);
        }
        self.show_osd(format!("Profile {}/{} · {id}", index + 1, ids.len()), None);
    }

    /// Set the deck's brightness now. Not saved: the config's value is what
    /// the deck comes back to after a restart.
    fn set_brightness(&mut self, percent: u8) {
        self.brightness = percent;
        self.send(Paint::Brightness(percent));
        self.preview.set_brightness(percent);
        self.preview
            .publish(galdeck_ipc::Event::BrightnessChanged { percent });
    }

    /// Put a short message over the bottom of the screen for a moment.
    fn show_osd(&mut self, text: String, level: Option<(f32, bool)>) {
        self.show_osd_for(text, level, OsdHold::For(OSD_HOLD));
    }

    /// Put a message over the bottom of the screen for as long as `hold`
    /// says.
    fn show_osd_for(&mut self, text: String, level: Option<(f32, bool)>, hold: OsdHold) {
        let (most, until_input) = match hold {
            OsdHold::For(most) => (most, false),
            OsdHold::UntilInput(most) => (most, true),
        };
        let until = self.clock.now().saturating_add(most);
        self.osd = Some(Osd {
            text,
            level,
            until_input,
        });
        self.scheduler.cancel_kind(TimerKind::OsdEnd);
        self.scheduler.at(until, TimerKind::OsdEnd);
        self.screen_dirty = true;
    }

    /// Someone is at the deck: a message that was waiting for them has been
    /// seen.
    fn osd_seen(&mut self) {
        if self.osd.as_ref().is_some_and(|osd| osd.until_input) {
            self.osd = None;
            self.scheduler.cancel_kind(TimerKind::OsdEnd);
            self.screen_dirty = true;
        }
    }

    /// Take the microphone out of push-to-talk, if a key had it live.
    fn end_talk(&mut self) {
        if self.talking.take().is_some() {
            self.controls.talk(false);
        }
    }

    /// Whether a key's tap is push-to-talk, which acts on the key's edges
    /// rather than through the gesture machine.
    fn is_push_to_talk(&self, key: u8) -> bool {
        self.key_config(key)
            .and_then(KeyConfig::tap)
            .and_then(|action| action.as_built_in().map(|i| i.action))
            == Some(galdeck_model::BuiltIn::PushToTalk)
    }

    /// Where a knob's ring rests: its style's colour, or its mode's while it
    /// has modes, and red over either while the level it turns is muted, so a
    /// muted microphone never looks like a live one.
    fn ring_rest(&self, encoder: u8, plan: &galdeck_model::EncoderPlan, base: Rgb) -> Rgb {
        let base = match &plan.modes {
            Some(stack) => self
                .mode_rests(&stack.entries, base)
                .get(plan.mode)
                .copied()
                .unwrap_or(base),
            None => base,
        };
        let level = match self.knob_app(encoder, plan) {
            Some(app) => self.app_levels.get(&app),
            None => knob_audio_target(plan).and_then(|target| self.levels.get(&target)),
        };
        if level.is_some_and(|level| level.muted) {
            crate::ring::muted_rest(base)
        } else {
            base
        }
    }

    /// The colour a mode names for its ring, if it names one.
    fn mode_color(&self, entry: &galdeck_model::ModeEntry) -> Option<Rgb> {
        let color = entry.ring.as_ref()?;
        // Quietly: a colour the palette lacks was reported when the config
        // loaded, and repainting a ring must not report it again.
        self.palette
            .resolve(color, "modes.ring", &mut Diagnostics::new())
    }

    /// Where each mode of a stack rests; see [`mode_rings`].
    fn mode_rests(&self, entries: &[galdeck_model::ModeEntry], base: Rgb) -> Vec<Rgb> {
        let named: Vec<Option<Rgb>> = entries.iter().map(|e| self.mode_color(e)).collect();
        mode_rings(&named, base)
    }

    /// The app a knob's turn moves, once the knob has acted on one: an
    /// app-volume turn with a target has acted on the app that target found.
    fn knob_app(&self, encoder: u8, plan: &galdeck_model::EncoderPlan) -> Option<String> {
        use galdeck_model::BuiltIn;
        let (action, _) = plan.cw.as_ref().or(plan.ccw.as_ref())?;
        let invocation = action.as_built_in()?;
        if !matches!(
            invocation.action,
            BuiltIn::AppVolumeUp | BuiltIn::AppVolumeDown
        ) {
            return None;
        }
        self.knob_apps
            .get(encoder as usize)?
            .as_ref()
            .map(|app| app.key.clone())
    }

    /// Record where a knob's ring rests now, without painting it.
    fn rest_ring(&mut self, encoder: u8, plan: &galdeck_model::EncoderPlan) {
        let style = self.style_for(Some(&plan.style), &format!("encoders[{encoder}].style"));
        let base = self.ring_rest(encoder, plan, style.ring);
        if let Some(ring) = self.rings.get_mut(encoder as usize) {
            ring.rest(base);
        }
    }

    /// Re-rest the rings of the knobs that turn `target`, whose mute changed.
    fn rest_level_rings(&mut self, target: &crate::audio::AudioTarget) {
        self.rest_rings_where(|_, _, plan| knob_audio_target(plan).as_ref() == Some(target));
    }

    /// Re-rest the rings of the knobs that turn `app`, whose mute changed or
    /// which a knob has just taken up.
    fn rest_app_rings(&mut self, app: &str) {
        self.rest_rings_where(|engine, encoder, plan| {
            engine.knob_app(encoder, plan).as_deref() == Some(app)
        });
    }

    /// Re-rest and repaint the rings of the knobs `test` picks, given each
    /// knob's plan on the page showing.
    fn rest_rings_where(&mut self, test: impl Fn(&Self, u8, &galdeck_model::EncoderPlan) -> bool) {
        for index in Encoders::indices() {
            // An animated ring is the animation's to paint.
            if self.ring_animations[index as usize].is_some() {
                continue;
            }
            let plan = self.encoder_plan(index);
            if !test(self, index, &plan) {
                continue;
            }
            self.rest_ring(index, &plan);
            self.paint_ring(index);
        }
    }

    /// What a worker came back with.
    fn on_control_report(&mut self, report: crate::controls::ControlReport) {
        use crate::controls::ControlReport;
        use crate::widgets::media::Outcome;
        let now = self.clock.now();
        match report {
            ControlReport::Volume { target, level } => {
                self.on_level(target.clone(), level);
                if let Some((encoder, asked, at)) = self.level_asker.clone() {
                    if asked == target && now.duration_since(at) < LEVEL_REPORT_WINDOW {
                        let fraction = (level.percent / 100.0) as f32;
                        self.rings[encoder as usize].level(fraction, level.muted, now);
                        self.paint_ring(encoder);
                        self.show_osd(
                            level_text(&target, level.percent, level.muted),
                            Some((fraction, level.muted)),
                        );
                    }
                }
            }
            ControlReport::Read { target, level } => match level {
                Ok(level) => self.on_level(target, level),
                Err(reason) => {
                    self.forget_level(&target);
                    // Once, rather than at every poll: a missing sound tool
                    // is not going to turn up in two seconds.
                    if self.unread.insert(target.clone()) {
                        log::warn!("reading the level of {target:?}: {reason}");
                        self.show_failure("volume", &reason);
                    }
                }
            },
            ControlReport::LevelFailed {
                target,
                what,
                reason,
            } => {
                self.forget_level(&target);
                self.show_failure(what, &reason);
            }
            ControlReport::Media(outcome) => match outcome {
                Outcome::Done { .. } => {
                    // The card should show what the key just did. The player
                    // takes a moment to change state; ask again shortly.
                    let slots: Vec<Slot> = self.widget_state.keys().copied().collect();
                    for slot in slots {
                        if self.widget_at(slot).map(|w| w.kind)
                            == Some(galdeck_model::WidgetKind::Media)
                        {
                            self.scheduler.cancel_kind(TimerKind::WidgetTick { slot });
                            self.scheduler.after(
                                now,
                                MEDIA_RESAMPLE,
                                TimerKind::WidgetTick { slot },
                            );
                        }
                    }
                }
                Outcome::Unsupported { player, why } => {
                    self.show_osd(format!("{player}: {why}"), None);
                }
                Outcome::NoPlayer => self.show_osd("Nothing is playing".into(), None),
            },
            ControlReport::Output {
                display,
                index,
                count,
                level,
                from,
            } => {
                // The default output is another device now, and whatever was
                // known of the one before -- its level, whether it was muted
                // -- says nothing about this one. Headphones left at full
                // volume must not show as the speakers' quiet level.
                match level {
                    Some(level) => self.on_level(crate::audio::AudioTarget::Output, level),
                    None => self.forget_level(&crate::audio::AudioTarget::Output),
                }
                if let Some(ring) = from.and_then(|encoder| self.rings.get_mut(encoder as usize)) {
                    ring.position(index, now);
                }
                if let Some(encoder) = from {
                    self.paint_ring(encoder);
                }
                self.show_osd(output_text(&display, count, level), level.map(level_bar));
            }
            ControlReport::App {
                app,
                display,
                level,
                from,
                ..
            } => {
                let was = self.app_levels.insert(app.clone(), level);
                let mut rest = was.is_some_and(|known| known.muted) != level.muted;
                // A report on a knob's question is also which app the knob
                // now has, for its next turn and for its ring.
                if let Some(slot) =
                    from.and_then(|encoder| self.knob_apps.get_mut(encoder as usize))
                {
                    let now_has = Some(KnobApp {
                        key: app.clone(),
                        display: display.clone(),
                    });
                    rest |= *slot != now_has;
                    *slot = now_has;
                }
                if rest {
                    self.rest_app_rings(&app);
                }
                if let Some(ring) = from.and_then(|encoder| self.rings.get_mut(encoder as usize)) {
                    let (fraction, muted) = level_bar(level);
                    ring.level(fraction, muted, now);
                }
                if let Some(encoder) = from {
                    self.paint_ring(encoder);
                }
                self.show_osd(
                    format!("{display} · {}", percent_text(level)),
                    Some(level_bar(level)),
                );
            }
            // The knob keeps its app: one that has paused and dropped its
            // stream is usually about to play again, and a knob that jumped
            // to whatever else is making sound would move the wrong thing.
            // Pressing it moves on.
            ControlReport::NoApp { app: None, .. } => {
                self.show_osd("No app is playing sound".into(), None);
            }
            ControlReport::NoApp {
                app: Some(name),
                from,
            } => {
                // By the name the knob learnt with it, "Firefox", when that
                // is the app the job asked for, rather than by the program's
                // name it asked by, "firefox".
                let learnt = from
                    .and_then(|encoder| self.knob_apps.get(encoder as usize)?.clone())
                    .filter(|known| {
                        crate::pipewire::display_name(&known.key, crate::pipewire::DISPLAY_LENGTH)
                            == name
                    })
                    .map(|known| known.display);
                self.show_osd(format!("{} isn't playing", learnt.unwrap_or(name)), None);
            }
            ControlReport::Failed { what, reason } => self.show_failure(what, &reason),
        }
    }

    /// Say on the screen that something outside the daemon failed.
    fn show_failure(&mut self, what: &str, reason: &str) {
        let mut text = format!("{what}: {reason}");
        if text.chars().count() > 60 {
            text = text.chars().take(59).collect::<String>() + "…";
        }
        self.show_osd(text, None);
    }

    /// Everything the model knows how to do, for an editor.
    fn catalog(&self) -> galdeck_ipc::Catalog {
        use galdeck_model::{BuiltIn, Preset, SlotRule, StepUnit};
        let describe = |action: Option<galdeck_model::Action>| action.map(|a| a.describe());
        galdeck_ipc::Catalog {
            built_ins: BuiltIn::ALL
                .iter()
                .map(|b| {
                    let step = b.step();
                    galdeck_ipc::BuiltInInfo {
                        name: b.name().into(),
                        group: b.group().into(),
                        label: b.describe().into(),
                        step_unit: step.map(|(unit, ..)| {
                            match unit {
                                StepUnit::Percent => "percent",
                                StepUnit::Seconds => "seconds",
                                StepUnit::Notches => "notches",
                            }
                            .into()
                        }),
                        step_default: step.map(|(_, d, ..)| d),
                        step_min: step.map(|(_, _, min, _)| min),
                        step_max: step.map(|(.., max)| max),
                        takes_target: b.takes_target(),
                        needs_virtual_input: b.needs_virtual_input(),
                        keys_only: matches!(
                            b.slot_rule(),
                            SlotRule::KeyTapOnly | SlotRule::KeyOnly
                        ),
                        knobs_only: b.slot_rule() == SlotRule::KnobOnly,
                        target_kind: b.target_kind().map(|kind| kind.name().into()),
                        needs_mixer: needs_mixer(*b),
                    }
                })
                .collect(),
            presets: Preset::ALL
                .iter()
                .map(|p| {
                    let [press, cw, ccw] = p.gestures(None, None);
                    let needs_virtual_input = [&press, &cw, &ccw]
                        .into_iter()
                        .flatten()
                        .any(galdeck_model::Action::needs_virtual_input);
                    let needs_mixer = [&press, &cw, &ccw]
                        .into_iter()
                        .flatten()
                        .filter_map(galdeck_model::Action::as_built_in)
                        .any(|invocation| needs_mixer(invocation.action));
                    galdeck_ipc::PresetInfo {
                        name: p.name().into(),
                        title: p.title().into(),
                        label: p.describe().into(),
                        needs_virtual_input,
                        press: describe(press),
                        cw: describe(cw),
                        ccw: describe(ccw),
                        ring_shows: p.ring().map(ring_shows_name),
                        target_kind: p.target_kind().map(|kind| kind.name().into()),
                        needs_mixer,
                    }
                })
                .collect(),
            keys: galdeck_model::keys::KEYS
                .iter()
                .map(|k| galdeck_ipc::KeyNameInfo {
                    name: k.name.into(),
                    group: k.group.into(),
                })
                .collect(),
        }
    }

    fn capabilities(&self) -> galdeck_ipc::Capabilities {
        galdeck_ipc::Capabilities {
            virtual_input: if self.workspace.global.virtual_input {
                self.controls.virtual_input_state()
            } else {
                "off".into()
            },
            audio: crate::controls::audio_tool().into(),
            media: "ok".into(),
            mixer: match crate::audio::mixer() {
                Ok(()) => "ok".into(),
                Err(why) => format!("unavailable: {why}"),
            },
            desktop: std::env::var("XDG_CURRENT_DESKTOP")
                .map(|desktop| crate::pipewire::display_name(&desktop, 64))
                .unwrap_or_default(),
        }
    }

    /// Do something because an editor asked, as a key would.
    fn run_action(
        &mut self,
        fields: std::collections::BTreeMap<String, galdeck_model::Value>,
    ) -> Response {
        let table: toml::Table = fields
            .into_iter()
            .map(|(key, value)| (key, toml_value(value)))
            .collect();
        let action: galdeck_model::Action = match toml::Value::Table(table).try_into() {
            Ok(action) => action,
            Err(e) => {
                return Response::Error {
                    message: format!("not an action: {e}"),
                }
            }
        };
        // The ones that act on their own key or knob have none here.
        if let Some(built_in) = action.as_built_in().map(|i| i.action) {
            use galdeck_model::SlotRule;
            let message = match built_in.slot_rule() {
                SlotRule::Anywhere => None,
                SlotRule::KeyTapOnly => {
                    Some("push-to-talk needs a key held down; try it on the deck".to_string())
                }
                SlotRule::KeyOnly => Some(format!(
                    "{} acts on the key it is bound to; try it on the deck",
                    built_in.name()
                )),
                SlotRule::KnobOnly => Some(format!(
                    "{} acts on the knob it is bound to; try it on the deck",
                    built_in.name()
                )),
            };
            if let Some(message) = message {
                return Response::Error { message };
            }
        }
        self.perform(&action, From::Editor, 1);
        Response::Ok
    }

    /// Put a calibration pattern on every key.
    ///
    /// A bright border drawn exactly at the image's edge, corner marks, and
    /// the size in the middle. If the border sits flush with the physical key
    /// the size is right; if anything shows outside it the image is too small
    /// for the panel, which is what leaves boot imagery around the edges.
    ///
    /// Deliberately not diffed away or restored on its own: it is a
    /// diagnostic, the page comes back with `galdeck reload`, and a pattern
    /// that vanished by itself would be useless for looking at.
    fn test_pattern(&mut self, size: u32) -> Response {
        const MIN: u32 = 32;
        const MAX: u32 = 512;
        if !(MIN..=MAX).contains(&size) {
            return Response::Error {
                message: format!("size must be {MIN}-{MAX}, got {size}"),
            };
        }
        if !self.device.connected {
            return Response::Error {
                message: "no device is connected".into(),
            };
        }

        let mut canvas = galdeck::Canvas::filled(size, size, Rgb::new(10, 10, 40));
        let edge = Rgb::new(255, 220, 0);
        // Three pixels, so a one-pixel crop is still visible as a thinner line
        // rather than as nothing at all.
        for inset in 0..3 {
            canvas.draw_rect(
                inset as i32,
                inset as i32,
                size - inset * 2,
                size - inset * 2,
                edge,
            );
        }
        // Corner blocks: if a corner is missing, that edge is off the panel.
        let mark = (size / 8).max(4);
        for (x, y) in [
            (0, 0),
            (size - mark, 0),
            (0, size - mark),
            (size - mark, size - mark),
        ] {
            canvas.fill_rect(x as i32, y as i32, mark, mark, Rgb::new(255, 60, 60));
        }
        if let Some(font) = self.font.as_ref() {
            let style = galdeck::TextStyle::new(font, size as f32 / 4.0)
                .color(Rgb::WHITE)
                .align(galdeck::Align::Center)
                .max_width(size - 8);
            canvas.draw_text(&size.to_string(), size as i32 / 2, size as i32 / 2, &style);
        }

        let jpeg = match canvas.to_jpeg(galdeck::ids::DEFAULT_JPEG_QUALITY) {
            Ok(jpeg) => Arc::<[u8]>::from(jpeg),
            Err(e) => {
                return Response::Error {
                    message: format!("encoding the pattern: {e}"),
                }
            }
        };
        for index in Buttons::indices() {
            self.send(Paint::Key {
                index,
                target: KeyTarget::Jpeg(Arc::clone(&jpeg)),
            });
        }
        log::info!("drew a {size}x{size} calibration pattern; `galdeck reload` restores the page");
        Response::Ok
    }

    /// Fill every calibrated zone through the panel region path.
    ///
    /// Where `test_pattern` asks how much of a key the firmware's key path
    /// reaches, this asks whether the region path reaches the rest. Two
    /// things are worth watching for: whether the lower rows draw at all --
    /// the framework has only ever confirmed region writes down to y=448,
    /// and three of four rows sit below that -- and whether what is drawn
    /// stays put, or the firmware repaints the key area from its own image
    /// buffer a moment later.
    fn zone_pattern(&mut self) -> Response {
        if !self.device.connected {
            return Response::Error {
                message: "no device is connected".into(),
            };
        }
        let zones = self.calibration.zones();
        if zones.is_empty() {
            return Response::Error {
                message: "the calibration has no zones".into(),
            };
        }

        let mut drawn = 0;
        let mut skipped = Vec::new();
        for zone in zones {
            let rect = zone.bounds;
            // The firmware shears an off-block region rather than rejecting
            // it, so the drawn size is floored to whole blocks. A calibrated
            // zone is normally already a multiple of 8; this is for the one
            // that was nudged by hand.
            let width = galdeck::Panel::to_mcu_floor(rect.width);
            let height = galdeck::Panel::to_mcu_floor(rect.height);
            if width == 0
                || height == 0
                || rect.x as u32 + width as u32 > galdeck::ids::PANEL_WIDTH as u32
                || rect.y as u32 + height as u32 > galdeck::ids::PANEL_HEIGHT as u32
            {
                skipped.push(zone.index);
                continue;
            }

            let mut canvas =
                galdeck::Canvas::filled(width as u32, height as u32, Rgb::new(10, 10, 40));
            // On the zone's exact edge, so a band of unpainted panel around
            // it is the thing you are looking for.
            for inset in 0..3 {
                canvas.draw_rect(
                    inset,
                    inset,
                    width as u32 - inset as u32 * 2,
                    height as u32 - inset as u32 * 2,
                    Rgb::new(0, 230, 90),
                );
            }
            if let Some(font) = self.font.as_ref() {
                let style = galdeck::TextStyle::new(font, height as f32 / 3.0)
                    .color(Rgb::WHITE)
                    .align(galdeck::Align::Center)
                    .max_width(width as u32 - 8);
                canvas.draw_text(
                    &zone.index.to_string(),
                    width as i32 / 2,
                    height as i32 / 2,
                    &style,
                );
            }
            let jpeg = match canvas.to_jpeg(galdeck::ids::DEFAULT_JPEG_QUALITY) {
                Ok(jpeg) => Arc::<[u8]>::from(jpeg),
                Err(e) => {
                    return Response::Error {
                        message: format!("encoding zone {}: {e}", zone.index),
                    }
                }
            };
            self.send(Paint::Key {
                index: zone.index,
                target: KeyTarget::Region {
                    x: rect.x,
                    y: rect.y,
                    width,
                    height,
                    jpeg,
                },
            });
            drawn += 1;
        }

        log::info!(
            "drew {drawn} zone(s) through the region path; `galdeck reload` restores the page"
        );
        if !skipped.is_empty() {
            return Response::Error {
                message: format!(
                    "drew {drawn} zone(s); {} could not be drawn ({skipped:?}): \
                     the rectangle does not fit the panel in whole 8px blocks",
                    skipped.len()
                ),
            };
        }
        Response::Ok
    }

    /// How key `index` should be addressed on this unit.
    ///
    /// Falls back to the firmware path whenever the region path would be no
    /// better or not safe: with no measured calibration there is no
    /// rectangle to draw at, and where a zone is no larger than the image
    /// the key path already blits, the extra bytes buy nothing.
    fn key_placement(&self, index: u8) -> KeyPlacement {
        if !self.zone_paint {
            return KeyPlacement::Firmware;
        }
        // A template is arithmetic, not a measurement. Drawing at rectangles
        // nobody checked against the hardware is how you end up with content
        // half off the keycap.
        if self.calibration.source == galdeck::layout::Source::Template {
            return KeyPlacement::Firmware;
        }
        let Some(zone) = self.calibration.zone_at(index) else {
            return KeyPlacement::Firmware;
        };
        let rect = zone.bounds;
        // The firmware shears an off-block region rather than rejecting it.
        let width = galdeck::Panel::to_mcu_floor(rect.width);
        let height = galdeck::Panel::to_mcu_floor(rect.height);
        let key_pixels = galdeck::ids::KEY_PIXELS as u16;
        if width <= key_pixels && height <= key_pixels {
            return KeyPlacement::Firmware;
        }
        if u32::from(rect.x) + u32::from(width) > u32::from(galdeck::ids::PANEL_WIDTH)
            || u32::from(rect.y) + u32::from(height) > u32::from(galdeck::ids::PANEL_HEIGHT)
        {
            log::warn!("zone {index} does not fit the panel; using the key path");
            return KeyPlacement::Firmware;
        }
        KeyPlacement::Zone {
            x: rect.x,
            y: rect.y,
            width,
            height,
        }
    }

    /// What to put on a key with nothing configured.
    ///
    /// An image rather than `Button::clear()`, which is `set_color(BLACK)` and
    /// therefore a `03 06` feature report. That fill does not replace what the
    /// key's image buffer holds, so on a freshly connected module the boot
    /// animation stays visible underneath it -- which is exactly what it
    /// looked like on hardware.
    ///
    /// It is also cheaper: a feature report costs a forced 2 ms, while a
    /// kilobyte of flat JPEG is a couple of unpaced writes.
    fn blank_target(&mut self, placement: KeyPlacement) -> KeyTarget {
        let size = placement.size();
        if let std::collections::btree_map::Entry::Vacant(slot) = self.blank_keys.entry(size) {
            let canvas = galdeck::Canvas::filled(size.0, size.1, Rgb::BLACK);
            match canvas.to_jpeg(galdeck::ids::DEFAULT_JPEG_QUALITY) {
                Ok(jpeg) => {
                    slot.insert(jpeg.into());
                }
                Err(e) => {
                    log::warn!("encoding the blank key failed: {e}");
                    // The feature-report fill is better than nothing.
                    return KeyTarget::Color(Rgb::BLACK);
                }
            }
        }
        match self.blank_keys.get(&size) {
            Some(jpeg) => placement.target(Arc::clone(jpeg)),
            None => KeyTarget::Color(Rgb::BLACK),
        }
    }

    /// Send one ring's current colours and re-arm its return-to-rest timer.
    fn paint_ring(&mut self, encoder: u8) {
        let now = self.clock.now();
        let Some(ring) = self.rings.get_mut(encoder as usize) else {
            return;
        };
        ring.expire(now);
        let colors = ring.colors();
        let deadline = ring.deadline();

        self.preview.set_ring(encoder, colors);
        self.send(Paint::Ring { encoder, colors });

        // Registered as a deadline rather than polled for, which is what lets
        // an idle daemon cost nothing at all.
        self.scheduler.cancel_kind(TimerKind::RingRest { encoder });
        if let Some(at) = deadline {
            self.scheduler.at(at, TimerKind::RingRest { encoder });
        }
    }

    fn handle_event(&mut self, event: Event, at: Tick) {
        log::debug!("event: {event:?}");
        if matches!(
            event,
            Event::KeyDown(_) | Event::EncoderDown(_) | Event::EncoderRotate(..)
        ) {
            self.osd_seen();
        }
        match event {
            Event::KeyDown(key) => {
                self.preview.publish(galdeck_ipc::Event::KeyPressed { key });
                // Before anything the press does, so the key answers at once.
                self.start_press(key, at);
                // Push-to-talk works on the key's own edges: the microphone
                // is live exactly while the key is down.
                if self.is_push_to_talk(key) {
                    self.talking = Some(key);
                    self.controls.talk(true);
                    self.show_osd("Microphone live".into(), None);
                    return;
                }
                let bindings = self.bindings_for(key);
                let decision = self.input.down(key, at, bindings);
                self.act_on(key, decision);
            }
            Event::KeyUp(key) => {
                if self.talking == Some(key) {
                    self.end_talk();
                    self.show_osd("Microphone muted".into(), None);
                    return;
                }
                let bindings = self.bindings_for(key);
                let decision = self.input.up(key, at, bindings);
                self.act_on(key, decision);
            }
            Event::EncoderDown(encoder) => {
                self.preview
                    .publish(galdeck_ipc::Event::EncoderPressed { encoder });
                if let Some(ring) = self.rings.get_mut(encoder as usize) {
                    ring.click(at);
                    self.paint_ring(encoder);
                }
                if encoder as usize >= self.knobs.len() {
                    return;
                }
                self.nav_latch[encoder as usize] = None;
                let plan = self.encoder_plan(encoder);
                let press = plan.press.map(|(action, _)| action);
                let hold = plan.hold.map(|(action, _)| action);
                self.scheduler.cancel_kind(TimerKind::KnobHold { encoder });
                if let Some(held) = &hold {
                    // With a hold bound, a press cannot be answered until the
                    // knob comes back up -- the same rule keys follow.
                    let switches_mode = held
                        .as_built_in()
                        .is_some_and(|i| i.action == galdeck_model::BuiltIn::NextMode);
                    let after = if switches_mode {
                        KNOB_MODE_HOLD
                    } else {
                        Duration::from_millis(crate::input::HOLD_MS)
                    };
                    self.scheduler
                        .at(at.saturating_add(after), TimerKind::KnobHold { encoder });
                    self.knobs[encoder as usize] = KnobState {
                        down: true,
                        press,
                        hold,
                        ..KnobState::default()
                    };
                } else {
                    // Answered now, so nothing is left for the release.
                    self.knobs[encoder as usize] = KnobState {
                        down: true,
                        ..KnobState::default()
                    };
                    if let Some(action) = press {
                        self.perform(&action, From::Knob(encoder), 1);
                    }
                }
            }
            Event::EncoderUp(encoder) => {
                let Some(knob) = self.knobs.get_mut(encoder as usize) else {
                    return;
                };
                let was = std::mem::take(knob);
                self.scheduler.cancel_kind(TimerKind::KnobHold { encoder });
                // A press that turned into a turn was a grip on the knob,
                // not a click; nobody means both.
                if was.hold.is_some() && was.down && !was.turned && !was.hold_fired {
                    if let Some(action) = was.press {
                        self.perform(&action, From::Knob(encoder), 1);
                    }
                }
            }
            Event::EncoderRotate(encoder, delta) => {
                self.preview
                    .publish(galdeck_ipc::Event::EncoderTurned { encoder, delta });
                if encoder as usize >= self.knobs.len() {
                    return;
                }
                if self.knobs[encoder as usize].down {
                    self.knobs[encoder as usize].turned = true;
                    self.scheduler.cancel_kind(TimerKind::KnobHold { encoder });
                }
                let plan = self.encoder_plan(encoder);
                // A spin through pages keeps going through pages even when
                // it lands on one that binds this knob to something else.
                let latched = self.nav_latch[encoder as usize]
                    .as_ref()
                    .filter(|latch| at < latch.until)
                    .map(|latch| (latch.cw.clone(), latch.ccw.clone()));
                let is_latched = latched.is_some();
                let (cw, ccw) = latched.unwrap_or_else(|| {
                    (
                        plan.cw.as_ref().map(|(a, _)| a.clone()),
                        plan.ccw.as_ref().map(|(a, _)| a.clone()),
                    )
                });
                let action = if delta > 0 { cw.clone() } else { ccw.clone() };
                // Level and position knobs show those on the ring; everything
                // else gets the moving highlight that says the turn landed.
                let shows_on_ring = plan.ring().is_some() || is_latched;
                if !shows_on_ring {
                    if let Some(ring) = self.rings.get_mut(encoder as usize) {
                        ring.turn(delta, at);
                        self.paint_ring(encoder);
                    }
                }
                let Some(action) = action else {
                    return;
                };
                // Fast turns coalesce into one report with |delta| > 1.
                let detents = (delta.unsigned_abs() as u32).min(MAX_DETENTS_PER_EVENT);
                match &action {
                    galdeck_model::Action::Shell(cmd) => {
                        // Run once per detent, in order, on this knob's own
                        // runner, so a relative command does not collapse a
                        // spin into one step. GALDECK_DELTA carries the sign.
                        if let Some(runner) = self.rotation.get(encoder as usize) {
                            for _ in 0..detents {
                                if !runner.push(cmd, delta) {
                                    log::debug!("encoder {encoder} queue full, dropping a detent");
                                    break;
                                }
                            }
                        }
                    }
                    other => {
                        let navigates = other
                            .as_built_in()
                            .is_some_and(|i| i.action.is_navigation());
                        if navigates {
                            self.nav_latch[encoder as usize] = Some(NavLatch {
                                cw,
                                ccw,
                                until: at.saturating_add(NAV_LATCH),
                            });
                        }
                        self.perform(other, From::Knob(encoder), detents);
                    }
                }
            }
            _ => {}
        }
    }

    /// Every knob as if released without a click: the page it went down on
    /// is gone, or the deck is.
    fn forget_knobs(&mut self) {
        for encoder in Encoders::indices() {
            self.scheduler.cancel_kind(TimerKind::KnobHold { encoder });
        }
        for knob in &mut self.knobs {
            *knob = KnobState::default();
        }
    }

    /// A knob has been down long enough: what holding it does, unless it
    /// has been turned since, which makes it a grip rather than a hold.
    fn knob_held(&mut self, encoder: u8) {
        let Some(knob) = self.knobs.get_mut(encoder as usize) else {
            return;
        };
        if !knob.down || knob.turned || knob.hold_fired {
            return;
        }
        knob.hold_fired = true;
        if let Some(action) = knob.hold.clone() {
            self.perform(&action, From::Knob(encoder), 1);
        }
    }

    fn switch_page(&mut self, id: &str) -> bool {
        let Some(profile) = self.current_profile() else {
            return false;
        };
        match profile.pages.iter().position(|p| p.id == id) {
            Some(index) => {
                if index != self.page_index {
                    // Bounded, because a deck is navigated by hand and an
                    // unbounded stack would grow for as long as the daemon
                    // runs. Dropping the oldest entry loses the far end of a
                    // very long trail, which nobody is walking back anyway.
                    if self.page_stack.len() >= MAX_PAGE_STACK {
                        self.page_stack.remove(0);
                    }
                    self.page_stack.push(self.page_index);
                }
                self.page_index = index;
                self.paint_page();
                self.preview.publish(galdeck_ipc::Event::PageChanged {
                    profile: self.profile_id.clone(),
                    page: id.to_string(),
                });
                true
            }
            None => {
                log::warn!("unknown page {id:?}");
                false
            }
        }
    }

    /// Return to the page this one was reached from.
    fn go_back(&mut self) -> bool {
        match self.page_stack.pop() {
            Some(index) => {
                self.page_index = index;
                self.paint_page();
                true
            }
            None => {
                log::debug!("nothing to go back to");
                false
            }
        }
    }

    fn switch_profile(&mut self, id: &str) -> bool {
        if !self.workspace.profiles.contains_key(id) {
            log::warn!("unknown profile {id:?}");
            return false;
        }
        if id == self.profile_id {
            return true;
        }
        log::info!("switching to profile {id:?}");
        self.profile_id = id.to_string();
        self.enter_profile();
        self.paint_page();
        self.preview.publish(galdeck_ipc::Event::ProfileChanged {
            profile: self.profile_id.clone(),
        });
        true
    }

    /// Re-read the config from disk.
    ///
    /// Parse and validate before swapping, so a broken edit leaves the running
    /// config alone rather than blanking the deck. Where the user was is
    /// preserved by name where that still exists.
    fn reload(&mut self) -> Response {
        let (workspace, diagnostics) = Workspace::load(&self.config_dir);
        let Some(workspace) = workspace else {
            let message = diagnostics
                .iter()
                .map(|d| format!("{}: {}", d.path, d.message))
                .collect::<Vec<_>>()
                .join("\n");
            return Response::Error { message };
        };
        for diagnostic in &diagnostics {
            log::warn!("{}: {}", diagnostic.path, diagnostic.message);
        }

        let was_profile = self.profile_id.clone();
        let was_page = self.current_page().map(|p| p.id.clone());

        self.font = render::load_font(workspace.global.font.as_deref());
        // What a knob set stays set through a save; only a new value in the
        // file itself replaces it.
        if workspace.global.brightness != self.workspace.global.brightness {
            self.brightness = workspace.global.brightness;
        }
        self.profile_id = if workspace.profiles.contains_key(&was_profile) {
            was_profile
        } else {
            workspace
                .start_profile()
                .map(str::to_string)
                .unwrap_or_default()
        };
        self.workspace = workspace;
        self.keep_countdowns();
        self.keep_dial_modes();
        self.keep_key_states();
        self.find_icons();
        self.enter_profile();
        self.load_documents();
        self.controls
            .set_virtual_input(self.workspace.global.virtual_input);
        self.prepare_virtual_input();

        if let Some(id) = was_page {
            if let Some(index) = self
                .current_profile()
                .and_then(|p| p.pages.iter().position(|page| page.id == id))
            {
                self.page_index = index;
            }
        }
        self.paint_page();
        self.preview.publish(galdeck_ipc::Event::ConfigChanged);
        Response::Ok
    }

    fn service_control(&mut self) {
        while let Ok(msg) = self.control_rx.try_recv() {
            // One request in the whole protocol cannot be answered where it
            // is handled, so it is taken apart here rather than making every
            // other arm carry a Reply it never uses.
            if matches!(msg.request, Request::ReleaseDevice) {
                match self.release_device() {
                    Reply::Now(response) => {
                        let _ = msg.reply.send(*response);
                    }
                    Reply::WhenReleased => self.pending_release.push(msg.reply),
                }
                continue;
            }
            // A download waits on someone else's server, for up to
            // `assets::DOWNLOAD_TIMEOUT`, so it goes to a thread of its own.
            if let Request::FetchAsset { url } = &msg.request {
                let url = url.clone();
                let dir = crate::assets::dir(&self.config_dir);
                let reply = msg.reply;
                let spawned = std::thread::Builder::new()
                    .name("galdeck-download".into())
                    .spawn(move || {
                        let _ = reply.send(asset_response(crate::assets::download(&url, &dir)));
                    });
                if let Err(e) = spawned {
                    log::warn!("downloading a picture: {e}");
                }
                continue;
            }
            // Listing media players is a D-Bus round trip per player, and a
            // player that has hung would hold the whole deck while it timed
            // out. Nothing here needs the engine's state, so it goes to a
            // thread with the reply channel and the loop carries on.
            // A place search is a web request: seconds, possibly, and never on
            // the thread that owns the deck.
            if let Request::Geocode { name } = &msg.request {
                let name = name.clone();
                let reply = msg.reply;
                let geocoder = Arc::clone(&self.geocoder);
                let spawned = std::thread::Builder::new()
                    .name("galdeck-geocode".into())
                    .spawn(move || {
                        let response = match geocoder.search(&name) {
                            Ok(places) => Response::Places {
                                places: places
                                    .into_iter()
                                    .map(|place| galdeck_ipc::PlaceInfo {
                                        label: place.label(),
                                        name: place.name,
                                        latitude: place.latitude,
                                        longitude: place.longitude,
                                    })
                                    .collect(),
                            },
                            Err(message) => Response::Error { message },
                        };
                        let _ = reply.send(response);
                    });
                if let Err(e) = spawned {
                    log::warn!("searching for a place: {e}");
                }
                continue;
            }
            // pw-dump takes a tenth of a second here and longer elsewhere,
            // so never on this thread; see `AudioTargets`.
            if matches!(msg.request, Request::AudioTargets) {
                let reply = msg.reply;
                self.audio_targets.ask(move |found| {
                    let _ = reply.send(targets_response(found));
                });
                continue;
            }
            // Listing the icon names reads every directory the themes list;
            // it waits for the themes, which are found on a thread too.
            if matches!(msg.request, Request::IconNames) {
                match &self.icons {
                    Some(icons) => answer_icon_names(icons, msg.reply),
                    None if self.icon_names_waiting.len() < MAX_ICON_NAME_WAITERS => {
                        self.icon_names_waiting.push(msg.reply);
                    }
                    None => {
                        let _ = msg.reply.send(Response::Error {
                            message: "still looking for the icon themes".into(),
                        });
                    }
                }
                continue;
            }
            if matches!(msg.request, Request::WidgetSources) {
                let reply = msg.reply;
                let spawned = std::thread::Builder::new()
                    .name("galdeck-sources".into())
                    .spawn(move || {
                        let sources = crate::widgets::sources::discover();
                        let _ = reply.send(Response::WidgetSources(sources));
                    });
                if let Err(e) = spawned {
                    log::warn!("listing widget sources: {e}");
                }
                continue;
            }
            let response = self.handle_request(msg.request);
            let _ = msg.reply.send(response);
        }
    }

    /// Hand the device to another process.
    ///
    /// Parking the io thread is the easy half. The hard half is *when* to
    /// answer: `Ok` is read by the caller as permission to open the hidraw
    /// node, and two handles on one node is the leading suspect for the
    /// module dropping off the USB bus. So the reply waits for the io thread
    /// to say the handle is gone.
    fn release_device(&mut self) -> Reply {
        let was_parked = self.parked.swap(true, Ordering::Relaxed);

        // Already gone -- unplugged, or released twice -- so no Disconnected
        // is coming and a deferred reply would hang the caller forever.
        if !self.device.connected {
            return Reply::Now(Box::new(Response::Ok));
        }
        if !was_parked {
            self.preview.publish(galdeck_ipc::Event::DeviceReleased);
        }
        Reply::WhenReleased
    }

    /// Take the device back.
    ///
    /// Answering at once is correct here, and asymmetric with the release on
    /// purpose: nothing bad happens if the caller carries on before the io
    /// thread has reopened the node, and opening blocks for over a second.
    fn resume_device(&mut self) -> Response {
        if !self.parked.swap(false, Ordering::Relaxed) {
            // Not parked, so this is a stray resume. Harmless, and saying so
            // beats pretending a handover happened.
            return Response::Ok;
        }
        // The only thing that changes a calibration is a wizard run, and a
        // wizard run always ends here -- so this is the one moment the file
        // is worth re-reading without being asked.
        self.load_calibration();
        self.preview.publish(galdeck_ipc::Event::DeviceResumed);
        // Nothing is painted from here: the io thread reopens the device,
        // which arrives as Connected, and that path already repaints
        // everything from scratch.
        Response::Ok
    }

    /// Read the calibration from disk, falling back to the template.
    ///
    /// A failure is recorded rather than logged and forgotten: an
    /// uncalibrated daemon behaves plausibly and looks wrong, so the UI has
    /// to be able to say which it is looking at.
    fn load_calibration(&mut self) {
        match PanelLayout::load(&self.calibration_path) {
            Ok(layout) => {
                self.calibration = layout;
                self.calibration_problem = None;
            }
            Err(error) => {
                log::info!(
                    "no usable calibration at {} ({error}); using the template",
                    self.calibration_path.display()
                );
                self.calibration = PanelLayout::TEMPLATE;
                self.calibration_problem = Some(error.to_string());
            }
        }
    }

    /// The calibration as the protocol describes it.
    fn calibration_snapshot(&self) -> Calibration {
        let grid = self.calibration.grid;
        Calibration {
            path: self.calibration_path.display().to_string(),
            source: match self.calibration.source {
                galdeck::layout::Source::Template => CalSource::Template,
                galdeck::layout::Source::File => CalSource::File,
                galdeck::layout::Source::Calibrated => CalSource::Calibrated,
            },
            screen: to_cal_rect(self.calibration.screen),
            bounds: to_cal_rect(grid.bounds),
            rows: grid.rows,
            columns: grid.columns,
            bleed_x: grid.bleed_x,
            bleed_y: grid.bleed_y,
            zones: self
                .calibration
                .zones()
                .into_iter()
                .map(|zone| CalZone {
                    row: zone.row,
                    column: zone.column,
                    index: zone.index,
                    bounds: to_cal_rect(zone.bounds),
                    overridden: zone.overridden,
                })
                .collect(),
            panel_width: galdeck::ids::PANEL_WIDTH,
            panel_height: galdeck::ids::PANEL_HEIGHT,
            key_image_size: galdeck::ids::KEY_PIXELS,
            text: self.calibration.to_text(),
            released: self.parked.load(Ordering::Relaxed),
            problem: self.calibration_problem.clone(),
        }
    }

    /// Replace the grid and save.
    ///
    /// Validated before anything is written, and the candidate is built by
    /// cloning the live layout rather than from scratch, so the measured
    /// parts of a calibration survive an edit that only meant to move the
    /// boundary. Changing the row or column count still discards the bands,
    /// which is the framework's rule and the right one: a per-row
    /// measurement means nothing once the rows have moved.
    fn set_calibration(
        &mut self,
        screen: CalRect,
        bounds: CalRect,
        rows: u8,
        columns: u8,
        bleed_x: i16,
        bleed_y: i16,
    ) -> Response {
        let mut candidate = self.calibration.clone();
        candidate.screen = from_cal_rect(screen);
        candidate.set_grid(PanelGrid {
            bounds: from_cal_rect(bounds),
            rows,
            columns,
            bleed_x,
            bleed_y,
        });
        if let Err(problem) = candidate.validate() {
            return Response::Error {
                message: problem.to_string(),
            };
        }
        // Calibrated, not File: these numbers describe this unit, and a later
        // reader has no way to tell they were typed rather than measured.
        candidate.source = galdeck::layout::Source::Calibrated;
        if let Err(error) = candidate.save(&self.calibration_path) {
            return Response::Error {
                message: format!("saving {}: {error}", self.calibration_path.display()),
            };
        }
        self.calibration = candidate;
        self.calibration_problem = None;
        // Keys and the screen move with it, so the edit is seen on the deck
        // as it is saved rather than at the next page switch.
        self.paint_page();
        self.preview.publish(galdeck_ipc::Event::CalibrationChanged);
        Response::Ok
    }
    fn handle_request(&mut self, request: Request) -> Response {
        match request {
            // Handled in service_control, which owns the reply channel it
            // has to hold on to.
            Request::ReleaseDevice => Response::Ok,
            // Likewise answered on its own thread in service_control; this
            // only runs for a caller that bypassed it.
            Request::WidgetSources => Response::WidgetSources(crate::widgets::sources::discover()),
            Request::Catalog => Response::Catalog(self.catalog()),
            // Answered on its own thread in service_control; this only runs
            // for a caller that bypassed it, and running pw-dump here would
            // stall the deck.
            Request::AudioTargets => Response::Error {
                message: "busy".into(),
            },
            Request::RunAction { fields } => self.run_action(fields),
            // Answered on its own thread in service_control; this only runs
            // for a caller that bypassed it.
            Request::Geocode { .. } => Response::Error {
                message: "busy".into(),
            },
            Request::GetThemes => Response::Themes {
                themes: crate::theme_editor::describe(&self.workspace),
            },
            Request::CreateTheme {
                id,
                name,
                extends,
                copy,
            } => self.create_theme(&id, name.as_deref(), extends.as_deref(), copy.as_deref()),
            Request::PreviewTheme { theme, patches } => self.preview_theme(&theme, &patches),
            Request::KeyboardLayout => Response::KeyboardLayout {
                leds: crate::lighting::editor::places(),
            },
            Request::KeyboardFrame => Response::KeyboardFrame {
                frame: self
                    .preview
                    .keyboard()
                    .map(|frame| crate::lighting::editor::frame_text(&frame)),
            },
            Request::PreviewLighting {
                lighting,
                theme,
                seconds,
                fps,
                presses,
            } => self.preview_lighting(&lighting, theme.as_deref(), seconds, fps, &presses),
            // The control socket answers these itself, and the UI's
            // /api/call refuses them: the engine holds neither the codes nor
            // the token, and a request that reaches it here came by a way
            // that must not be able to mint a sign-in.
            Request::UiLogin { .. } | Request::UiRotateToken => Response::Error {
                message: "only answered on the control socket".into(),
            },
            Request::RenderWidget {
                fields,
                width,
                height,
            } => self.render_widget_preview(fields, width, height),
            // Answered on its own thread in service_control, like Geocode.
            Request::FetchAsset { .. } => Response::Error {
                message: "busy".into(),
            },
            Request::SaveAsset { name, data } => self.save_asset(&name, &data),
            Request::SetKeyState { key, state, run } => self.set_key_state(key, &state, run),
            // Answered on a thread of its own in service_control.
            Request::IconNames => Response::Error {
                message: "busy".into(),
            },
            Request::RenderKeyState { key, state } => self.render_key_state(key, state.as_deref()),
            Request::Which { names } => which(&names),
            Request::ResumeDevice => self.resume_device(),
            Request::GetCalibration => Response::Calibration(self.calibration_snapshot()),
            Request::SetCalibration {
                screen,
                bounds,
                rows,
                columns,
                bleed_x,
                bleed_y,
            } => self.set_calibration(screen, bounds, rows, columns, bleed_x, bleed_y),
            Request::ReloadCalibration => {
                self.load_calibration();
                self.paint_page();
                self.preview.publish(galdeck_ipc::Event::CalibrationChanged);
                Response::Ok
            }
            Request::Ping => Response::Ok,
            Request::Status => Response::Status(Status {
                connected: self.device.connected,
                firmware: self.device.firmware.clone(),
                serial: self.device.serial.clone(),
                profile: self.profile_id.clone(),
                profiles: self.workspace.profiles.keys().cloned().collect(),
                page: self
                    .current_page()
                    .map(|p| p.id.clone())
                    .unwrap_or_default(),
                pages: self
                    .current_profile()
                    .map(|p| p.pages.iter().map(|page| page.id.clone()).collect())
                    .unwrap_or_default(),
                brightness: self.brightness,
                released: self.parked.load(Ordering::Relaxed),
                capabilities: self.capabilities(),
            }),
            Request::SetBrightness { percent, device } => {
                if percent > 100 {
                    return Response::Error {
                        message: "brightness must be 0-100".into(),
                    };
                }
                // The module has exactly one brightness control -- feature
                // report `03 08 <pct>`, documented in the framework's
                // protocol module -- and no way to read it back. Refusing the
                // per-surface values beats dimming the whole panel and
                // reporting success for something that did not happen.
                match device {
                    DeckDevice::All | DeckDevice::LcdPanel => {}
                    DeckDevice::LeftEncoder | DeckDevice::RightEncoder => {
                        return Response::Error {
                            message: "the module has one brightness control for the whole panel; \
                                      there is no per-encoder brightness to set"
                                .into(),
                        };
                    }
                }
                // Answered immediately; the io thread applies it when it next
                // drains. Nothing here waits on the device, which is the whole
                // point of the split.
                self.set_brightness(percent);
                Response::Ok
            }
            Request::SwitchPage { name } => {
                if self.switch_page(&name) {
                    Response::Ok
                } else {
                    Response::Error {
                        message: format!("unknown page {name:?}"),
                    }
                }
            }
            Request::SwitchProfile { name } => {
                if self.switch_profile(&name) {
                    Response::Ok
                } else {
                    Response::Error {
                        message: format!("unknown profile {name:?}"),
                    }
                }
            }
            Request::TestPattern { size } => self.test_pattern(size),
            Request::ZonePattern => self.zone_pattern(),
            Request::GetConfig => Response::Config(self.config_snapshot()),
            Request::GetLayout => match self.layout() {
                Some(layout) => Response::Layout(Box::new(layout)),
                None => Response::Error {
                    message: "no profile is loaded".into(),
                },
            },
            Request::ValidateConfig {
                file,
                patches,
                generation,
            } => match self.stage(&file, &patches, generation) {
                Ok((_, diagnostics)) => Response::Diagnostics { diagnostics },
                Err(message) => Response::Error { message },
            },
            Request::ApplyConfig {
                file,
                patches,
                generation,
            } => {
                let (staged, diagnostics) = match self.stage(&file, &patches, generation) {
                    Ok(result) => result,
                    Err(message) => return Response::Error { message },
                };
                if diagnostics
                    .iter()
                    .any(|d| d.severity == galdeck_ipc::Severity::Error)
                {
                    // Refused rather than written and then complained about.
                    return Response::Diagnostics { diagnostics };
                }
                let Some(document) = self.documents.get_mut(&file) else {
                    return Response::Error {
                        message: format!("no such config file {file:?}"),
                    };
                };
                if let Err(e) = document.commit(staged) {
                    return Response::Error {
                        message: e.render(),
                    };
                }
                if let Err(e) = document.save() {
                    return Response::Error {
                        message: format!("saving {file}: {e}"),
                    };
                }
                self.reload()
            }
            Request::Reload => self.reload(),
        }
    }
}

/// Serialized runner for one encoder's rotation actions.
///
/// Rotation commands are usually relative read-modify-writes
/// (`wpctl set-volume ... 2%+`), so detents run concurrently all read the
/// same starting value and collapse into a single step — a spin then
/// moves the volume by one notch instead of the eight it earned. Each
/// encoder gets one worker thread that runs its detents strictly in
/// order, one finishing before the next starts.
struct RotationRunner {
    tx: SyncSender<(String, i8)>,
}

impl RotationRunner {
    fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel::<(String, i8)>(ROTATION_QUEUE_DEPTH);
        std::thread::spawn(move || {
            while let Ok((cmd, delta)) = rx.recv() {
                run_action(&cmd, Some(delta));
            }
        });
        RotationRunner { tx }
    }

    /// Queue one detent, returning false if the worker is still behind. A
    /// knob is a live control: dropping detents from an unusually fast
    /// spin beats replaying them seconds after the hand has stopped.
    fn push(&self, cmd: &str, delta: i8) -> bool {
        self.tx.try_send((cmd.to_string(), delta)).is_ok()
    }
}

/// Run a shell command without blocking the engine; a helper thread reaps it.
/// Run a shell command to completion. Only the rotation workers call this;
/// everything else goes through [`spawn_action`] so the engine keeps
/// polling the device.
fn run_action(cmd: &str, delta: Option<i8>) {
    log::info!("exec: {cmd}");
    match crate::actions::action_command(cmd, delta).status() {
        Ok(status) if !status.success() => log::warn!("action exited with {status}"),
        Err(e) => log::warn!("spawning action failed: {e}"),
        _ => {}
    }
}

/// What anyone watching a key with states can see of it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StateSeen {
    shown: Option<String>,
    known: bool,
    badge: Option<crate::states::Badge>,
}

/// A key with states as it looks in none of them: its own look.
fn own_look(cfg: &KeyConfig) -> KeyConfig {
    KeyConfig {
        states: Vec::new(),
        status: None,
        status_interval_ms: None,
        ..cfg.clone()
    }
}

/// Send the icon names in `icons` to `reply`, listing them first on a
/// thread of their own if nobody has asked for them yet.
fn answer_icon_names(icons: &IconSet, reply: Sender<Response>) {
    if let Some(names) = icons.names.get() {
        let _ = reply.send(Response::IconNames {
            names: names.clone(),
        });
        return;
    }
    let icons = icons.clone();
    let spawned = std::thread::Builder::new()
        .name("galdeck-icon-names".into())
        .spawn(move || {
            // Once, however many ask at the same time.
            let names = icons.names.get_or_init(|| icons.themes.symbolic_names());
            let _ = reply.send(Response::IconNames {
                names: names.clone(),
            });
        });
    if let Err(e) = spawned {
        log::warn!("listing icon names: {e}");
    }
}

/// Which of `names` are programs on the daemon's `PATH`, for an editor to
/// say what a ready-made key needs. Only looked for, never run; only plain
/// names, since a path is not a question about `PATH`; and only in the
/// absolute directories on it, where `audio`'s own tools are looked for.
fn which(names: &[String]) -> Response {
    let path = crate::audio::search_path();
    let found = names
        .iter()
        .take(MAX_WHICH)
        .filter(|name| is_program_name(name))
        .filter(|name| crate::audio::find(name, &path).is_some())
        .cloned()
        .collect();
    Response::Which { found }
}

/// Whether `name` could be a program's name rather than a path to one.
fn is_program_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_PROGRAM_NAME
        && name != "."
        && name != ".."
        && !name.contains(['/', '\0'])
}

pub(crate) fn hex(color: galdeck::Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
}

/// The framework's rectangle, as the protocol carries it.
fn to_cal_rect(rect: PanelRect) -> CalRect {
    CalRect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

fn from_cal_rect(rect: CalRect) -> PanelRect {
    PanelRect::new(rect.x, rect.y, rect.width, rect.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A measured calibration with this screen and the template's keys.
    fn measured_screen(screen: PanelRect) -> PanelLayout {
        let mut layout = PanelLayout::TEMPLATE;
        layout.screen = screen;
        layout.source = galdeck::layout::Source::File;
        layout
    }

    #[test]
    fn the_screen_is_laid_out_on_its_measured_height() {
        // What a unit whose glass shows 396 rows calibrates to. Before, the
        // tiles stopped at 384 and the last twelve rows kept whatever the
        // firmware had put there.
        let layout = measured_screen(PanelRect::new(0, 0, 720, 396));
        let placement = ScreenPlacement::from_calibration(&layout);
        assert_eq!(
            placement,
            ScreenPlacement::Measured {
                visible: PanelRect::new(0, 0, 720, 396),
                frame: PanelRect::new(0, 0, 720, 400),
            }
        );
        assert_eq!(placement.size(), (720, 396));

        // The bottom row of a 6-row grid reaches the measured bottom.
        let grid = galdeck_model::LcdGrid {
            columns: 12,
            rows: 6,
        };
        let cells = galdeck_model::Cells {
            column: 0,
            row: 5,
            columns: 12,
            rows: 1,
        };
        let area = tile_area(cells, grid, placement.size());
        assert_eq!(area.y as u32 + area.height + TILE_GAP / 2, 396);
    }

    #[test]
    fn the_screen_uses_the_segment_when_nothing_was_measured() {
        let mut template = measured_screen(PanelRect::new(0, 0, 720, 396));
        template.source = galdeck::layout::Source::Template;
        assert_eq!(
            ScreenPlacement::from_calibration(&template),
            ScreenPlacement::Firmware
        );
        // Measured, and exactly the segment: nothing to gain from the
        // region path.
        let segment = measured_screen(PanelRect::new(0, 0, 720, 384));
        assert_eq!(
            ScreenPlacement::from_calibration(&segment),
            ScreenPlacement::Firmware
        );
    }

    #[test]
    fn a_screen_frame_never_reaches_the_keys() {
        // Rounding 396 out to 400 would cover the top two rows of keys that
        // start at 398, and the screen repaints every second.
        let mut layout = measured_screen(PanelRect::new(0, 0, 720, 396));
        layout.set_grid(PanelGrid {
            bounds: PanelRect::new(0, 398, 720, 848),
            ..layout.grid
        });
        assert_eq!(
            ScreenPlacement::from_calibration(&layout),
            ScreenPlacement::Measured {
                visible: PanelRect::new(0, 0, 720, 384),
                frame: PanelRect::new(0, 0, 720, 384),
            }
        );
    }

    #[test]
    fn no_two_modes_rest_at_the_same_colour() {
        let teal = MODE_RINGS[0];
        let yellow = MODE_RINGS[1];
        let cases: Vec<(Vec<Option<Rgb>>, Rgb)> = vec![
            // Nord: the knob's own ring is the first default.
            (vec![None; 4], teal),
            (vec![None; 4], Rgb::new(40, 40, 40)),
            // A purple near two of the defaults.
            (vec![None; 4], Rgb::new(0xd3, 0x86, 0x9b)),
            (vec![None; 4], Rgb::new(0xc2, 0x8a, 0x8e)),
            // The first mode names a default, on a black knob.
            (vec![Some(teal), None], Rgb::BLACK),
            (vec![Some(yellow), None, None], teal),
            // A later mode names what would have been an earlier one's.
            (vec![None, None, Some(yellow)], Rgb::BLACK),
        ];
        for (named, base) in cases {
            let rests = mode_rings(&named, base);
            for (i, a) in rests.iter().enumerate() {
                for b in &rests[i + 1..] {
                    assert_ne!(a, b, "{named:?} on {base:?}: {rests:?}");
                }
            }
            if named[0].is_none() {
                assert_eq!(rests[0], base);
            }
        }
        // A named colour is kept as named.
        assert_eq!(mode_rings(&[None, Some(yellow)], teal)[1], yellow);
    }

    #[test]
    fn a_knob_knows_which_level_it_turns() {
        use galdeck_model::{Action, BuiltIn, Layer, Preset};
        let plan = |cw: Action| galdeck_model::EncoderPlan {
            cw: Some((cw, Layer::Page)),
            ..Default::default()
        };
        let [_, cw, _] = Preset::Mic.gestures(None, None);
        assert_eq!(
            knob_audio_target(&plan(cw.unwrap())),
            Some(crate::audio::AudioTarget::Input)
        );
        let [_, cw, _] = Preset::Volume.gestures(None, None);
        assert_eq!(
            knob_audio_target(&plan(cw.unwrap())),
            Some(crate::audio::AudioTarget::Output)
        );
        let node = Action::BuiltIn(galdeck_model::Invocation {
            action: BuiltIn::VolumeDown,
            step: None,
            target: Some("42".into()),
        });
        assert_eq!(
            knob_audio_target(&plan(node)),
            Some(crate::audio::AudioTarget::Node("42".into()))
        );
        assert_eq!(knob_audio_target(&plan(Action::Shell("true".into()))), None);
        assert_eq!(
            knob_audio_target(&galdeck_model::EncoderPlan::default()),
            None
        );
    }

    #[test]
    fn an_output_switch_says_where_it_landed_and_how_loud_that_is() {
        use crate::audio::Level;
        let loud = Level {
            percent: 64.4,
            muted: false,
        };
        assert_eq!(
            output_text("Headphones", 3, Some(loud)),
            "Output · Headphones · 64%"
        );
        assert_eq!(
            output_text(
                "HDMI 1",
                2,
                Some(Level {
                    percent: 30.0,
                    muted: true
                })
            ),
            "Output · HDMI 1 · muted"
        );
        assert_eq!(output_text("HDMI 1", 2, None), "Output · HDMI 1");
        // Nothing else to switch to is why the turn changed nothing.
        assert_eq!(
            output_text("Speaker", 1, Some(loud)),
            "Only output · Speaker"
        );
    }

    #[test]
    fn a_mode_switch_names_the_knob_its_place_and_the_preset() {
        use galdeck_model::Preset;
        assert_eq!(
            mode_text(0, 1, 3, Preset::AppVolume),
            "Left knob 2/3 · App volume"
        );
        assert_eq!(mode_text(1, 0, 2, Preset::Pages), "Right knob 1/2 · Pages");
        assert_eq!(knob_name(7), "Knob 7");
    }

    #[test]
    fn audio_targets_leave_out_names_nobody_gave() {
        use crate::pipewire::{AppTarget, OutputTarget, Targets};
        let found = Ok(Targets {
            outputs: vec![OutputTarget {
                display: "Speaker".into(),
                name: "alsa_output.speaker".into(),
                nick: "Speaker".into(),
                description: String::new(),
                usable: true,
                default: true,
            }],
            apps: vec![AppTarget {
                app: "spd".into(),
                display: "speech-dispatcher".into(),
                binary: String::new(),
                running: false,
            }],
        });
        let Response::AudioTargets { outputs, apps } = targets_response(found) else {
            panic!("not audio targets");
        };
        assert_eq!(outputs[0].nick.as_deref(), Some("Speaker"));
        assert_eq!(outputs[0].description, None);
        assert!(outputs[0].usable && outputs[0].default);
        assert_eq!(apps[0].binary, None);
        assert_eq!(apps[0].app, "spd");

        assert!(matches!(
            targets_response(Err("pw-dump is not installed".into())),
            Response::Error { message } if message == "pw-dump is not installed"
        ));
    }

    /// The left knob turns an app's volume and the right one switches
    /// outputs, set once for every page.
    const APP_AND_OUTPUT_KNOBS: &str = "version = 2\nprofile = \"p\"\n\
        [[encoders]]\nencoder = 0\npreset = \"app_volume\"\n\
        [[encoders]]\nencoder = 1\npreset = \"outputs\"\n";

    /// An engine on `global` and a one-page profile, with no deck and
    /// nothing reading what it paints: enough to hand it what a worker
    /// reported and see what it made of it. Nothing here queues a job, so
    /// no test that uses it changes the machine's sound.
    fn engine_with(name: &str, global: &str) -> (Engine, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("galdeck-engine-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        // With the icon theme named, building the engine does not ask the
        // desktop which one it uses: that ask is one at a time across the
        // process, and the icons tests time theirs.
        let global = format!("icon_theme = \"Adwaita\"\n{global}");
        std::fs::write(dir.join("galdeck.toml"), global).unwrap();
        std::fs::write(dir.join("profiles/p.toml"), "[[pages]]\nid = \"one\"\n").unwrap();
        let (workspace, diagnostics) = Workspace::load(&dir);
        let workspace = workspace.unwrap_or_else(|| panic!("config should load: {diagnostics:#?}"));
        let (waker, wake) = galdeck_core::wake_channel();
        let (paint_tx, _) = std::sync::mpsc::sync_channel(64);
        let (_, device_rx) = std::sync::mpsc::sync_channel(1);
        let (_, control_rx) = std::sync::mpsc::channel();
        let (widget_host, widget_rx) = WidgetHost::new(waker.clone());
        let (plugin_host, plugin_rx) = PluginHost::discover(&dir, waker);
        let engine = Engine::new(
            dir.clone(),
            workspace,
            EngineParts {
                control_rx,
                device_rx,
                paint_tx,
                widget_rx,
                wake,
                deadline: Arc::new(DeadlineCell::new()),
                clock: Arc::new(crate::clock::SystemClock::new()),
                shutdown: Arc::new(AtomicBool::new(false)),
                preview: Preview::new(),
                widget_host,
                plugin_host,
                plugin_rx,
                parked: Arc::new(AtomicBool::new(false)),
                zone_paint: false,
                calibration_path: Some(dir.join("no-such-calibration.conf")),
            },
        )
        .expect("engine builds");
        (engine, dir)
    }

    fn osd_text(engine: &Engine) -> Option<&str> {
        engine.osd.as_ref().map(|osd| osd.text.as_str())
    }

    fn level(percent: f64, muted: bool) -> crate::audio::Level {
        crate::audio::Level { percent, muted }
    }

    fn app_report(
        app: &str,
        display: &str,
        level: crate::audio::Level,
        from: Option<u8>,
    ) -> crate::controls::ControlReport {
        crate::controls::ControlReport::App {
            app: app.into(),
            display: display.into(),
            index: 0,
            count: 2,
            level,
            from,
        }
    }

    #[test]
    fn an_app_report_teaches_only_the_knob_that_asked_which_app_it_has() {
        use galdeck_model::{BuiltIn, Invocation};
        let (mut engine, dir) = engine_with("app-report", APP_AND_OUTPUT_KNOBS);
        engine.on_control_report(app_report(
            "spotify",
            "Spotify",
            level(40.0, false),
            Some(0),
        ));
        assert_eq!(
            engine.knob_apps[0],
            Some(KnobApp {
                key: "spotify".into(),
                display: "Spotify".into()
            })
        );
        assert_eq!(osd_text(&engine), Some("Spotify · 40%"));

        // The knob's next turn asks for it by what the worker knows it by;
        // a key without a target takes whatever is playing, every time; and
        // an app named in the config is that app, whatever the knob had.
        let turn = Invocation {
            action: BuiltIn::AppVolumeUp,
            step: None,
            target: None,
        };
        assert_eq!(
            engine.app_for(&turn, From::Knob(0)).as_deref(),
            Some("spotify")
        );
        assert_eq!(engine.app_for(&turn, From::Key(2)), None);
        let named = Invocation {
            target: Some("Firefox".into()),
            ..turn.clone()
        };
        assert_eq!(
            engine.app_for(&named, From::Knob(0)).as_deref(),
            Some("Firefox")
        );

        // A key's answer teaches no knob anything.
        engine.on_control_report(app_report("firefox", "Firefox", level(70.0, false), None));
        assert_eq!(
            engine.knob_apps[0].as_ref().map(|app| app.key.as_str()),
            Some("spotify")
        );
        assert_eq!(engine.knob_apps[1], None);

        // Muted from anywhere, the knob that has the app rests red.
        let base = Rgb::new(40, 40, 40);
        let plan = engine.encoder_plan(0);
        assert_eq!(engine.ring_rest(0, &plan, base), base);
        engine.on_control_report(app_report("spotify", "Spotify", level(40.0, true), None));
        assert_eq!(
            engine.ring_rest(0, &plan, base),
            crate::ring::muted_rest(base)
        );
        assert_eq!(osd_text(&engine), Some("Spotify · muted"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_app_that_is_not_playing_is_named_as_the_knob_learnt_it() {
        use crate::controls::ControlReport;
        let (mut engine, dir) = engine_with("no-app", APP_AND_OUTPUT_KNOBS);
        engine.on_control_report(ControlReport::NoApp {
            app: None,
            from: Some(0),
        });
        assert_eq!(osd_text(&engine), Some("No app is playing sound"));
        assert_eq!(engine.knob_apps[0], None);

        // The job asked by the program's name; the knob says the app's.
        engine.on_control_report(app_report(
            "firefox",
            "Firefox",
            level(70.0, false),
            Some(0),
        ));
        engine.on_control_report(ControlReport::NoApp {
            app: Some("firefox".into()),
            from: Some(0),
        });
        assert_eq!(osd_text(&engine), Some("Firefox isn't playing"));
        // An app named in the config is said as the worker said it.
        engine.on_control_report(ControlReport::NoApp {
            app: Some("Spotify".into()),
            from: Some(0),
        });
        assert_eq!(osd_text(&engine), Some("Spotify isn't playing"));
        // The knob keeps its app for when it plays again.
        assert_eq!(
            engine.knob_apps[0].as_ref().map(|app| app.key.as_str()),
            Some("firefox")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_output_switch_replaces_what_was_known_of_the_output_before() {
        use crate::audio::AudioTarget;
        use crate::controls::ControlReport;
        let (mut engine, dir) = engine_with("output-report", APP_AND_OUTPUT_KNOBS);
        engine.on_level(AudioTarget::Output, level(20.0, true));
        engine.on_control_report(ControlReport::Output {
            display: "Headphones".into(),
            index: 1,
            count: 3,
            level: Some(level(64.0, false)),
            from: Some(1),
        });
        assert_eq!(
            engine.levels.get(&AudioTarget::Output),
            Some(&level(64.0, false))
        );
        assert_eq!(osd_text(&engine), Some("Output · Headphones · 64%"));
        assert_eq!(
            engine.osd.as_ref().and_then(|osd| osd.level),
            Some((0.64, false))
        );
        // The knob that asked shows where among the outputs it landed.
        let colors = engine.rings[1].colors();
        assert_ne!(colors[1], colors[0]);
        assert_eq!(colors[0], colors[2]);

        // A level that could not be read is not the old output's.
        engine.on_control_report(ControlReport::Output {
            display: "HDMI 1".into(),
            index: 2,
            count: 3,
            level: None,
            from: None,
        });
        assert!(!engine.levels.contains_key(&AudioTarget::Output));
        assert_eq!(osd_text(&engine), Some("Output · HDMI 1"));

        engine.on_control_report(ControlReport::Output {
            display: "Speaker".into(),
            index: 0,
            count: 1,
            level: Some(level(30.0, false)),
            from: Some(1),
        });
        assert_eq!(osd_text(&engine), Some("Only output · Speaker"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_mode_s_colour_still_turns_red_while_what_it_turns_is_muted() {
        use crate::audio::AudioTarget;
        let (mut engine, dir) = engine_with(
            "mode-rest",
            "version = 2\nprofile = \"p\"\n[[encoders]]\nencoder = 0\n\
             modes = [\"deck_brightness\", \"volume\", { preset = \"mic\", ring = \"#00ff00\" }]\n",
        );
        let base = Rgb::new(40, 40, 40);
        engine.on_level(AudioTarget::Output, level(30.0, true));
        engine.on_level(AudioTarget::Input, level(30.0, false));
        // The first mode turns no level: the knob's own colour.
        assert_eq!(engine.ring_rest(0, &engine.encoder_plan(0), base), base);

        // Set as the hold would, without reading any level.
        let stack = engine.encoder_plan(0).modes.expect("the knob has modes");
        let key = (0, stack.layer, stack.entries);
        engine.dial_modes.insert(key.clone(), 1);
        assert_eq!(
            engine.ring_rest(0, &engine.encoder_plan(0), base),
            crate::ring::muted_rest(MODE_RINGS[0])
        );
        // A mode's own colour is tinted the same way.
        engine.dial_modes.insert(key, 2);
        let green = Rgb::new(0, 255, 0);
        assert_eq!(engine.ring_rest(0, &engine.encoder_plan(0), base), green);
        engine.on_level(AudioTarget::Input, level(30.0, true));
        assert_eq!(
            engine.ring_rest(0, &engine.encoder_plan(0), base),
            crate::ring::muted_rest(green)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_muted_ring_rests_red_and_a_live_one_does_not() {
        let base = Rgb::new(40, 40, 40);
        let muted = crate::ring::muted_rest(base);
        assert!(muted.r > muted.g && muted.r > base.r, "{muted:?}");
    }

    fn ms(n: u64) -> Tick {
        Tick(n * 1000)
    }

    fn timer(duration: Option<&str>) -> galdeck_model::Widget {
        galdeck_model::Widget {
            duration: duration.map(String::from),
            ..galdeck_model::Widget::of(galdeck_model::WidgetKind::Timer)
        }
    }

    #[test]
    fn built_ins_bound_where_they_have_nothing_to_act_on_say_so() {
        use galdeck_model::BuiltIn;
        assert!(misplaced(BuiltIn::TimerToggle, From::Key(3)).is_none());
        assert!(misplaced(BuiltIn::TimerReset, From::Knob(0)).is_some());
        assert!(misplaced(BuiltIn::TimerToggle, From::Timer).is_some());
        assert!(misplaced(BuiltIn::PushToTalk, From::Knob(1)).is_some());
        assert!(misplaced(BuiltIn::NextMode, From::Knob(1)).is_none());
        assert!(misplaced(BuiltIn::NextApp, From::Key(0)).is_some());
        assert!(misplaced(BuiltIn::VolumeUp, From::Timer).is_none());
        assert_eq!(
            misplaced(BuiltIn::TimerReset, From::Knob(0)).as_deref(),
            Some("timer_reset works only on a key")
        );
    }

    #[test]
    fn what_a_state_runs_cannot_step_its_key_or_hold_it_down() {
        use galdeck_model::BuiltIn;
        assert!(misplaced(BuiltIn::NextState, From::Key(3)).is_none());
        assert!(misplaced(BuiltIn::PreviousState, From::Key(3)).is_none());
        for action in [
            BuiltIn::NextState,
            BuiltIn::PreviousState,
            BuiltIn::TimerToggle,
            BuiltIn::PushToTalk,
        ] {
            assert_eq!(
                misplaced(action, From::State(3)).as_deref(),
                Some("a state's action cannot change the key's state"),
                "{action:?}"
            );
        }
        assert!(misplaced(BuiltIn::VolumeMute, From::State(3)).is_none());
        assert!(misplaced(BuiltIn::NextPage, From::State(3)).is_none());
        assert!(misplaced(BuiltIn::NextMode, From::State(3)).is_some());
    }

    #[test]
    fn which_looks_for_plain_names_on_the_path_and_runs_nothing() {
        // `sh` is on every PATH a daemon runs with; the rest are not
        // programs' names, or not anyone's.
        let names = [
            "sh",
            "galdeck-no-such-program",
            "/bin/sh",
            "../sh",
            "",
            "..",
        ]
        .map(String::from);
        let Response::Which { found } = which(&names) else {
            panic!("not an answer to which");
        };
        assert_eq!(found, ["sh"]);
        assert!(is_program_name("gnome-session-inhibit"));
        assert!(!is_program_name(&"x".repeat(MAX_PROGRAM_NAME + 1)));
        assert!(!is_program_name("a\0b"));
    }

    #[test]
    fn a_key_past_the_deck_s_last_is_neither_put_in_a_state_nor_drawn_in_one() {
        // It loads, with an error, so an editor can still name it.
        let (mut engine, dir) = engine_with("far-key", "version = 2\nprofile = \"p\"\n");
        std::fs::write(
            dir.join("profiles/p.toml"),
            "[[pages]]\nid = \"one\"\n[[pages.keys]]\nkey = 15\n\
             [[pages.keys.states]]\nname = \"off\"\n[[pages.keys.states]]\nname = \"on\"\n",
        )
        .unwrap();
        assert!(matches!(engine.reload(), Response::Ok));
        for run in [false, true] {
            assert!(matches!(
                engine.set_key_state(15, "on", run),
                Response::Error { .. }
            ));
        }
        assert!(matches!(
            engine.render_key_state(15, Some("on")),
            Response::Error { .. }
        ));
        assert!(engine.key_states.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_key_s_widget_says_what_its_countdown_counts() {
        let stopwatch = galdeck_model::Widget::of(galdeck_model::WidgetKind::Stopwatch);
        assert_eq!(countdown_length(Some(&stopwatch)), Ok(None));
        assert_eq!(
            countdown_length(Some(&timer(Some("4m")))),
            Ok(Some(Duration::from_secs(240)))
        );
        assert_eq!(
            countdown_length(Some(&timer(Some("soon")))),
            Err("Timer has no valid duration")
        );
        assert_eq!(
            countdown_length(Some(&timer(None))),
            Err("Timer has no valid duration")
        );
        let clock = galdeck_model::Widget::of(galdeck_model::WidgetKind::Clock);
        assert_eq!(countdown_length(Some(&clock)), Err("No timer on this key"));
        assert_eq!(countdown_length(None), Err("No timer on this key"));
    }

    #[test]
    fn the_editor_is_told_where_a_countdown_has_got_to() {
        let mut countdown = Countdown::new(Some(Duration::from_secs(60)));
        let info = timer_info(&countdown, ms(0));
        assert_eq!(
            (info.state.as_str(), info.remaining_ms, info.elapsed_ms),
            ("stopped", Some(60_000), 0)
        );
        countdown.toggle(ms(0));
        let info = timer_info(&countdown, ms(1500));
        assert_eq!(
            (info.state.as_str(), info.remaining_ms, info.elapsed_ms),
            ("running", Some(58_500), 1500)
        );
        countdown.toggle(ms(2000));
        assert_eq!(timer_info(&countdown, ms(9000)).state, "paused");
        countdown.toggle(ms(9000));
        assert!(countdown.finish_if_due(ms(70_000)));
        let info = timer_info(&countdown, ms(70_000));
        assert_eq!(
            (info.state.as_str(), info.remaining_ms, info.elapsed_ms),
            ("done", Some(0), 60_000)
        );

        let mut stopwatch = Countdown::new(None);
        stopwatch.toggle(ms(0));
        let info = timer_info(&stopwatch, ms(2500));
        assert_eq!(
            (info.state.as_str(), info.remaining_ms, info.elapsed_ms),
            ("running", None, 2500)
        );
    }

    #[test]
    fn a_finished_timer_is_named_by_its_title_then_its_label_then_its_length() {
        let titled = galdeck_model::Widget {
            title: Some("Pomodoro".into()),
            ..timer(Some("25m"))
        };
        assert_eq!(done_text(&titled, Some("Work")), "Timer done · Pomodoro");
        assert_eq!(
            done_text(&timer(Some("4m")), Some("Tea")),
            "Timer done · Tea"
        );
        assert_eq!(done_text(&timer(Some("4m")), Some("  ")), "Timer done · 4m");
        assert_eq!(done_text(&timer(None), None), "Timer done");
    }

    #[test]
    fn a_finished_timer_flashes_for_ten_seconds_then_rests_tinted() {
        let mut countdown = Countdown::new(Some(Duration::from_secs(1)));
        countdown.toggle(ms(0));
        assert_eq!(alarm_share(&countdown, ms(500)), None, "still counting");
        assert!(countdown.finish_if_due(ms(1000)));
        // Lit, dark, lit: from the clock, whenever it is asked.
        assert_eq!(alarm_share(&countdown, ms(1000)), Some(ALARM_SHARE));
        assert_eq!(alarm_share(&countdown, ms(1499)), Some(ALARM_SHARE));
        assert_eq!(alarm_share(&countdown, ms(1500)), Some(0.0));
        assert_eq!(alarm_share(&countdown, ms(2000)), Some(ALARM_SHARE));
        assert_eq!(alarm_share(&countdown, ms(10_600)), Some(0.0));
        // Past the flash, it stays tinted until it is tapped.
        assert_eq!(alarm_share(&countdown, ms(11_000)), Some(ALARM_SHARE));
        assert_eq!(alarm_share(&countdown, ms(3_600_000)), Some(ALARM_SHARE));
        countdown.toggle(ms(3_600_000));
        assert_eq!(alarm_share(&countdown, ms(3_600_000)), None);
    }

    #[test]
    fn a_countdown_reads_as_its_time_and_what_it_has_left() {
        let mut countdown = Countdown::new(Some(Duration::from_secs(90)));
        countdown.toggle(ms(0));
        assert_eq!(
            countdown_reading(&countdown, ms(30_000)),
            crate::widgets::Reading::Value {
                text: "1:00".into(),
                value: Some(60.0),
            }
        );
        let stopwatch = Countdown::new(None);
        assert_eq!(
            countdown_reading(&stopwatch, ms(0)),
            crate::widgets::Reading::Value {
                text: "0:00".into(),
                value: None,
            }
        );
    }

    #[test]
    fn mute_and_push_to_talk_keys_say_which_device_they_show() {
        use crate::audio::AudioTarget;
        let key = |toml: &str| -> KeyConfig { toml::from_str(toml).unwrap() };
        assert_eq!(
            state_key(&key("key = 0\nexec = { action = \"volume_mute\" }")),
            Some(StateKey::Mute(AudioTarget::Output))
        );
        assert_eq!(
            state_key(&key(
                "key = 0\nexec = { action = \"volume_mute\", target = \"mic\" }"
            )),
            Some(StateKey::Mute(AudioTarget::Input))
        );
        assert_eq!(
            state_key(&key("key = 0\nexec = { action = \"mic_mute\" }")),
            Some(StateKey::Mute(AudioTarget::Input))
        );
        // A volume widget's own tap mutes what it shows.
        assert_eq!(
            state_key(&key("key = 0\nwidget = { kind = \"volume\" }")),
            Some(StateKey::Mute(AudioTarget::Output))
        );
        let talk = state_key(&key("key = 0\nexec = { action = \"push_to_talk\" }"));
        assert_eq!(talk, Some(StateKey::Talk));
        assert_eq!(talk.unwrap().target(), AudioTarget::Input);
        // A hold that mutes is not what the key shows.
        assert_eq!(
            state_key(&key(
                "key = 0\nexec = \"true\"\nhold = { action = \"mic_mute\" }"
            )),
            None
        );
        assert_eq!(state_key(&key("key = 0\nlabel = \"a\"")), None);
    }

    /// The bug this guards against: rotation actions are relative
    /// read-modify-writes, so detents running concurrently all read the
    /// same starting value and collapse into a single step. Eight queued
    /// detents must land as eight increments, not one.
    #[test]
    fn detents_run_one_at_a_time() {
        let path = std::env::temp_dir().join(format!("galdeck-detents-{}", std::process::id()));
        std::fs::write(&path, "0").unwrap();
        let file = path.display();
        let cmd = format!("n=$(cat {file}); echo $((n + 1)) > {file}");

        let runner = RotationRunner::new();
        for _ in 0..8 {
            assert!(runner.push(&cmd, 1), "queue is deeper than eight detents");
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let count = loop {
            let count = std::fs::read_to_string(&path).unwrap_or_default();
            if count.trim() == "8" || std::time::Instant::now() >= deadline {
                break count;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        std::fs::remove_file(&path).ok();
        assert_eq!(
            count.trim(),
            "8",
            "detents raced instead of running in order"
        );
    }

    /// An engine from `engine_with` that paints: a deck connected, a clock
    /// the test moves, and what it sends kept for the test to count.
    fn painting_engine(
        name: &str,
    ) -> (
        Engine,
        Arc<galdeck_core::ManualClock>,
        std::sync::mpsc::Receiver<Paint>,
        PathBuf,
    ) {
        let (mut engine, dir) = engine_with(name, "version = 2\nprofile = \"p\"\n");
        let clock = Arc::new(galdeck_core::ManualClock::starting_at(Tick(1_000_000)));
        engine.clock = clock.clone();
        let (paint_tx, paint_rx) = std::sync::mpsc::sync_channel(64);
        engine.paint_tx = paint_tx;
        engine.device.connected = true;
        (engine, clock, paint_rx, dir)
    }

    fn screens_sent(paints: &std::sync::mpsc::Receiver<Paint>) -> usize {
        paints
            .try_iter()
            .filter(|paint| matches!(paint, Paint::Lcd { .. }))
            .count()
    }

    #[test]
    fn a_knob_turned_fast_sends_the_screen_once_a_frame_and_always_its_last_state() {
        // Each detent of a volume knob redraws the level over the screen, a
        // full frame each. Fifty a second on top of a moving background took
        // the module's firmware off the bus.
        let (mut engine, clock, paints, dir) = painting_engine("screen-pace");
        engine.show_osd("Volume · 40%".into(), Some((0.4, false)));
        engine.paint_screen_when_due();
        assert_eq!(
            screens_sent(&paints),
            1,
            "the first change goes straight out"
        );

        // Detents inside the next 50 ms are held back, all together.
        for percent in [42, 44, 46] {
            clock.advance(Duration::from_millis(10));
            engine.show_osd(format!("Volume · {percent}%"), None);
            engine.paint_screen_when_due();
        }
        assert_eq!(screens_sent(&paints), 0);
        assert!(engine.scheduler.contains_kind(TimerKind::ScreenFrame));

        // When the window ends, the last of them goes out, once.
        clock.advance(Duration::from_millis(20));
        engine.paint_screen_when_due();
        assert_eq!(screens_sent(&paints), 1);
        assert!(!engine.screen_dirty);
        assert_eq!(osd_text(&engine), Some("Volume · 46%"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn over_a_moving_background_the_screen_goes_only_with_its_frames() {
        let (mut engine, clock, paints, dir) = painting_engine("screen-rides");
        let backdrop: galdeck_model::Backdrop =
            toml::from_str("animation = \"aurora\"\nfps = 20\nspan = \"both\"").unwrap();
        let geometry = engine.backdrop_geometry();
        let scene = crate::backdrop::Scene::load(&backdrop, vec![Rgb::BLACK, Rgb::WHITE], geometry)
            .unwrap();
        engine.scene = Some(scene);
        engine.scene_tick = 0;
        engine.screen_dirty = true;
        engine.paint_screen_when_due();
        assert_eq!(screens_sent(&paints), 1);

        // A turn between two of its frames waits for the next, even past the
        // 50 ms a screen without one would wait: a frame of its own would
        // be one more than the background sends, and would put the screen a
        // frame out of step with the keys from then on.
        clock.advance(Duration::from_millis(10));
        engine.show_osd("Volume · 42%".into(), None);
        engine.paint_screen_when_due();
        clock.advance(Duration::from_millis(60));
        engine.paint_screen_when_due();
        assert_eq!(screens_sent(&paints), 0, "no frame of its own");

        // The background's next frame takes it.
        engine.scene_tick += 1;
        engine.paint_screen_when_due();
        assert_eq!(screens_sent(&paints), 1);
        assert!(!engine.screen_dirty);
        let _ = std::fs::remove_dir_all(dir);
    }
}

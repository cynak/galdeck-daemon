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
use galdeck_ipc::{
    AnimationInfo, ConfigFile, ConfigSnapshot, EncoderInfo, KeyInfo, Layout, Patch, Request,
    Response, Status, WidgetInfo,
};

use crate::io::DeviceMsg;
use crate::preview::Preview;
use crate::widgets::{Sample, WidgetHost};

use crate::render;
use crate::ring::RingFeedback;
use galdeck_model::v2::{EncoderConfig, KeyConfig, Page, Profile};
use galdeck_model::{
    config_file_names, Animation, AnimationKind, ConfigDocument, Diagnostics, ResolvedPalette,
    ResolvedStyle, StyleLayer, Workspace,
};

/// Longest the core loop sleeps with nothing scheduled.
///
/// Nothing depends on this for correctness: every real deadline is registered
/// with the scheduler and every message rings the waker. It only bounds how
/// long shutdown takes to notice.
const CORE_IDLE: Duration = Duration::from_millis(250);
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
}

/// A control request paired with its reply channel.
pub struct ControlMsg {
    pub request: Request,
    pub reply: Sender<Response>,
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
    /// The widget runner, and the text each widget last produced.
    widget_host: WidgetHost,
    widget_rx: Receiver<Sample>,
    widget_text: Vec<Option<String>>,
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
    WidgetTick { key: u8 },
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
/// and no encoding at all.
struct RingAnimation {
    animation: Animation,
    base: Rgb,
    to: Rgb,
    interval: Duration,
    next: u8,
}

impl RingAnimation {
    /// The colours for the current frame.
    fn frame(&self) -> [Rgb; galdeck::Ring::SEGMENTS as usize] {
        let count = u32::from(self.animation.frames());
        let phase = f32::from(self.next) / count as f32;
        match self.animation.kind {
            // One lit segment travelling round.
            AnimationKind::Spin => {
                let lit = (phase * galdeck::Ring::SEGMENTS as f32) as usize
                    % galdeck::Ring::SEGMENTS as usize;
                let mut colors = [self.base; galdeck::Ring::SEGMENTS as usize];
                colors[lit] = self.to;
                colors
            }
            // A lit segment with a tail fading out behind it.
            AnimationKind::Comet => {
                let head = (phase * galdeck::Ring::SEGMENTS as f32) as usize
                    % galdeck::Ring::SEGMENTS as usize;
                let mut colors = [self.base; galdeck::Ring::SEGMENTS as usize];
                for behind in 0..galdeck::Ring::SEGMENTS as usize {
                    let at = (head + galdeck::Ring::SEGMENTS as usize - behind)
                        % galdeck::Ring::SEGMENTS as usize;
                    let strength = 1.0 - (behind as f32 / galdeck::Ring::SEGMENTS as f32);
                    colors[at] = self.base.lerp(self.to, strength);
                }
                colors
            }
            kind => {
                let mix = kind.mix_at(phase);
                [self.base.lerp(self.to, mix); galdeck::Ring::SEGMENTS as usize]
            }
        }
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
        } = parts;
        let font = render::load_font(workspace.global.font.as_deref());
        let brightness = workspace.global.brightness;
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
            brightness,
            device: DeviceStatus::default(),
            control_rx,
            device_rx,
            paint_tx,
            deadline,
            clock,
            wake,
            shutdown,
            scheduler: Scheduler::new(),
            preview,
            rotation: Encoders::indices().map(|_| RotationRunner::new()).collect(),
            rings: Encoders::indices().map(|_| RingFeedback::new()).collect(),
            key_animations: (0..Buttons::COUNT).map(|_| None).collect(),
            ring_animations: Encoders::indices().map(|_| None).collect(),
            widget_host,
            widget_rx,
            widget_text: (0..Buttons::COUNT).map(|_| None).collect(),
        };
        engine.enter_profile();
        engine.load_documents();
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
                KeyInfo {
                    key: key.key,
                    index,
                    label: key.label.clone(),
                    text: self.label_for(key),
                    widget: key.widget.as_ref().map(|w| WidgetInfo {
                        kind: format!("{:?}", w.kind).to_lowercase(),
                        interval_ms: w.interval_ms(),
                        format: w.format.clone(),
                        command: w.command.clone(),
                        placeholder: w.placeholder.clone(),
                    }),
                    animation: key.animation.as_ref().map(|a| self.animation_info(a)),
                    icon: key.icon.as_ref().map(|p| p.display().to_string()),
                    exec: key.exec.clone(),
                    page: key.page.clone(),
                    profile: key.profile.clone(),
                    back: key.back,
                    background: hex(style.key_bg),
                    background_is_own: from.key_bg == galdeck_model::StyleSource::Cell,
                }
            })
            .collect();

        let encoders = page
            .encoders
            .iter()
            .enumerate()
            .map(|(index, encoder)| {
                let (style, from) = self.workspace.style_for(
                    &self.theme,
                    &self.palette,
                    profile,
                    page,
                    Some(&encoder.style),
                    "encoder",
                    &mut out,
                );
                EncoderInfo {
                    encoder: encoder.encoder,
                    index,
                    press: encoder.press.clone(),
                    cw: encoder.cw.clone(),
                    ccw: encoder.ccw.clone(),
                    ring: hex(style.ring),
                    ring_is_own: from.ring == galdeck_model::StyleSource::Cell,
                    animation: encoder.animation.as_ref().map(|a| self.animation_info(a)),
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
        })
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
        self.page_index = self
            .workspace
            .profile(&self.profile_id)
            .map(Profile::home_index)
            .unwrap_or(0);
        self.page_stack.clear();
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
            self.service_control();

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
        log::info!("engine stopped");
    }

    fn fire(&mut self, kind: TimerKind) {
        match kind {
            TimerKind::RingRest { encoder } => self.paint_ring(encoder),
            TimerKind::KeyFrame { key } => self.advance_key_animation(key),
            TimerKind::RingFrame { encoder } => self.advance_ring_animation(encoder),
            TimerKind::WidgetTick { key } => self.tick_widget(key),
        }
    }

    /// Sample a widget and schedule its next refresh.
    fn tick_widget(&mut self, key: u8) {
        let Some(widget) = self
            .current_page()
            .and_then(|page| page.keys.iter().find(|k| k.key == key))
            .and_then(|cfg| cfg.widget.clone())
        else {
            return;
        };
        // Blocking kinds answer through the channel instead of returning here.
        if let Some(text) = self.widget_host.sample(key, &widget) {
            self.on_widget_sample(Sample {
                key,
                text: Some(text),
            });
        }
        let now = self.clock.now();
        let interval = Duration::from_millis(u64::from(widget.interval_ms()));
        self.scheduler
            .after(now, interval, TimerKind::WidgetTick { key });
    }

    /// Take a widget's new text and repaint just that key.
    fn on_widget_sample(&mut self, sample: Sample) {
        if sample.key >= Buttons::COUNT {
            return;
        }
        if self.widget_text[sample.key as usize] == sample.text {
            // Nothing moved. A clock showing the same minute must not cost a
            // JPEG encode every second.
            return;
        }
        self.widget_text[sample.key as usize] = sample.text;
        self.repaint_key(sample.key);
    }

    /// Re-render one key, without touching the rest of the page.
    fn repaint_key(&mut self, key: u8) {
        if !self.device.connected {
            return;
        }
        let Some(cfg) = self
            .current_page()
            .and_then(|page| page.keys.iter().find(|k| k.key == key))
            .cloned()
        else {
            return;
        };
        let style = self.style_for(Some(&cfg.style), "key");
        let label = self.label_for(&cfg);
        let canvas = render::key(
            &style,
            cfg.icon.as_deref(),
            label.as_deref(),
            self.font.as_ref(),
        );
        match canvas.to_jpeg(galdeck::ids::DEFAULT_JPEG_QUALITY) {
            Ok(jpeg) => {
                let jpeg: Arc<[u8]> = jpeg.into();
                self.preview.set_key(key, Some(Arc::clone(&jpeg)));
                self.send(Paint::Key {
                    index: key,
                    target: KeyTarget::Jpeg(jpeg),
                });
            }
            Err(e) => log::warn!("encoding key {key} failed: {e}"),
        }
    }

    /// What a key should show: its widget's text if it has produced any, then
    /// the widget's placeholder, then the key's own label.
    fn label_for(&self, cfg: &KeyConfig) -> Option<String> {
        if cfg.widget.is_some() {
            if let Some(text) = self.widget_text[cfg.key as usize].clone() {
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

        self.preview.set_key(key, Some(Arc::clone(&frame)));
        self.send(Paint::Key {
            index: key,
            target: KeyTarget::Jpeg(frame),
        });
        let now = self.clock.now();
        self.scheduler
            .after(now, interval, TimerKind::KeyFrame { key });
    }

    fn advance_ring_animation(&mut self, encoder: u8) {
        let Some(animation) = self.ring_animations[encoder as usize].as_mut() else {
            return;
        };
        let colors = animation.frame();
        let count = animation.animation.frames();
        animation.next = (animation.next + 1) % count;
        let interval = animation.interval;

        // Turn and click feedback wins: an animation must not hide what the
        // knob is doing. The rest colour it returns to is the animation's, so
        // the next frame picks straight back up.
        if !self.rings[encoder as usize].is_active() {
            self.rings[encoder as usize].rest(colors[0]);
            self.preview.set_ring(encoder, colors);
            self.send(Paint::Ring { encoder, colors });
        }
        let now = self.clock.now();
        self.scheduler
            .after(now, interval, TimerKind::RingFrame { encoder });
    }

    /// Schedule the widgets on this page, and forget the ones that left.
    fn start_widgets(&mut self, page: &galdeck_model::v2::Page) {
        for key in Buttons::indices() {
            self.scheduler.cancel_kind(TimerKind::WidgetTick { key });
        }
        // Text from the page we just left would otherwise show on whatever key
        // happens to share its position here.
        for slot in &mut self.widget_text {
            *slot = None;
        }

        let now = self.clock.now();
        for cfg in &page.keys {
            if cfg.widget.is_none() || cfg.key >= Buttons::COUNT {
                continue;
            }
            // Fire immediately rather than after one interval: a clock that
            // takes a second to appear looks broken.
            self.scheduler
                .at(now, TimerKind::WidgetTick { key: cfg.key });
        }
    }

    /// Render and encode every frame of the animations on this page.
    ///
    /// Done once, here, rather than per frame. The cost is real -- eight
    /// frames is eight JPEG encodes -- but it is paid on a page switch instead
    /// of thirty times a second forever.
    fn build_animations(&mut self, page: &galdeck_model::v2::Page) {
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

        for cfg in &page.keys {
            let Some(animation) = &cfg.animation else {
                continue;
            };
            if animation.kind.is_ring_only() || cfg.key >= Buttons::COUNT {
                continue;
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

            let count = animation.frames();
            let mut frames = Vec::with_capacity(count as usize);
            for index in 0..count {
                let mix = animation.mix_for_frame(index);
                let mut frame_style = style;
                frame_style.key_bg = style.key_bg.lerp(to, mix);
                let canvas = render::key(
                    &frame_style,
                    cfg.icon.as_deref(),
                    cfg.label.as_deref(),
                    self.font.as_ref(),
                );
                match canvas.to_jpeg(galdeck::ids::DEFAULT_JPEG_QUALITY) {
                    Ok(jpeg) => frames.push(Arc::from(jpeg)),
                    Err(e) => {
                        log::warn!("encoding a frame for key {} failed: {e}", cfg.key);
                        break;
                    }
                }
            }
            if frames.len() < 2 {
                continue;
            }
            encoded_frames += frames.len();
            let interval = Duration::from_millis(u64::from(animation.frame_interval_ms()));
            self.key_animations[cfg.key as usize] = Some(KeyAnimation {
                frames,
                interval,
                next: 0,
            });
            self.scheduler
                .after(now, interval, TimerKind::KeyFrame { key: cfg.key });
        }

        for cfg in &page.encoders {
            let Some(animation) = &cfg.animation else {
                continue;
            };
            if cfg.encoder >= Encoders::COUNT {
                continue;
            }
            let style = self.style_for(Some(&cfg.style), "encoder");
            let to = animation
                .to
                .as_ref()
                .and_then(|color| {
                    self.palette
                        .resolve(color, "animation.to", &mut Diagnostics::new())
                })
                .unwrap_or(Rgb::WHITE);
            let interval = Duration::from_millis(u64::from(animation.frame_interval_ms()));
            self.ring_animations[cfg.encoder as usize] = Some(RingAnimation {
                animation: animation.clone(),
                base: style.ring,
                to,
                interval,
                next: 0,
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
                self.preview.publish(galdeck_ipc::Event::DeviceDisconnected);
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

    fn encoder_config(&self, encoder: u8) -> Option<&EncoderConfig> {
        self.current_page()?
            .encoders
            .iter()
            .find(|e| e.encoder == encoder)
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
        self.send(Paint::Brightness(self.brightness));
        self.preview.set_brightness(self.brightness);

        // Cloned up front so the borrow of the workspace ends before the
        // paints go out; a page is a dozen small structs.
        let Some(page) = self.current_page().cloned() else {
            return;
        };

        for index in Buttons::indices() {
            let cell = page.keys.iter().find(|k| k.key == index);
            let target = match cell {
                Some(cfg) => {
                    let style = self.style_for(Some(&cfg.style), &format!("keys[{index}].style"));
                    let label = self.label_for(cfg);
                    let canvas = render::key(
                        &style,
                        cfg.icon.as_deref(),
                        label.as_deref(),
                        self.font.as_ref(),
                    );
                    match canvas.to_jpeg(galdeck::ids::DEFAULT_JPEG_QUALITY) {
                        Ok(jpeg) => KeyTarget::Jpeg(jpeg.into()),
                        Err(e) => {
                            log::warn!("encoding key {index} failed: {e}");
                            KeyTarget::Color(style.key_bg)
                        }
                    }
                }
                None => KeyTarget::Blank,
            };
            if let KeyTarget::Jpeg(jpeg) = &target {
                self.preview.set_key(index, Some(Arc::clone(jpeg)));
            } else {
                self.preview.set_key(index, None);
            }
            self.send(Paint::Key { index, target });
        }

        for index in Encoders::indices() {
            let cell = page.encoders.iter().find(|e| e.encoder == index);
            let style = self.style_for(cell.map(|c| &c.style), &format!("encoders[{index}].style"));
            // The page paints the ring, so the feedback state rests there --
            // but it does not get to claim the hardware shows it. Only the
            // mirror, on the far side of a real write, says that.
            self.rings[index as usize].rest(style.ring);
            self.paint_ring(index);
        }

        self.build_animations(&page);
        self.start_widgets(&page);

        let style = self.style_for(None, "lcd.style");
        let text = page.lcd_text.clone().unwrap_or_else(|| page.id.clone());
        let screen = render::lcd(&style, &text, self.font.as_ref());
        match screen.to_jpeg(galdeck::ids::DEFAULT_JPEG_QUALITY) {
            Ok(jpeg) => {
                let jpeg: Arc<[u8]> = jpeg.into();
                self.preview.set_lcd(Arc::clone(&jpeg));
                self.send(Paint::Lcd { jpeg });
            }
            Err(e) => log::warn!("encoding the lcd failed: {e}"),
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
        match event {
            Event::KeyDown(key) => {
                let Some(cfg) = self.key_config(key) else {
                    return;
                };
                let (exec, page, profile, back) = (
                    cfg.exec.clone(),
                    cfg.page.clone(),
                    cfg.profile.clone(),
                    cfg.back,
                );
                self.preview.publish(galdeck_ipc::Event::KeyPressed { key });
                if let Some(cmd) = exec {
                    spawn_action(&cmd);
                }
                // At most one navigation, in the order a key would sensibly
                // declare them.
                if let Some(name) = profile {
                    self.switch_profile(&name);
                } else if let Some(name) = page {
                    self.switch_page(&name);
                } else if back {
                    self.go_back();
                }
            }
            Event::EncoderDown(encoder) => {
                self.preview
                    .publish(galdeck_ipc::Event::EncoderPressed { encoder });
                if let Some(ring) = self.rings.get_mut(encoder as usize) {
                    ring.click(at);
                    self.paint_ring(encoder);
                }
                if let Some(cmd) = self.encoder_config(encoder).and_then(|e| e.press.clone()) {
                    spawn_action(&cmd);
                }
            }
            Event::EncoderRotate(encoder, delta) => {
                // The ring answers every turn, bound or not.
                self.preview
                    .publish(galdeck_ipc::Event::EncoderTurned { encoder, delta });
                if let Some(ring) = self.rings.get_mut(encoder as usize) {
                    ring.turn(delta, at);
                    self.paint_ring(encoder);
                }
                let cfg = self.encoder_config(encoder);
                let cmd = if delta > 0 {
                    cfg.and_then(|e| e.cw.clone())
                } else {
                    cfg.and_then(|e| e.ccw.clone())
                };
                if let (Some(cmd), Some(runner)) = (cmd, self.rotation.get(encoder as usize)) {
                    // Fast turns coalesce into one report with |delta| > 1;
                    // run the command once per detent (capped) so spins
                    // aren't silently dropped. GALDECK_DELTA carries the
                    // signed total for scripts that prefer one scaled step.
                    // Detents queue on this encoder's runner so they run one
                    // at a time rather than racing each other.
                    let detents = (delta.unsigned_abs() as u32).min(MAX_DETENTS_PER_EVENT);
                    for _ in 0..detents {
                        if !runner.push(&cmd, delta) {
                            log::debug!("encoder {encoder} queue full, dropping a detent");
                            break;
                        }
                    }
                }
            }
            _ => {}
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
        self.brightness = workspace.global.brightness;
        self.profile_id = if workspace.profiles.contains_key(&was_profile) {
            was_profile
        } else {
            workspace
                .start_profile()
                .map(str::to_string)
                .unwrap_or_default()
        };
        self.workspace = workspace;
        self.enter_profile();
        self.load_documents();

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
            let response = self.handle_request(msg.request);
            let _ = msg.reply.send(response);
        }
    }

    fn handle_request(&mut self, request: Request) -> Response {
        match request {
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
            }),
            Request::SetBrightness { percent } => {
                if percent > 100 {
                    return Response::Error {
                        message: "brightness must be 0-100".into(),
                    };
                }
                self.brightness = percent;
                // Answered immediately; the io thread applies it when it next
                // drains. Nothing here waits on the device, which is the whole
                // point of the split.
                self.send(Paint::Brightness(percent));
                self.preview.set_brightness(percent);
                self.preview
                    .publish(galdeck_ipc::Event::BrightnessChanged { percent });
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
            Request::GetConfig => Response::Config(self.config_snapshot()),
            Request::GetLayout => match self.layout() {
                Some(layout) => Response::Layout(layout),
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
fn spawn_action(cmd: &str) {
    log::info!("exec: {cmd}");
    match action_command(cmd, None).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || match child.wait() {
                Ok(status) if !status.success() => log::warn!("action exited with {status}"),
                Err(e) => log::warn!("waiting on action: {e}"),
                _ => {}
            });
        }
        Err(e) => log::warn!("spawning action failed: {e}"),
    }
}

/// Run a shell command to completion. Only the rotation workers call this;
/// everything else goes through [`spawn_action`] so the engine keeps
/// polling the device.
fn run_action(cmd: &str, delta: Option<i8>) {
    log::info!("exec: {cmd}");
    match action_command(cmd, delta).status() {
        Ok(status) if !status.success() => log::warn!("action exited with {status}"),
        Err(e) => log::warn!("spawning action failed: {e}"),
        _ => {}
    }
}

/// `sh -c <cmd>`, with the signed rotation delta exported as
/// `GALDECK_DELTA` for scripts that prefer one scaled step per event.
fn action_command(cmd: &str, delta: Option<i8>) -> std::process::Command {
    let mut command = std::process::Command::new("sh");
    command.arg("-c").arg(cmd);
    if let Some(delta) = delta {
        command.env("GALDECK_DELTA", delta.to_string());
    }
    command
}

fn hex(color: galdeck::Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

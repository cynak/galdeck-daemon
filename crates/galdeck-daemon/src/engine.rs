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
use galdeck_ipc::{Request, Response, Status};

use crate::io::DeviceMsg;

use crate::render;
use crate::ring::RingFeedback;
use galdeck_model::{Config, EncoderConfig, KeyConfig, Page};

/// Longest the core loop sleeps with nothing scheduled.
///
/// Nothing depends on this for correctness: every real deadline is registered
/// with the scheduler and every message rings the waker. It only bounds how
/// long shutdown takes to notice.
const CORE_IDLE: Duration = Duration::from_millis(250);
const DEFAULT_KEY_COLOR: Rgb = Rgb::new(24, 26, 32);
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

/// A control request paired with its reply channel.
pub struct ControlMsg {
    pub request: Request,
    pub reply: Sender<Response>,
}

pub struct Engine {
    config_path: PathBuf,
    config: Config,
    font: Option<galdeck::Font>,
    page_index: usize,
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
    /// One serialized action runner per encoder; see [`RotationRunner`].
    rotation: Vec<RotationRunner>,
    /// Ring turn-feedback state, one per encoder.
    rings: Vec<RingFeedback>,
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config_path: PathBuf,
        config: Config,
        control_rx: Receiver<ControlMsg>,
        device_rx: Receiver<DeviceMsg>,
        paint_tx: SyncSender<Paint>,
        deadline: Arc<DeadlineCell>,
        clock: Arc<dyn Clock>,
        wake: WakeReceiver,
        shutdown: Arc<AtomicBool>,
    ) -> Result<Self> {
        let font = render::load_font(config.font.as_deref());
        let brightness = config.brightness;
        Ok(Engine {
            config_path,
            config,
            font,
            page_index: 0,
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
            rotation: Encoders::indices().map(|_| RotationRunner::new()).collect(),
            rings: Encoders::indices().map(|_| RingFeedback::new()).collect(),
        })
    }

    pub fn run(&mut self) {
        log::info!("engine started, config: {}", self.config_path.display());
        while !self.shutdown.load(Ordering::Relaxed) {
            let now = self.clock.now();
            for (_, kind) in self.scheduler.due(now) {
                self.fire(kind);
            }
            while let Ok(msg) = self.device_rx.try_recv() {
                self.on_device(msg);
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
        }
    }

    fn on_device(&mut self, msg: DeviceMsg) {
        match msg {
            DeviceMsg::Connected { firmware, serial } => {
                self.device = DeviceStatus {
                    connected: true,
                    firmware: Some(firmware),
                    serial: Some(serial),
                };
                self.paint_page();
            }
            DeviceMsg::Disconnected => {
                self.device = DeviceStatus::default();
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

    fn current_page(&self) -> &Page {
        &self.config.pages[self.page_index.min(self.config.pages.len() - 1)]
    }

    fn key_config(&self, key: u8) -> Option<&KeyConfig> {
        self.current_page().keys.iter().find(|k| k.key == key)
    }

    fn encoder_config(&self, encoder: u8) -> Option<&EncoderConfig> {
        self.current_page()
            .encoders
            .iter()
            .find(|e| e.encoder == encoder)
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

        let page = self.current_page();
        for index in Buttons::indices() {
            let target = match page.keys.iter().find(|k| k.key == index) {
                Some(cfg) => {
                    let background = cfg
                        .color
                        .as_deref()
                        .and_then(Rgb::from_hex)
                        .unwrap_or(DEFAULT_KEY_COLOR);
                    let canvas = render::key(
                        background,
                        cfg.image.as_deref(),
                        cfg.label.as_deref(),
                        self.font.as_ref(),
                    );
                    match canvas.to_jpeg(galdeck::ids::DEFAULT_JPEG_QUALITY) {
                        Ok(jpeg) => KeyTarget::Jpeg(jpeg.into()),
                        Err(e) => {
                            log::warn!("encoding key {index} failed: {e}");
                            KeyTarget::Color(background)
                        }
                    }
                }
                None => KeyTarget::Blank,
            };
            self.send(Paint::Key { index, target });
        }

        let ring_colors: Vec<Rgb> = Encoders::indices()
            .map(|index| {
                self.current_page()
                    .encoders
                    .iter()
                    .find(|e| e.encoder == index)
                    .and_then(|cfg| cfg.ring.as_deref())
                    .and_then(Rgb::from_hex)
                    .unwrap_or(Rgb::BLACK)
            })
            .collect();
        for (index, color) in Encoders::indices().zip(ring_colors) {
            // The page paints the ring, so the feedback state rests there --
            // but it does not get to claim the hardware shows it. Only the
            // mirror, on the far side of a real write, says that.
            self.rings[index as usize].rest(color);
            self.paint_ring(index);
        }

        let page = self.current_page();
        let text = page
            .lcd_text
            .as_deref()
            .or(self.config.lcd_text.as_deref())
            .unwrap_or(&page.name)
            .to_string();
        let screen = render::lcd(&text, self.font.as_ref());
        match screen.to_jpeg(galdeck::ids::DEFAULT_JPEG_QUALITY) {
            Ok(jpeg) => self.send(Paint::Lcd { jpeg: jpeg.into() }),
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
                let (exec, page_target) = match self.key_config(key) {
                    Some(cfg) => (cfg.exec.clone(), cfg.page.clone()),
                    None => (None, None),
                };
                if let Some(cmd) = exec {
                    spawn_action(&cmd);
                }
                if let Some(name) = page_target {
                    self.switch_page(&name);
                }
            }
            Event::EncoderDown(encoder) => {
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

    fn switch_page(&mut self, name: &str) -> bool {
        match self.config.pages.iter().position(|p| p.name == name) {
            Some(index) => {
                self.page_index = index;
                self.paint_page();
                true
            }
            None => {
                log::warn!("unknown page {name:?}");
                false
            }
        }
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
                page: self.current_page().name.clone(),
                pages: self.config.pages.iter().map(|p| p.name.clone()).collect(),
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
            Request::Reload => match Config::load(&self.config_path) {
                Ok(config) => {
                    self.font = render::load_font(config.font.as_deref());
                    self.brightness = config.brightness;
                    let current = self.current_page().name.clone();
                    self.page_index = config
                        .pages
                        .iter()
                        .position(|p| p.name == current)
                        .unwrap_or(0);
                    self.config = config;
                    self.paint_page();
                    Response::Ok
                }
                Err(e) => Response::Error {
                    message: format!("{e:#}"),
                },
            },
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

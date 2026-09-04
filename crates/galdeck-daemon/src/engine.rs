//! The device engine: owns the HID handle on one thread, applies pages,
//! dispatches input events to actions, and services control requests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use galdeck::{Buttons, Encoders, Event, Galleon, Rgb};
use galdeck_ipc::{Request, Response, Status};

use crate::config::{Config, EncoderConfig, KeyConfig, Page};
use crate::render;
use crate::ring::RingFeedback;

const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);
/// How long one poll waits for input when the device is idle.
const POLL_TIMEOUT: Duration = Duration::from_millis(200);
/// Shorter poll while a ring is animating, so it returns to rest on time
/// instead of waiting for the next input event.
const RING_FRAME: Duration = Duration::from_millis(40);
const DEFAULT_KEY_COLOR: Rgb = Rgb::new(24, 26, 32);
/// Cap on commands spawned for one coalesced rotation report.
const MAX_DETENTS_PER_EVENT: u32 = 8;
/// Depth of one encoder's rotation queue. Deep enough to absorb a fast
/// spin, shallow enough that a slow action cannot build a backlog the
/// knob keeps paying off after the user has stopped turning.
const ROTATION_QUEUE_DEPTH: usize = 16;

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
    device: Option<DeviceState>,
    last_connect_attempt: Option<Instant>,
    control_rx: Receiver<ControlMsg>,
    shutdown: Arc<AtomicBool>,
    /// One serialized action runner per encoder; see [`RotationRunner`].
    rotation: Vec<RotationRunner>,
    /// Ring turn-feedback state, one per encoder.
    rings: Vec<RingFeedback>,
}

struct DeviceState {
    deck: Galleon,
    firmware: String,
    serial: String,
}

impl Engine {
    pub fn new(
        config_path: PathBuf,
        config: Config,
        control_rx: Receiver<ControlMsg>,
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
            device: None,
            last_connect_attempt: None,
            control_rx,
            shutdown,
            rotation: Encoders::indices().map(|_| RotationRunner::new()).collect(),
            rings: Encoders::indices().map(|_| RingFeedback::new()).collect(),
        })
    }

    pub fn run(&mut self) {
        log::info!("engine started, config: {}", self.config_path.display());
        while !self.shutdown.load(Ordering::Relaxed) {
            self.service_control();

            if self.device.is_none() {
                self.maybe_connect();
                if self.device.is_none() {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            }

            let timeout = if self.rings.iter().any(|ring| ring.is_active()) {
                RING_FRAME
            } else {
                POLL_TIMEOUT
            };
            let state = self.device.as_mut().unwrap();
            match state.deck.poll(timeout) {
                Ok(events) => {
                    // A keepalive gap (suspend, long stall) means the module
                    // re-entered software mode and the firmware wiped our
                    // state — redraw the page.
                    if state.deck.take_mode_reentry() {
                        log::info!("module re-entered software mode, re-applying page");
                        self.apply_page();
                    }
                    for event in events {
                        self.handle_event(event);
                    }
                    self.tick_rings();
                }
                Err(e) => {
                    log::warn!("device error, will reconnect: {e}");
                    self.device = None;
                }
            }
        }

        // Leave the module tidy: blank everything and hand it back to
        // hardware mode via the logo screen.
        if let Some(state) = self.device.as_mut() {
            let _ = state.deck.clear_all();
            let _ = state.deck.reset_to_logo();
        }
        log::info!("engine stopped");
    }

    fn maybe_connect(&mut self) {
        if let Some(last) = self.last_connect_attempt {
            if last.elapsed() < RECONNECT_INTERVAL {
                return;
            }
        }
        self.last_connect_attempt = Some(Instant::now());

        let api = match galdeck::hidapi::HidApi::new() {
            Ok(api) => api,
            Err(e) => {
                log::warn!("hidapi init failed: {e}");
                return;
            }
        };
        match Galleon::open(&api) {
            Ok(mut deck) => {
                let firmware = deck.firmware_version().unwrap_or_else(|_| "unknown".into());
                let serial = deck.serial_number().unwrap_or_else(|_| "unknown".into());
                log::info!("connected: firmware {firmware}, serial {serial}");
                if !galdeck::ids::VALIDATED_FIRMWARES.contains(&firmware.as_str()) {
                    log::warn!(
                        "firmware {firmware} differs from the validated versions {:?} — if the module drops out of software mode, the keepalive may have changed on this firmware; please report it",
                        galdeck::ids::VALIDATED_FIRMWARES
                    );
                }
                self.device = Some(DeviceState {
                    deck,
                    firmware,
                    serial,
                });
                self.apply_page();
            }
            Err(galdeck::Error::DeviceNotFound) => {
                log::debug!("device not present, retrying");
            }
            Err(e) => {
                log::warn!("open failed: {e}");
            }
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
    fn apply_page(&mut self) {
        let Some(state) = self.device.as_mut() else {
            return;
        };
        let page = &self.config.pages[self.page_index.min(self.config.pages.len() - 1)];
        log::info!("applying page {:?}", page.name);

        let ring_colors: Vec<Rgb> = Encoders::indices()
            .map(|index| {
                page.encoders
                    .iter()
                    .find(|e| e.encoder == index)
                    .and_then(|e| e.ring.as_deref())
                    .and_then(Rgb::from_hex)
                    .unwrap_or(Rgb::BLACK)
            })
            .collect();

        let result: std::result::Result<(), galdeck::Error> = (|| {
            state.deck.set_brightness(self.brightness)?;

            for index in Buttons::indices() {
                match page.keys.iter().find(|k| k.key == index) {
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
                        state.deck.button(index)?.draw(&canvas)?;
                    }
                    None => state.deck.button(index)?.clear()?,
                }
            }

            for (index, color) in Encoders::indices().zip(ring_colors.iter().copied()) {
                state.deck.encoder(index)?.ring().set_all(color)?;
            }

            let text = page
                .lcd_text
                .as_deref()
                .or(self.config.lcd_text.as_deref())
                .unwrap_or(&page.name);
            let screen = render::lcd(text, self.font.as_ref());
            state.deck.lcd().draw(&screen)?;
            Ok(())
        })();

        match result {
            // The rings now show these colours, and rest back to them
            // after every turn.
            Ok(()) => {
                for (ring, color) in self.rings.iter_mut().zip(ring_colors) {
                    ring.rest(color);
                }
            }
            Err(e) => {
                log::warn!("applying page failed, will reconnect: {e}");
                self.device = None;
            }
        }
    }

    /// Push any ring repaints the feedback state machine is waiting on.
    fn tick_rings(&mut self) {
        let Some(state) = self.device.as_mut() else {
            return;
        };
        let now = Instant::now();
        let mut failure = None;
        'rings: for (index, ring) in self.rings.iter_mut().enumerate() {
            for (segment, color) in ring.updates(now) {
                let write = state
                    .deck
                    .encoder(index as u8)
                    .and_then(|mut encoder| encoder.ring().set_segment(segment, color));
                if let Err(e) = write {
                    failure = Some(e);
                    break 'rings;
                }
            }
        }
        if let Some(e) = failure {
            log::warn!("ring feedback failed, will reconnect: {e}");
            self.device = None;
        }
    }

    fn handle_event(&mut self, event: Event) {
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
                    ring.click(Instant::now());
                }
                if let Some(cmd) = self.encoder_config(encoder).and_then(|e| e.press.clone()) {
                    spawn_action(&cmd);
                }
            }
            Event::EncoderRotate(encoder, delta) => {
                // The ring answers every turn, bound or not.
                if let Some(ring) = self.rings.get_mut(encoder as usize) {
                    ring.turn(delta, Instant::now());
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
                self.apply_page();
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
                connected: self.device.is_some(),
                firmware: self.device.as_ref().map(|d| d.firmware.clone()),
                serial: self.device.as_ref().map(|d| d.serial.clone()),
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
                if let Some(state) = self.device.as_mut() {
                    if let Err(e) = state.deck.set_brightness(percent) {
                        self.device = None;
                        return Response::Error {
                            message: e.to_string(),
                        };
                    }
                }
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
                    self.apply_page();
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

        let deadline = Instant::now() + Duration::from_secs(10);
        let count = loop {
            let count = std::fs::read_to_string(&path).unwrap_or_default();
            if count.trim() == "8" || Instant::now() >= deadline {
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

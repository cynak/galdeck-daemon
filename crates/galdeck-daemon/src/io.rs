//! The io pump: the only thread that touches the device.
//!
//! Its whole job is to write what it is told, within a wall-clock budget, and
//! to keep polling. It holds no configuration, renders nothing, and makes no
//! decisions beyond "is this worth writing" — which it answers from the
//! [`DeckShadow`].
//!
//! It is a separate thread from the one that owns the daemon's state for one
//! decisive reason: opening the device blocks about 1.2 seconds, and a
//! keepalive sent after a gap of two seconds blocks another full second. With
//! the control mailbox on this thread, every reconnect attempt while the
//! keyboard is unplugged would freeze the API — which is exactly the state
//! someone developing against `--device virtual` is in.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use galdeck_core::{Clock, DeadlineCell, Tick, Waker};
use galdeck_device::{Deck, DeckOp, DeckShadow, FakeDeck, FakeDeckHandle, HardwareDeck, Paint};

use crate::engine::DeviceMode;

/// Poll timeout when nothing is scheduled and nothing is moving.
///
/// Twenty `read_timeout` syscalls a second is negligible, and this is the
/// entire latency uncertainty for a push nobody scheduled. It cannot be
/// replaced by waiting on a file descriptor: `Galleon` exposes none, every
/// method takes `&mut self`, and `poll` blocks — so no reactor, async or
/// otherwise, could do better here.
const IDLE_POLL: Duration = Duration::from_millis(50);
/// Poll timeout while something is happening. Armed by any input event or any
/// executed write, so 200 syscalls a second only while the deck is in use.
const ACTIVE_POLL: Duration = Duration::from_millis(5);
/// How long an event or a write keeps the loop in its attentive mode.
const ACTIVE_WINDOW: Duration = Duration::from_millis(250);
/// Never poll for less than this, or a stale deadline becomes a spin.
const MIN_POLL: Duration = Duration::from_millis(1);
/// Wall-clock ceiling on one batch of writes.
///
/// Charged with `DeckOp::cost()`, and checked *before* each op rather than
/// after, so a 2 ms feature report cannot overshoot the budget by its whole
/// duration. Twelve milliseconds keeps input latency imperceptible even when
/// a full ring frame lands in the same pass.
const WRITE_BUDGET: Duration = Duration::from_millis(12);
/// How often to retry opening the device.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);
/// A batch slower than this says something is wrong with the cost model.
const SLOW_BATCH_WARN: Duration = Duration::from_millis(100);
/// How long to sleep when there is nothing to poll: no device present, or the
/// device handed to another process. Long enough not to spin, short enough
/// that resuming feels immediate.
const IDLE_SLEEP: Duration = Duration::from_millis(100);

/// What the io thread tells the rest of the daemon.
#[derive(Debug)]
pub enum DeviceMsg {
    Connected {
        firmware: String,
        serial: String,
    },
    Disconnected,
    /// The module re-entered software mode and wiped firmware-asserted state.
    /// Everything must be repainted.
    ModeReentry,
    Input(galdeck::Event, Tick),
}

pub struct IoThread {
    deck: Option<Box<dyn Deck>>,
    shadow: DeckShadow,
    /// Ops that have been planned but not yet written, because the write
    /// budget ran out mid-batch. They are resumed on the next pass; dropping
    /// them would silently lose the paint that produced them.
    pending: VecDeque<DeckOp>,
    mode: DeviceMode,
    paints: Receiver<Paint>,
    events: SyncSender<DeviceMsg>,
    deadline: Arc<DeadlineCell>,
    clock: Arc<dyn Clock>,
    waker: Waker,
    shutdown: Arc<AtomicBool>,
    /// Set by the engine while another process holds the device.
    ///
    /// An atomic rather than a message because it has to be readable from
    /// inside this loop without draining a mailbox, and because the engine
    /// sets it from a request it is about to leave unanswered.
    parked: Arc<AtomicBool>,
    active_until: Tick,
    last_connect_attempt: Option<Tick>,
    /// Kept so a virtual deck's surface stays reachable for the preview.
    virtual_handle: Option<FakeDeckHandle>,
}

impl IoThread {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mode: DeviceMode,
        paints: Receiver<Paint>,
        events: SyncSender<DeviceMsg>,
        deadline: Arc<DeadlineCell>,
        clock: Arc<dyn Clock>,
        waker: Waker,
        shutdown: Arc<AtomicBool>,
        parked: Arc<AtomicBool>,
    ) -> Self {
        Self {
            deck: None,
            shadow: DeckShadow::new(),
            pending: VecDeque::new(),
            mode,
            paints,
            events,
            deadline,
            clock,
            waker,
            shutdown,
            parked,
            active_until: Tick::ZERO,
            last_connect_attempt: None,
            virtual_handle: None,
        }
    }

    /// Start with a deck that is already open.
    ///
    /// The test seam: it lets a harness hold the fake's handle and watch
    /// exactly what reaches the panel.
    #[allow(clippy::too_many_arguments)]
    pub fn with_deck(
        deck: Box<dyn Deck>,
        paints: Receiver<Paint>,
        events: SyncSender<DeviceMsg>,
        deadline: Arc<DeadlineCell>,
        clock: Arc<dyn Clock>,
        waker: Waker,
        shutdown: Arc<AtomicBool>,
        parked: Arc<AtomicBool>,
    ) -> Self {
        let mut io = Self::new(
            DeviceMode::Virtual,
            paints,
            events,
            deadline,
            clock,
            waker,
            shutdown,
            parked,
        );
        io.send(DeviceMsg::Connected {
            firmware: deck.firmware().to_string(),
            serial: deck.serial().to_string(),
        });
        io.deck = Some(deck);
        io
    }

    pub fn virtual_handle(&self) -> Option<FakeDeckHandle> {
        self.virtual_handle.clone()
    }

    pub fn run(mut self) {
        while !self.shutdown.load(Ordering::Relaxed) {
            // Handed to another process. Closing the handle is the easy half;
            // the important half is the `continue`, because a parked loop
            // that still reached try_connect would reopen the hidraw node
            // underneath whoever just took it -- which is the exact failure
            // the handover exists to prevent.
            if self.parked.load(Ordering::Relaxed) {
                if self.deck.is_some() {
                    // Sends Disconnected, which is what the engine is waiting
                    // for before it answers the release.
                    self.drop_device();
                }
                self.discard_paints();
                std::thread::sleep(IDLE_SLEEP);
                continue;
            }

            if self.deck.is_none() {
                self.try_connect();
                if self.deck.is_none() {
                    // Nothing to poll, so this is the one place the loop may
                    // wait without the device: drain paints so the channel
                    // cannot back up, and sleep briefly.
                    self.discard_paints();
                    std::thread::sleep(IDLE_SLEEP);
                    continue;
                }
            }

            let spent = self.flush();
            if spent > SLOW_BATCH_WARN {
                log::warn!("slow io batch: {spent:?}");
            }
            self.poll_once();
        }

        // Leave the module tidy: blank everything and hand it back to hardware
        // mode via the logo screen.
        if let Some(deck) = self.deck.as_mut() {
            let _ = deck.clear_all();
            let _ = deck.reset_to_logo();
        }
    }

    fn try_connect(&mut self) {
        let now = self.clock.now();
        if let Some(last) = self.last_connect_attempt {
            if now.duration_since(last) < RECONNECT_INTERVAL {
                return;
            }
        }
        self.last_connect_attempt = Some(now);

        let opened: Option<Box<dyn Deck>> = match self.mode {
            DeviceMode::Virtual => {
                let (deck, handle) = FakeDeck::new();
                self.virtual_handle = Some(handle);
                Some(Box::new(deck))
            }
            DeviceMode::Auto => match galdeck::hidapi::HidApi::new() {
                Ok(api) => match HardwareDeck::open(&api) {
                    Ok(deck) => {
                        if !deck.firmware_is_validated() {
                            log::warn!(
                                "firmware {} differs from the validated versions {:?} — if the module drops out of software mode, the keepalive may have changed on this firmware; please report it",
                                deck.firmware(),
                                galdeck::ids::VALIDATED_FIRMWARES
                            );
                        }
                        Some(Box::new(deck) as Box<dyn Deck>)
                    }
                    Err(galdeck::Error::DeviceNotFound) => {
                        log::debug!("device not present, retrying");
                        None
                    }
                    Err(e) => {
                        log::warn!("open failed: {e}");
                        None
                    }
                },
                Err(e) => {
                    log::warn!("hidapi init failed: {e}");
                    None
                }
            },
        };

        if let Some(deck) = opened {
            log::info!(
                "connected: firmware {}, serial {}",
                deck.firmware(),
                deck.serial()
            );
            let msg = DeviceMsg::Connected {
                firmware: deck.firmware().to_string(),
                serial: deck.serial().to_string(),
            };
            // The mirror describes a device that is no longer there.
            self.shadow.forget_everything();
            self.deck = Some(deck);
            self.send(msg);
        }
    }

    /// Write as much of the pending paint as the budget allows.
    ///
    /// Returns what it spent. Whatever does not fit stays queued and is picked
    /// up on the next pass, after a poll -- which is what stops a big repaint
    /// from starving input without losing any of it.
    fn flush(&mut self) -> Duration {
        let mut spent = Duration::ZERO;
        loop {
            if self.pending.is_empty() && !self.refill() {
                break;
            }
            let Some(op) = self.pending.front() else {
                break;
            };
            // Checked before writing, so a 2 ms feature report cannot
            // overshoot the budget by its whole duration. At least one op
            // always goes through, or a batch of expensive ops would never
            // make progress.
            if !spent.is_zero() && spent + op.cost() > WRITE_BUDGET {
                break;
            }
            let op = self.pending.pop_front().expect("front was just checked");
            let Some(deck) = self.deck.as_mut() else {
                break;
            };
            match op.apply(deck.as_mut()) {
                Ok(()) => {
                    self.shadow.record(&op);
                    spent += op.cost();
                }
                Err(e) if e.is_disconnected() => {
                    log::warn!("write failed, will reconnect: {e}");
                    self.drop_device();
                    break;
                }
                Err(e) => log::warn!("write rejected: {e}"),
            }
        }
        if !spent.is_zero() {
            self.active_until = self.clock.now().saturating_add(ACTIVE_WINDOW);
        }
        spent
    }

    /// Take one paint from the channel and plan it. Returns false when there
    /// is nothing waiting.
    fn refill(&mut self) -> bool {
        loop {
            let paint = match self.paints.try_recv() {
                Ok(paint) => paint,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return false,
            };
            if matches!(paint, Paint::Forget) {
                self.shadow.forget_everything();
                // Anything already planned described the old belief.
                self.pending.clear();
                continue;
            }
            let ops = self.shadow.plan(&paint);
            if !ops.is_empty() {
                self.pending.extend(ops);
                return true;
            }
        }
    }

    fn poll_once(&mut self) {
        let now = self.clock.now();
        let timeout = if now < self.active_until {
            ACTIVE_POLL
        } else {
            self.deadline
                .remaining(now)
                .unwrap_or(IDLE_POLL)
                .clamp(MIN_POLL, IDLE_POLL)
        };

        let Some(deck) = self.deck.as_mut() else {
            return;
        };
        match deck.poll(timeout) {
            Ok(events) => {
                if deck.take_mode_reentry() {
                    log::info!("module re-entered software mode");
                    self.shadow.forget_everything();
                    self.send(DeviceMsg::ModeReentry);
                }
                if events.is_empty() {
                    return;
                }
                let at = self.clock.now();
                self.active_until = at.saturating_add(ACTIVE_WINDOW);
                for event in events {
                    self.send(DeviceMsg::Input(event, at));
                }
                self.waker.notify();
            }
            Err(e) if e.is_disconnected() => {
                log::warn!("device error, will reconnect: {e}");
                self.drop_device();
            }
            Err(e) => log::debug!("device hiccup, continuing: {e}"),
        }
    }

    fn drop_device(&mut self) {
        self.deck = None;
        self.pending.clear();
        self.virtual_handle = None;
        self.shadow.forget_everything();
        self.send(DeviceMsg::Disconnected);
    }

    /// Throw away paint that arrived while there is nothing to paint on.
    fn discard_paints(&mut self) {
        self.pending.clear();
        while self.paints.try_recv().is_ok() {}
    }

    fn send(&self, msg: DeviceMsg) {
        if self.events.send(msg).is_ok() {
            self.waker.notify();
        }
    }
}

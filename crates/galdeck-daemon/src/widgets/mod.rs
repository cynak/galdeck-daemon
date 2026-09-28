//! Sampling widgets.
//!
//! Two classes, and the difference matters more than anything else here.
//! A clock read or a few bytes from `/proc` takes microseconds and happens on
//! the thread that owns the daemon's state. A shell command, a web request or
//! a D-Bus call takes as long as someone else decides it takes, so it happens
//! on a worker and comes back through a channel — otherwise one slow widget
//! stalls every key on the deck, and the device stops being polled.
//!
//! There is a worker per kind of slowness rather than one for everything: a
//! weather request timing out must not leave the media widget's progress bar
//! frozen behind it.

pub mod command;
pub mod demo;
pub mod draw;
pub mod media;
pub mod nixie;
pub mod sources;
pub mod system;
pub mod weather;

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};

use galdeck_core::Waker;
use galdeck_model::{Units, Widget, WidgetKind};

use media::{MediaClient, Poll};
use system::{CpuSampler, NetworkSampler, TemperatureSampler};
use weather::WeatherClient;

/// How many jobs may wait for one worker.
///
/// A widget that cannot keep up with its own interval should drop refreshes
/// rather than build a backlog it pays off after the user has moved on.
const QUEUE_DEPTH: usize = 8;
/// How many finished samples may wait for the engine.
const SAMPLE_DEPTH: usize = 32;

/// Where a widget is shown.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum Slot {
    /// A key, by index.
    Key(u8),
    /// A tile on the info screen, by its index in the page's `lcd` list.
    Tile(u8),
}

/// What a widget produced.
#[derive(Clone, Debug, PartialEq)]
pub enum Reading {
    /// Text to show, and the number behind it when there is one.
    Value {
        text: String,
        value: Option<f64>,
    },
    Weather(weather::Weather),
    /// `None` when no player is running, which is an answer rather than a
    /// failure and draws as one.
    Media(Option<media::Media>),
}

impl Reading {
    fn text(text: impl Into<String>) -> Self {
        Reading::Value {
            text: text.into(),
            value: None,
        }
    }

    fn number(value: f64, text: String) -> Self {
        Reading::Value {
            text,
            value: Some(value),
        }
    }

    /// The text a label would show, for widgets drawn as one.
    pub fn label(&self) -> Option<String> {
        match self {
            Reading::Value { text, .. } => Some(text.clone()),
            Reading::Weather(w) => Some(weather::Weather::degrees(w.temperature)),
            Reading::Media(Some(m)) => m.title.clone(),
            Reading::Media(None) => None,
        }
    }

    fn value(&self) -> Option<f64> {
        match self {
            Reading::Value { value, .. } => *value,
            _ => None,
        }
    }
}

/// A widget's latest reading, and the ones before it for a graph.
#[derive(Clone, Debug, Default)]
pub struct SlotState {
    pub reading: Option<Reading>,
    pub history: VecDeque<f64>,
    /// The largest value seen, for scaling a graph with no fixed top.
    pub peak: f64,
    /// How alarming the latest reading is against the widget's thresholds.
    pub level: galdeck_model::Level,
    /// When the widget's data was last read. An animated view is drawn more
    /// often than it is read: see `Widget::sample_interval_ms`.
    pub sampled_at: Option<galdeck_core::Tick>,
}

impl SlotState {
    /// Take a new reading. Returns whether anything visible changed.
    pub fn update(&mut self, reading: Option<Reading>, widget: &Widget) -> bool {
        let mut changed = false;
        if let Some(value) = reading.as_ref().and_then(Reading::value) {
            if widget.is_graphic() {
                // Every sample moves a graph along, even an unchanged one.
                self.history.push_back(value);
                while self.history.len() > widget.history() {
                    self.history.pop_front();
                }
                changed = true;
            }
        }
        self.correct(reading, widget) || changed
    }

    /// Take a reading that did not come from the widget's own sampling -- a
    /// sound server's report of a change or a poll -- without moving a graph
    /// along. A graph's history is one point per interval; points from
    /// anywhere else would squeeze its time axis.
    pub fn correct(&mut self, reading: Option<Reading>, widget: &Widget) -> bool {
        let mut changed = false;
        if let Some(value) = reading.as_ref().and_then(Reading::value) {
            // Kept for a label too: it is the scale a threshold's margin is
            // measured against when the widget has no fixed top.
            self.peak = self.peak.max(value);
        }
        // An animated view has a new frame to show on every refresh.
        if widget.view().is_animated() {
            changed = true;
        }
        if self.reading != reading {
            self.reading = reading;
            changed = true;
        }
        // Worked out after the reading is stored, since the scale may have
        // moved with it; a change of level is a change worth a repaint.
        if let Some(value) = self.reading.as_ref().and_then(Reading::value) {
            if widget.warn.is_some() || widget.critical.is_some() {
                let level = widget.level(value, self.level, self.scale(widget));
                if level != self.level {
                    self.level = level;
                    changed = true;
                }
            }
        }
        changed
    }

    /// Where a graph or bar is full.
    pub fn scale(&self, widget: &Widget) -> f64 {
        widget.fixed_max().unwrap_or(self.peak).max(f64::EPSILON)
    }
}

/// A widget's reading, on its way back to the engine.
#[derive(Debug, Clone)]
pub struct Sample {
    pub slot: Slot,
    /// Which page it was asked for. A sample that arrives after a page
    /// switch belongs to a slot that now shows something else.
    pub generation: u64,
    /// `None` when the widget failed; the caller falls back to the label.
    pub reading: Option<Reading>,
}

struct Job {
    slot: Slot,
    generation: u64,
    widget: Widget,
}

/// The state a sampler keeps between readings, where it needs any.
enum Stateful {
    Cpu(CpuSampler),
    Temperature(TemperatureSampler),
    Network(NetworkSampler),
    Fan(system::FanSampler),
}

/// A volume reading, as the widget shows one: the same whether it came from
/// sampling or from a knob that just changed it.
pub fn volume_reading(level: f64, muted: bool) -> Reading {
    // Muted reads as nothing on a graph or a dial, which is what it sounds
    // like.
    let text = if muted {
        "muted".into()
    } else {
        percent(level)
    };
    Reading::number(if muted { 0.0 } else { level }, text)
}

/// Runs the widgets that cannot be sampled inline.
pub struct WidgetHost {
    /// Kept so the engine can give other workers a way to wake it.
    waker: Waker,
    commands: SyncSender<Job>,
    network: SyncSender<Job>,
    media: SyncSender<Job>,
    /// Per slot, because a CPU reading is a difference between two samples:
    /// a key and a tile sharing one sampler would each measure the time since
    /// the other last looked.
    samplers: HashMap<Slot, Stateful>,
}

impl WidgetHost {
    pub fn new(waker: Waker) -> (Self, Receiver<Sample>) {
        let waker_for_engine = waker.clone();
        let (samples_tx, samples_rx) = sync_channel::<Sample>(SAMPLE_DEPTH);

        let commands = spawn_worker("galdeck-widgets", samples_tx.clone(), waker.clone(), {
            move |widget: &Widget| match widget.kind {
                WidgetKind::Gpu => Some(
                    system::gpu_busy(widget.source.as_deref())
                        .map(|busy| Reading::number(busy, percent(busy))),
                ),
                WidgetKind::Volume => Some(
                    system::volume(widget.source.as_deref())
                        .map(|(level, muted)| volume_reading(level, muted)),
                ),
                _ => {
                    let line = command::run(widget.command.as_deref()?);
                    Some(line.map(|line| Reading::Value {
                        value: command::leading_number(&line),
                        text: line,
                    }))
                }
            }
        });

        let network = spawn_worker("galdeck-weather", samples_tx.clone(), waker.clone(), {
            let client = WeatherClient::default();
            move |widget: &Widget| {
                let weather = client.fetch(widget.latitude?, widget.longitude?, widget.units())?;
                // A failed refresh sends nothing, so the last forecast stays
                // up: fifteen-minute-old weather beats a blank tile.
                Some(Some(Reading::Weather(weather)))
            }
        });

        let media = spawn_worker("galdeck-media", samples_tx, waker, {
            let mut client = MediaClient::default();
            move |widget: &Widget| match client.poll(widget.source.as_deref()) {
                Poll::Playing(media) => Some(Some(Reading::Media(Some(media)))),
                Poll::Nothing => Some(Some(Reading::Media(None))),
                Poll::Failed => Some(None),
            }
        });

        (
            Self {
                waker: waker_for_engine,
                commands,
                network,
                media,
                samplers: HashMap::new(),
            },
            samples_rx,
        )
    }

    /// Sample a widget.
    ///
    /// Returns `Some` for the cheap kinds, which are answered here and now.
    /// `None` means the answer is coming through the channel later — or that
    /// there is no answer.
    pub fn sample(&mut self, slot: Slot, generation: u64, widget: &Widget) -> Option<Reading> {
        let source = widget.source.as_deref();
        match widget.kind {
            WidgetKind::Clock | WidgetKind::Date => {
                let now = now_in(widget.timezone());
                let text = now.strftime(widget.format()).to_string();
                // A clock face needs the time as a number, and a reading that
                // changes every second so its second hand moves; a clock
                // drawn as text does not, and would repaint needlessly.
                if widget.view() == galdeck_model::WidgetView::Analog {
                    let time = now.time();
                    let seconds = f64::from(time.hour()) * 3600.0
                        + f64::from(time.minute()) * 60.0
                        + f64::from(time.second());
                    Some(Reading::number(seconds, text))
                } else {
                    Some(Reading::text(text))
                }
            }
            WidgetKind::Battery => system::battery(source).map(|(charge, charging)| {
                // An arrow for charging, as the network widget uses for
                // direction: it needs no glyph a font might lack.
                let text = if charging {
                    format!("{} ↑", percent(charge))
                } else {
                    percent(charge)
                };
                Reading::number(charge, text)
            }),
            WidgetKind::Fan => {
                let Stateful::Fan(fan) = self.sampler(slot, || Stateful::Fan(Default::default()))
                else {
                    unreachable!()
                };
                fan.sample(source)
                    .map(|rpm| Reading::number(rpm, format!("{rpm:.0} rpm")))
            }
            WidgetKind::Load => {
                system::load_average().map(|load| Reading::number(load, format!("{load:.2}")))
            }
            WidgetKind::Uptime => system::uptime().map(|up| Reading::text(system::uptime_text(up))),
            WidgetKind::Cpu => {
                let Stateful::Cpu(cpu) = self.sampler(slot, || Stateful::Cpu(Default::default()))
                else {
                    unreachable!()
                };
                cpu.sample()
                    .map(|busy| Reading::number(busy, percent(busy)))
            }
            WidgetKind::Memory => {
                system::memory_used().map(|used| Reading::number(used, percent(used)))
            }
            WidgetKind::Disk => {
                system::disk_used(source).map(|used| Reading::number(used, percent(used)))
            }
            WidgetKind::Temperature => {
                let Stateful::Temperature(sensor) =
                    self.sampler(slot, || Stateful::Temperature(Default::default()))
                else {
                    unreachable!()
                };
                let units = widget.units();
                sensor
                    .sample(source, units)
                    .map(|degrees| Reading::number(degrees, temperature(degrees, units)))
            }
            WidgetKind::Network => {
                let Stateful::Network(net) =
                    self.sampler(slot, || Stateful::Network(Default::default()))
                else {
                    unreachable!()
                };
                net.sample(source).map(|(rx, tx)| {
                    Reading::number(
                        rx + tx,
                        format!("↓{} ↑{}", system::rate(rx), system::rate(tx)),
                    )
                })
            }
            WidgetKind::Command | WidgetKind::Gpu | WidgetKind::Volume => {
                self.enqueue(&self.commands, slot, generation, widget);
                None
            }
            WidgetKind::Weather => {
                self.enqueue(&self.network, slot, generation, widget);
                None
            }
            // Counted by the engine from taps on their key, never sampled.
            WidgetKind::Timer | WidgetKind::Stopwatch => None,
            WidgetKind::Media => {
                self.enqueue(&self.media, slot, generation, widget);
                None
            }
        }
    }

    /// A waker for the engine's loop, for other workers to report through.
    pub fn waker(&self) -> Waker {
        self.waker.clone()
    }

    /// Forget samplers for slots that are no longer showing anything.
    pub fn retain(&mut self, live: impl Fn(Slot) -> bool) {
        self.samplers.retain(|slot, _| live(*slot));
    }

    /// The sampler for `slot`, made fresh if it held a different kind.
    fn sampler(&mut self, slot: Slot, make: impl Fn() -> Stateful) -> &mut Stateful {
        let fresh = make();
        let entry = self.samplers.entry(slot).or_insert_with(&make);
        if std::mem::discriminant(entry) != std::mem::discriminant(&fresh) {
            *entry = fresh;
        }
        entry
    }

    fn enqueue(&self, queue: &SyncSender<Job>, slot: Slot, generation: u64, widget: &Widget) {
        let job = Job {
            slot,
            generation,
            widget: widget.clone(),
        };
        match queue.try_send(job) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                log::debug!("widget queue full, skipping a refresh of {slot:?}")
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

/// Start a worker that answers jobs with `handle`.
///
/// `handle` returns `None` to send nothing at all — the previous reading
/// stays — and `Some(None)` to report a failure.
fn spawn_worker(
    name: &str,
    samples: SyncSender<Sample>,
    waker: Waker,
    mut handle: impl FnMut(&Widget) -> Option<Option<Reading>> + Send + 'static,
) -> SyncSender<Job> {
    let (jobs_tx, jobs_rx) = sync_channel::<Job>(QUEUE_DEPTH);
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            for job in jobs_rx {
                let Some(reading) = handle(&job.widget) else {
                    continue;
                };
                let sample = Sample {
                    slot: job.slot,
                    generation: job.generation,
                    reading,
                };
                // A full channel means the engine is behind; the next
                // refresh will carry newer readings anyway.
                if samples.try_send(sample).is_ok() {
                    waker.notify();
                }
            }
        })
        .expect("spawning a widget worker");
    jobs_tx
}

/// The time now, in `zone` if one is named and known, else locally.
///
/// jiff arrives with env_logger, so this costs nothing new, and getting time
/// zones right is not something to hand-roll. A zone the tz database does not
/// know is logged once per sample and the local time shown, so a typo shows
/// the wrong time rather than no time.
pub fn now_in(zone: Option<&str>) -> jiff::Zoned {
    let now = jiff::Zoned::now();
    match zone {
        Some(name) => match jiff::tz::TimeZone::get(name) {
            Ok(tz) => now.with_time_zone(tz),
            Err(e) => {
                log::debug!("time zone {name:?}: {e}");
                now
            }
        },
        None => now,
    }
}

fn percent(value: f64) -> String {
    format!("{value:.0}%")
}

fn temperature(degrees: f64, units: Units) -> String {
    match units {
        Units::Celsius => format!("{degrees:.0}°C"),
        Units::Fahrenheit => format!("{degrees:.0}°F"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use galdeck_model::WidgetView;

    fn graph(kind: WidgetKind) -> Widget {
        Widget {
            view: Some(WidgetView::Graph),
            history: Some(3),
            ..Widget::of(kind)
        }
    }

    #[test]
    fn a_correction_shows_at_once_but_leaves_a_graph_s_history_alone() {
        let widget = graph(WidgetKind::Volume);
        let mut state = SlotState::default();
        state.update(Some(Reading::number(20.0, "20%".into())), &widget);
        assert!(state.correct(Some(Reading::number(40.0, "40%".into())), &widget));
        assert_eq!(state.history, [20.0]);
        assert_eq!(state.reading.as_ref().and_then(Reading::value), Some(40.0));
    }

    #[test]
    fn a_label_near_its_threshold_does_not_flicker() {
        let widget = Widget {
            warn: Some(4.0),
            ..Widget::of(WidgetKind::Load)
        };
        let mut state = SlotState::default();
        let levels: Vec<galdeck_model::Level> = [4.01, 3.99, 4.02, 3.98, 3.5]
            .into_iter()
            .map(|value| {
                state.update(Some(Reading::number(value, format!("{value}"))), &widget);
                state.level
            })
            .collect();
        use galdeck_model::Level::{Normal, Warn};
        // In at the line; out only once well back from it.
        assert_eq!(levels, [Warn, Warn, Warn, Warn, Normal]);
    }

    #[test]
    fn a_graph_keeps_only_its_history() {
        let widget = graph(WidgetKind::Cpu);
        let mut state = SlotState::default();
        for value in [1.0, 2.0, 3.0, 4.0] {
            state.update(Some(Reading::number(value, String::new())), &widget);
        }
        assert_eq!(state.history, [2.0, 3.0, 4.0]);
    }

    #[test]
    fn an_unchanged_label_is_not_a_change_but_a_graph_always_moves() {
        let label = Widget::of(WidgetKind::Memory);
        let mut state = SlotState::default();
        let reading = Reading::number(50.0, "50%".into());
        assert!(state.update(Some(reading.clone()), &label));
        assert!(!state.update(Some(reading.clone()), &label));

        let mut state = SlotState::default();
        let graphed = graph(WidgetKind::Memory);
        assert!(state.update(Some(reading.clone()), &graphed));
        assert!(state.update(Some(reading), &graphed));
    }

    #[test]
    fn a_graph_with_no_fixed_top_scales_to_its_peak() {
        let widget = graph(WidgetKind::Network);
        let mut state = SlotState::default();
        state.update(Some(Reading::number(300.0, String::new())), &widget);
        state.update(Some(Reading::number(100.0, String::new())), &widget);
        assert_eq!(state.scale(&widget), 300.0);
        assert_eq!(state.scale(&graph(WidgetKind::Cpu)), 100.0);
    }

    #[test]
    fn a_slot_that_changes_kind_gets_a_fresh_sampler() {
        let (mut host, _rx) = WidgetHost::new(galdeck_core::wake_channel().0);
        let slot = Slot::Key(0);
        host.sample(slot, 0, &Widget::of(WidgetKind::Cpu));
        assert!(matches!(host.samplers.get(&slot), Some(Stateful::Cpu(_))));
        host.sample(slot, 0, &Widget::of(WidgetKind::Network));
        assert!(matches!(
            host.samplers.get(&slot),
            Some(Stateful::Network(_))
        ));
    }
}

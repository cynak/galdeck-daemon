//! Sampling widgets.
//!
//! Two classes, and the difference matters more than anything else here.
//! A clock read or a few bytes from `/proc` takes microseconds and happens on
//! the thread that owns the daemon's state. A shell command takes as long as
//! someone else's script decides to take, so it happens on a worker and comes
//! back through a channel — otherwise one slow widget stalls every key on the
//! deck, and the device stops being polled.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use galdeck_core::Waker;
use galdeck_model::{Widget, WidgetKind};

/// How long a command widget may take before it is killed.
///
/// Long enough for anything reasonable, short enough that a wedged script does
/// not hold up every other command widget behind it in the queue.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// How often to check whether a command has finished.
const COMMAND_POLL: Duration = Duration::from_millis(20);
/// How many command samples may be waiting at once.
///
/// A widget that cannot keep up with its own interval should drop refreshes
/// rather than build a backlog it pays off after the user has moved on.
const QUEUE_DEPTH: usize = 8;

/// A widget's new text, on its way back to the engine.
#[derive(Debug, Clone)]
pub struct Sample {
    pub key: u8,
    /// `None` when the widget failed; the caller falls back to the label.
    pub text: Option<String>,
}

struct Job {
    key: u8,
    command: String,
}

/// Runs the widgets that cannot be sampled inline.
pub struct WidgetHost {
    jobs: SyncSender<Job>,
    cpu: CpuSampler,
}

impl WidgetHost {
    pub fn new(waker: Waker) -> (Self, Receiver<Sample>) {
        let (jobs_tx, jobs_rx) = sync_channel::<Job>(QUEUE_DEPTH);
        let (samples_tx, samples_rx) = sync_channel::<Sample>(QUEUE_DEPTH);

        std::thread::Builder::new()
            .name("galdeck-widgets".into())
            .spawn(move || {
                for job in jobs_rx {
                    let text = run_command(&job.command);
                    // A full channel means the engine is behind; the next
                    // refresh will carry newer text anyway.
                    if samples_tx.try_send(Sample { key: job.key, text }).is_ok() {
                        waker.notify();
                    }
                }
            })
            .expect("spawning the widget worker");

        (
            Self {
                jobs: jobs_tx,
                cpu: CpuSampler::default(),
            },
            samples_rx,
        )
    }

    /// Sample a widget.
    ///
    /// Returns `Some` for the cheap kinds, which are answered here and now.
    /// `None` means the answer is coming through the channel later — or that
    /// there is no answer.
    pub fn sample(&mut self, key: u8, widget: &Widget) -> Option<String> {
        match widget.kind {
            WidgetKind::Clock | WidgetKind::Date => Some(format_time(widget.format())),
            WidgetKind::Cpu => self.cpu.sample().map(|busy| format!("{busy:.0}%")),
            WidgetKind::Memory => memory_used().map(|used| format!("{used:.0}%")),
            WidgetKind::Command => {
                let command = widget.command.clone()?;
                match self.jobs.try_send(Job { key, command }) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        log::debug!("widget queue full, skipping a refresh of key {key}")
                    }
                    Err(TrySendError::Disconnected(_)) => {}
                }
                None
            }
        }
    }
}

fn format_time(format: &str) -> String {
    // jiff arrives with env_logger, so this costs nothing new, and getting the
    // local timezone right is not something to hand-roll.
    jiff::Zoned::now().strftime(format).to_string()
}

/// Total CPU use since the previous sample.
///
/// `/proc/stat` counts time since boot, so a single reading says nothing; the
/// number people mean by "CPU use" is a difference between two.
#[derive(Default)]
struct CpuSampler {
    previous: Option<(u64, u64)>,
}

impl CpuSampler {
    fn sample(&mut self) -> Option<f32> {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let line = stat.lines().next()?.strip_prefix("cpu ")?;
        let fields: Vec<u64> = line
            .split_whitespace()
            .filter_map(|f| f.parse().ok())
            .collect();
        // user nice system idle iowait irq softirq steal ...
        if fields.len() < 5 {
            return None;
        }
        let idle = fields[3] + fields[4];
        let total: u64 = fields.iter().sum();

        let result = match self.previous {
            Some((previous_idle, previous_total)) if total > previous_total => {
                let busy = (total - previous_total) - (idle.saturating_sub(previous_idle));
                Some(busy as f32 * 100.0 / (total - previous_total) as f32)
            }
            // The first reading has nothing to compare against.
            _ => None,
        };
        self.previous = Some((idle, total));
        result
    }
}

fn memory_used() -> Option<f32> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = None;
    let mut available = None;
    for line in meminfo.lines() {
        let value = || {
            line.split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<f64>().ok())
        };
        if line.starts_with("MemTotal:") {
            total = value();
        } else if line.starts_with("MemAvailable:") {
            available = value();
        }
    }
    let (total, available) = (total?, available?);
    if total <= 0.0 {
        return None;
    }
    Some(((total - available) / total * 100.0) as f32)
}

/// Run a command and take the first line of its output.
///
/// Killed if it overruns, so one wedged script cannot hold up every other
/// command widget behind it.
fn run_command(command: &str) -> Option<String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| log::warn!("widget command failed to start: {e}"))
        .ok()?;

    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                log::warn!("widget command exceeded {COMMAND_TIMEOUT:?}, killing it: {command}");
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(COMMAND_POLL),
            Err(e) => {
                log::warn!("waiting on a widget command: {e}");
                return None;
            }
        }
    }

    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        // Bounded: a key shows a handful of characters, and a command that
        // prints a megabyte should not cost a megabyte per refresh.
        let mut buffer = vec![0u8; galdeck_model::widget::MAX_OUTPUT_BYTES];
        if let Ok(n) = stdout.read(&mut buffer) {
            output = String::from_utf8_lossy(&buffer[..n]).into_owned();
        }
    }
    let first = output.lines().next().unwrap_or_default().trim().to_string();
    (!first.is_empty()).then_some(first)
}

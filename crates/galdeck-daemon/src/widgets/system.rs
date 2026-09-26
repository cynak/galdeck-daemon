//! Readings from the kernel: `/proc`, `/sys`, and one filesystem call.
//!
//! Everything here except the NVIDIA fallback is a few bytes read from a
//! pseudo-filesystem, fast enough to do on the thread that owns the deck.
//! Paths are passed in rather than hard-coded so the parsing can be tested
//! against a directory of fixtures instead of whatever machine runs the tests.

use std::path::{Path, PathBuf};
use std::time::Instant;

use galdeck_model::Units;

use crate::audio::{self, AudioTarget};

/// Total CPU use since the previous sample.
///
/// `/proc/stat` counts time since boot, so a single reading says nothing; the
/// number people mean by "CPU use" is a difference between two.
#[derive(Default)]
pub struct CpuSampler {
    previous: Option<(u64, u64)>,
}

impl CpuSampler {
    pub fn sample(&mut self) -> Option<f64> {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        self.sample_from(&stat)
    }

    fn sample_from(&mut self, stat: &str) -> Option<f64> {
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
                Some(busy as f64 * 100.0 / (total - previous_total) as f64)
            }
            // The first reading has nothing to compare against.
            _ => None,
        };
        self.previous = Some((idle, total));
        result
    }
}

pub fn memory_used() -> Option<f64> {
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
    Some((total - available) / total * 100.0)
}

/// Chips that measure the CPU package, most specific first.
///
/// What people mean by "the temperature" with nothing else said. `zenpower`
/// is the out-of-tree replacement for `k10temp` and is preferred when both
/// are loaded, because it is loaded on purpose.
const CPU_CHIPS: &[&str] = &["zenpower", "k10temp", "coretemp", "cpu_thermal"];

/// A hwmon temperature input, found once and then read on every sample.
///
/// Finding it means walking every chip and every label; reading it is one
/// small file. Keeping the path means only the first sample pays for the
/// walk, and a sensor that disappears (a GPU driver unloading) is looked
/// for again rather than read forever as an error.
#[derive(Default)]
pub struct TemperatureSampler {
    found: Option<(Option<String>, PathBuf)>,
}

impl TemperatureSampler {
    pub fn sample(&mut self, source: Option<&str>, units: Units) -> Option<f64> {
        self.sample_in(Path::new("/sys/class/hwmon"), source, units)
    }

    fn sample_in(&mut self, root: &Path, source: Option<&str>, units: Units) -> Option<f64> {
        let wanted = source.map(str::to_string);
        let path = match &self.found {
            Some((was, path)) if *was == wanted => path.clone(),
            _ => {
                let path = find_temperature(root, source)?;
                self.found = Some((wanted, path.clone()));
                path
            }
        };
        let millidegrees = match read_number(&path) {
            Some(value) => value,
            None => {
                self.found = None;
                return None;
            }
        };
        let celsius = millidegrees / 1000.0;
        Some(match units {
            Units::Celsius => celsius,
            Units::Fahrenheit => celsius * 9.0 / 5.0 + 32.0,
        })
    }
}

/// The `tempN_input` to read for `source`.
///
/// A source matches a chip's `name` (`k10temp`, `amdgpu`, `nvme`) or one of
/// its sensors' labels (`Tctl`, `Package id 0`, `edge`), ignoring case. With
/// no source, the CPU. A chip with several sensors and no label asked for
/// gives its first, which is the package or edge reading on every driver
/// worth mentioning.
fn find_temperature(root: &Path, source: Option<&str>) -> Option<PathBuf> {
    find_hwmon(root, "temp", source, Some(CPU_CHIPS))
}

/// The `fanN_input` to read for `source`: a chip name or a fan label, like a
/// temperature. With no source, the first fan anywhere that is reporting.
fn find_fan(root: &Path, source: Option<&str>) -> Option<PathBuf> {
    find_hwmon(root, "fan", source, None)
}

/// An hwmon input of type `prefix` (`temp`, `fan`) matching `source`.
///
/// With no source: the first input on the first of `preferred` present, or
/// with none preferred, the first input on any chip.
fn find_hwmon(
    root: &Path,
    prefix: &str,
    source: Option<&str>,
    preferred: Option<&[&str]>,
) -> Option<PathBuf> {
    let mut chips: Vec<(String, PathBuf)> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|entry| {
            let dir = entry.ok()?.path();
            let name = std::fs::read_to_string(dir.join("name")).ok()?;
            Some((name.trim().to_string(), dir))
        })
        .collect();
    // Directory order is whatever the kernel enumerated first; sorting makes
    // "the first nvme" mean the same drive across reboots.
    chips.sort();

    let inputs = |dir: &Path| -> Vec<(u32, PathBuf)> {
        let mut found: Vec<(u32, PathBuf)> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let name = entry.ok()?.file_name().into_string().ok()?;
                let index = name.strip_prefix(prefix)?.strip_suffix("_input")?;
                Some((index.parse().ok()?, dir.join(&name)))
            })
            .collect();
        found.sort();
        found
    };

    match (source, preferred) {
        (None, Some(preferred)) => preferred.iter().find_map(|wanted| {
            let (_, dir) = chips.iter().find(|(name, _)| name == wanted)?;
            inputs(dir).into_iter().next().map(|(_, path)| path)
        }),
        (None, None) => chips
            .iter()
            .find_map(|(_, dir)| inputs(dir).into_iter().next().map(|(_, path)| path)),
        (Some(source), _) => {
            let source = source.to_lowercase();
            if let Some((_, dir)) = chips.iter().find(|(name, _)| name.to_lowercase() == source) {
                return inputs(dir).into_iter().next().map(|(_, path)| path);
            }
            chips.iter().find_map(|(_, dir)| {
                inputs(dir).into_iter().find_map(|(index, path)| {
                    let label =
                        std::fs::read_to_string(dir.join(format!("{prefix}{index}_label"))).ok()?;
                    (label.trim().to_lowercase() == source).then_some(path)
                })
            })
        }
    }
}

/// A fan's speed in RPM, found once and then read, like a temperature.
#[derive(Default)]
pub struct FanSampler {
    found: Option<(Option<String>, PathBuf)>,
}

impl FanSampler {
    pub fn sample(&mut self, source: Option<&str>) -> Option<f64> {
        self.sample_in(Path::new("/sys/class/hwmon"), source)
    }

    fn sample_in(&mut self, root: &Path, source: Option<&str>) -> Option<f64> {
        let wanted = source.map(str::to_string);
        let path = match &self.found {
            Some((was, path)) if *was == wanted => path.clone(),
            _ => {
                let path = find_fan(root, source)?;
                self.found = Some((wanted, path.clone()));
                path
            }
        };
        let rpm = read_number(&path);
        if rpm.is_none() {
            self.found = None;
        }
        rpm
    }
}

/// A battery's charge as a percentage, and whether it is charging.
pub fn battery(source: Option<&str>) -> Option<(f64, bool)> {
    battery_in(Path::new("/sys/class/power_supply"), source)
}

fn battery_in(root: &Path, source: Option<&str>) -> Option<(f64, bool)> {
    let mut supplies: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|entry| Some(entry.ok()?.path()))
        .collect();
    supplies.sort();
    let chosen = supplies.into_iter().find(|dir| {
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        match source {
            Some(source) => name.eq_ignore_ascii_case(source),
            // Mains adapters and the batteries in a wireless mouse are power
            // supplies too; only a system battery says it is one.
            None => {
                std::fs::read_to_string(dir.join("type")).is_ok_and(|t| t.trim() == "Battery")
                    && std::fs::read_to_string(dir.join("scope"))
                        .map_or(true, |scope| scope.trim() != "Device")
            }
        }
    })?;
    let capacity = read_number(&chosen.join("capacity"))?;
    let status = std::fs::read_to_string(chosen.join("status")).unwrap_or_default();
    Some((capacity, status.trim() == "Charging"))
}

/// The one-minute load average.
pub fn load_average() -> Option<f64> {
    std::fs::read_to_string("/proc/loadavg")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// How long the machine has been up.
pub fn uptime() -> Option<std::time::Duration> {
    let seconds: f64 = std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(std::time::Duration::from_secs_f64(seconds))
}

/// An uptime in the two largest units that matter: `3d 4h`, `5h 12m`, `12m`.
pub fn uptime_text(uptime: std::time::Duration) -> String {
    let minutes = uptime.as_secs() / 60;
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours) {
        (0, 0) => format!("{minutes}m"),
        (0, _) => format!("{hours}h {minutes}m"),
        _ => format!("{days}d {hours}h"),
    }
}

/// A sound device's volume as a percentage, and whether it is muted.
/// Blocking: it asks the sound server, through [`crate::audio`].
///
/// No source means the default output and `mic` the default input; anything
/// else names the node to ask, by its id from `wpctl status` (a name works
/// only under pactl). A source that could not be a node reads as nothing, and
/// so does a machine with no sound tool, which is why both are only logged at
/// debug: this runs twice a second.
pub fn volume(source: Option<&str>) -> Option<(f64, bool)> {
    let target = AudioTarget::parse(source)
        .map_err(|e| log::debug!("volume widget: {e}"))
        .ok()?;
    let level = audio::read(&target)
        .map_err(|e| log::debug!("reading the volume of {target:?}: {e}"))
        .ok()?;
    Some((level.percent, level.muted))
}

/// GPU utilisation from the kernel, where the driver offers it.
///
/// amdgpu and i915's newer successor `xe` expose `gpu_busy_percent`; NVIDIA's
/// driver exposes nothing, which is why [`gpu_busy`] falls back to asking
/// `nvidia-smi` — and why the whole kind runs on a worker.
pub fn gpu_busy_sysfs(source: Option<&str>) -> Option<f64> {
    gpu_busy_in(Path::new("/sys/class/drm"), source)
}

fn gpu_busy_in(root: &Path, source: Option<&str>) -> Option<f64> {
    if let Some(card) = source {
        return read_number(&root.join(card).join("device/gpu_busy_percent"));
    }
    let mut cards: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?;
            // `card1-DP-2` is a connector, not a GPU.
            (name.starts_with("card") && !name.contains('-')).then_some(path)
        })
        .collect();
    cards.sort();
    cards
        .iter()
        .find_map(|card| read_number(&card.join("device/gpu_busy_percent")))
}

/// GPU utilisation from wherever it can be had. Blocking.
pub fn gpu_busy(source: Option<&str>) -> Option<f64> {
    if let Some(busy) = gpu_busy_sysfs(source) {
        return Some(busy);
    }
    let output = super::command::run(
        "nvidia-smi --query-gpu=utilization.gpu --format=csv,noheader,nounits",
    )?;
    output.trim().parse().ok()
}

/// Network throughput, in bytes per second, received and sent.
#[derive(Default)]
pub struct NetworkSampler {
    previous: Option<(Instant, u64, u64)>,
}

impl NetworkSampler {
    pub fn sample(&mut self, source: Option<&str>) -> Option<(f64, f64)> {
        let dev = std::fs::read_to_string("/proc/net/dev").ok()?;
        self.sample_from(&dev, source, Instant::now())
    }

    fn sample_from(&mut self, dev: &str, source: Option<&str>, now: Instant) -> Option<(f64, f64)> {
        let (rx, tx) = totals(dev, source)?;
        let result = match self.previous {
            Some((then, previous_rx, previous_tx)) => {
                let seconds = now.duration_since(then).as_secs_f64();
                // A counter going backwards is an interface that was reset;
                // show nothing for one sample rather than an absurd spike.
                (seconds > 0.0 && rx >= previous_rx && tx >= previous_tx).then(|| {
                    (
                        (rx - previous_rx) as f64 / seconds,
                        (tx - previous_tx) as f64 / seconds,
                    )
                })
            }
            None => None,
        };
        self.previous = Some((now, rx, tx));
        result
    }
}

/// Received and sent byte counters, for one interface or all but loopback.
fn totals(dev: &str, source: Option<&str>) -> Option<(u64, u64)> {
    let mut rx = 0u64;
    let mut tx = 0u64;
    let mut any = false;
    // Two header lines, then `iface: rx_bytes packets ... tx_bytes ...`.
    for line in dev.lines().skip(2) {
        let Some((name, counters)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let wanted = match source {
            Some(source) => name == source,
            None => name != "lo",
        };
        if !wanted {
            continue;
        }
        let fields: Vec<u64> = counters
            .split_whitespace()
            .filter_map(|f| f.parse().ok())
            .collect();
        if fields.len() < 9 {
            continue;
        }
        rx += fields[0];
        tx += fields[8];
        any = true;
    }
    any.then_some((rx, tx))
}

/// Space used on the filesystem holding `path`, as a percentage.
///
/// Measured against what an unprivileged user can have, as `df` does: the
/// blocks reserved for root are space nobody else will ever get.
pub fn disk_used(path: Option<&str>) -> Option<f64> {
    let path = std::ffi::CString::new(path.unwrap_or("/")).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is a valid C string and `stat` is a properly sized,
    // writable statvfs that outlives the call.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let used = stat.f_blocks.saturating_sub(stat.f_bfree) as f64;
    let usable = used + stat.f_bavail as f64;
    (usable > 0.0).then(|| used / usable * 100.0)
}

fn read_number(path: &Path) -> Option<f64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Bytes per second, short enough for a key: `850K`, `12.4M`.
pub fn rate(bytes_per_second: f64) -> String {
    const UNITS: &[&str] = &["B", "K", "M", "G"];
    let mut value = bytes_per_second.max(0.0);
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 && unit > 0 {
        format!("{value:.1}{}", UNITS[unit])
    } else {
        format!("{value:.0}{}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "galdeck-hwmon-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn chip(root: &Path, dir: &str, name: &str, sensors: &[(u32, Option<&str>, i64)]) {
        let dir = root.join(dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("name"), format!("{name}\n")).unwrap();
        for (index, label, millidegrees) in sensors {
            std::fs::write(
                dir.join(format!("temp{index}_input")),
                format!("{millidegrees}\n"),
            )
            .unwrap();
            if let Some(label) = label {
                std::fs::write(dir.join(format!("temp{index}_label")), format!("{label}\n"))
                    .unwrap();
            }
        }
    }

    #[test]
    fn cpu_use_is_a_difference_between_two_readings() {
        let mut cpu = CpuSampler::default();
        assert_eq!(cpu.sample_from("cpu  100 0 100 800 0 0 0 0\n"), None);
        // 100 more jiffies, 25 of them idle.
        let busy = cpu.sample_from("cpu  150 0 125 825 0 0 0 0\n").unwrap();
        assert!((busy - 75.0).abs() < 0.01, "{busy}");
    }

    #[test]
    fn with_no_source_the_temperature_is_the_cpus() {
        let root = fixture();
        chip(&root, "hwmon0", "nvme", &[(1, Some("Composite"), 41_000)]);
        chip(&root, "hwmon1", "k10temp", &[(1, Some("Tctl"), 55_250)]);
        let mut sampler = TemperatureSampler::default();
        assert_eq!(sampler.sample_in(&root, None, Units::Celsius), Some(55.25));
    }

    #[test]
    fn a_source_matches_a_chip_or_a_label() {
        let root = fixture();
        chip(&root, "hwmon0", "nvme", &[(1, Some("Composite"), 41_000)]);
        chip(
            &root,
            "hwmon1",
            "amdgpu",
            &[(1, Some("edge"), 50_000), (2, Some("junction"), 62_000)],
        );
        let mut sampler = TemperatureSampler::default();
        assert_eq!(
            sampler.sample_in(&root, Some("NVME"), Units::Celsius),
            Some(41.0)
        );
        assert_eq!(
            sampler.sample_in(&root, Some("junction"), Units::Celsius),
            Some(62.0)
        );
        assert_eq!(
            sampler.sample_in(&root, Some("missing"), Units::Celsius),
            None
        );
    }

    #[test]
    fn fahrenheit_is_converted() {
        let root = fixture();
        chip(
            &root,
            "hwmon0",
            "coretemp",
            &[(1, Some("Package id 0"), 100_000)],
        );
        let mut sampler = TemperatureSampler::default();
        assert_eq!(
            sampler.sample_in(&root, None, Units::Fahrenheit),
            Some(212.0)
        );
    }

    #[test]
    fn a_sensor_that_disappears_is_looked_for_again() {
        let root = fixture();
        chip(&root, "hwmon0", "k10temp", &[(1, None, 40_000)]);
        let mut sampler = TemperatureSampler::default();
        assert_eq!(sampler.sample_in(&root, None, Units::Celsius), Some(40.0));
        std::fs::remove_dir_all(root.join("hwmon0")).unwrap();
        chip(&root, "hwmon3", "k10temp", &[(1, None, 45_000)]);
        assert_eq!(sampler.sample_in(&root, None, Units::Celsius), None);
        assert_eq!(sampler.sample_in(&root, None, Units::Celsius), Some(45.0));
    }

    #[test]
    fn gpu_busy_skips_connectors_and_takes_the_first_card_that_reports() {
        let root = fixture();
        std::fs::create_dir_all(root.join("card0/device")).unwrap();
        std::fs::create_dir_all(root.join("card1-DP-2")).unwrap();
        std::fs::create_dir_all(root.join("card1/device")).unwrap();
        std::fs::write(root.join("card1/device/gpu_busy_percent"), "37\n").unwrap();
        assert_eq!(gpu_busy_in(&root, None), Some(37.0));
        assert_eq!(gpu_busy_in(&root, Some("card0")), None);
    }

    const DEV: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 5000      10    0    0    0     0          0         0     5000      10    0    0    0     0       0          0
  eth0: 1000      10    0    0    0     0          0         0     2000      10    0    0    0     0       0          0
 wlan0:  500      10    0    0    0     0          0         0      100      10    0    0    0     0       0          0
";

    #[test]
    fn network_totals_leave_out_loopback_unless_asked() {
        assert_eq!(totals(DEV, None), Some((1500, 2100)));
        assert_eq!(totals(DEV, Some("lo")), Some((5000, 5000)));
        assert_eq!(totals(DEV, Some("tun0")), None);
    }

    #[test]
    fn network_rate_is_bytes_per_second() {
        let mut sampler = NetworkSampler::default();
        let start = Instant::now();
        assert_eq!(sampler.sample_from(DEV, Some("eth0"), start), None);
        let later = DEV.replace("  eth0: 1000", "  eth0: 3048");
        let (rx, tx) = sampler
            .sample_from(
                &later,
                Some("eth0"),
                start + std::time::Duration::from_secs(2),
            )
            .unwrap();
        assert_eq!((rx, tx), (1024.0, 0.0));
    }

    #[test]
    fn a_fan_is_found_by_chip_or_label_or_as_the_first_there_is() {
        let root = fixture();
        let dir = root.join("hwmon2");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("name"), "dell_smm\n").unwrap();
        std::fs::write(dir.join("fan1_input"), "2400\n").unwrap();
        std::fs::write(dir.join("fan1_label"), "Processor Fan\n").unwrap();
        std::fs::write(dir.join("fan2_input"), "0\n").unwrap();
        let mut fans = FanSampler::default();
        assert_eq!(fans.sample_in(&root, None), Some(2400.0));
        assert_eq!(fans.sample_in(&root, Some("processor fan")), Some(2400.0));
        assert_eq!(fans.sample_in(&root, Some("dell_smm")), Some(2400.0));
        assert_eq!(fans.sample_in(&root, Some("nope")), None);
    }

    #[test]
    fn the_battery_is_the_system_one_not_the_mouses() {
        let root = fixture();
        for (name, kind, scope, capacity, status) in [
            ("AC", "Mains", None, None, None),
            ("BAT0", "Battery", None, Some("87"), Some("Charging")),
            (
                "hidpp_battery_0",
                "Battery",
                Some("Device"),
                Some("40"),
                Some("Discharging"),
            ),
        ] {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("type"), kind).unwrap();
            if let Some(scope) = scope {
                std::fs::write(dir.join("scope"), scope).unwrap();
            }
            if let Some(capacity) = capacity {
                std::fs::write(dir.join("capacity"), capacity).unwrap();
            }
            if let Some(status) = status {
                std::fs::write(dir.join("status"), status).unwrap();
            }
        }
        assert_eq!(battery_in(&root, None), Some((87.0, true)));
        assert_eq!(
            battery_in(&root, Some("hidpp_battery_0")),
            Some((40.0, false))
        );
        assert_eq!(battery_in(&root, Some("AC")), None);
    }

    #[test]
    fn uptimes_are_two_units_at_most() {
        use std::time::Duration;
        assert_eq!(uptime_text(Duration::from_secs(12 * 60 + 5)), "12m");
        assert_eq!(
            uptime_text(Duration::from_secs(5 * 3600 + 12 * 60)),
            "5h 12m"
        );
        assert_eq!(
            uptime_text(Duration::from_secs(3 * 86400 + 4 * 3600 + 59)),
            "3d 4h"
        );
    }

    #[test]
    fn rates_are_short() {
        assert_eq!(rate(0.0), "0B");
        assert_eq!(rate(850.0), "850B");
        assert_eq!(rate(2048.0), "2.0K");
        assert_eq!(rate(12.4 * 1024.0 * 1024.0), "12M");
    }

    #[test]
    fn the_root_filesystem_has_a_usage() {
        let used = disk_used(None).expect("statvfs on /");
        assert!((0.0..=100.0).contains(&used));
    }
}

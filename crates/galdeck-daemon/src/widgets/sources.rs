//! What a widget's `source` could name on this machine.
//!
//! For an editor to offer as a list. Each entry carries something to
//! recognise it by — a reading, a size, what is playing — because `hwmon3`'s
//! `temp2` means nothing to anyone, and "Tctl · 55°C" does.
//!
//! Blocking, because listing media players is a D-Bus call; the engine runs
//! this on a thread of its own.

use std::path::Path;

use galdeck_ipc::{SourceOption, WidgetSources};

pub fn discover() -> WidgetSources {
    WidgetSources {
        temperature: temperatures(Path::new("/sys/class/hwmon")),
        gpu: gpus(Path::new("/sys/class/drm")),
        network: interfaces(Path::new("/sys/class/net")),
        disk: filesystems(),
        media: players(),
        battery: batteries(Path::new("/sys/class/power_supply")),
        fan: fans(Path::new("/sys/class/hwmon")),
        // The two everyone has; a particular node's id is for those who
        // already know it.
        volume: vec![
            option("", Some("the default output".into())),
            option("mic", Some("the default input".into())),
        ],
    }
}

/// Every battery, with its charge.
fn batteries(root: &Path) -> Vec<SourceOption> {
    let mut found: Vec<SourceOption> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let dir = entry.ok()?.path();
            if read(&dir.join("type"))? != "Battery" {
                return None;
            }
            let name = dir.file_name()?.to_str()?.to_string();
            let model = read(&dir.join("model_name")).filter(|m| !m.is_empty());
            let charge = read(&dir.join("capacity")).map(|c| format!("{c}%"));
            let detail = [model, charge]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
            Some(option(name, (!detail.is_empty()).then_some(detail)))
        })
        .collect();
    found.sort_by(|a, b| a.value.cmp(&b.value));
    found
}

/// Every chip with a fan, then every labelled fan, each with its speed.
fn fans(root: &Path) -> Vec<SourceOption> {
    let mut chips: Vec<(String, std::path::PathBuf)> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let dir = entry.ok()?.path();
            Some((read(&dir.join("name"))?, dir))
        })
        .collect();
    chips.sort();
    let mut by_chip: Vec<SourceOption> = Vec::new();
    let mut by_label: Vec<SourceOption> = Vec::new();
    for (chip, dir) in &chips {
        let mut inputs: Vec<(u32, String)> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let name = entry.ok()?.file_name().into_string().ok()?;
                let index = name.strip_prefix("fan")?.strip_suffix("_input")?;
                Some((index.parse().ok()?, read(&dir.join(&name))?))
            })
            .collect();
        inputs.sort();
        let Some((_, first)) = inputs.first() else {
            continue;
        };
        if !by_chip.iter().any(|o| &o.value == chip) {
            by_chip.push(option(chip, Some(format!("{first} rpm"))));
        }
        for (index, rpm) in &inputs {
            if let Some(label) = read(&dir.join(format!("fan{index}_label"))) {
                if !label.is_empty() && !by_label.iter().any(|o| o.value == label) {
                    by_label.push(option(label, Some(format!("{chip} · {rpm} rpm"))));
                }
            }
        }
    }
    by_chip.extend(by_label);
    by_chip
}

fn option(value: impl Into<String>, detail: Option<String>) -> SourceOption {
    SourceOption {
        value: value.into(),
        detail,
    }
}

fn read(path: &Path) -> Option<String> {
    Some(std::fs::read_to_string(path).ok()?.trim().to_string())
}

/// Every chip with a temperature input, then every labelled sensor on them.
///
/// Chips first because a chip name is the stable thing to write; a label is
/// for picking one sensor on a chip that has several.
fn temperatures(root: &Path) -> Vec<SourceOption> {
    let mut chips: Vec<(String, std::path::PathBuf)> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let dir = entry.ok()?.path();
            Some((read(&dir.join("name"))?, dir))
        })
        .collect();
    chips.sort();

    let mut by_chip = Vec::new();
    let mut by_label = Vec::new();
    for (chip, dir) in &chips {
        let mut inputs: Vec<(u32, f64)> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let name = entry.ok()?.file_name().into_string().ok()?;
                let index = name.strip_prefix("temp")?.strip_suffix("_input")?;
                let millidegrees: f64 = read(&dir.join(&name))?.parse().ok()?;
                Some((index.parse().ok()?, millidegrees / 1000.0))
            })
            .collect();
        inputs.sort_by_key(|(index, _)| *index);
        let Some((_, first)) = inputs.first() else {
            continue;
        };
        if !by_chip.iter().any(|o: &SourceOption| &o.value == chip) {
            by_chip.push(option(chip, Some(format!("{first:.0}°C"))));
        }
        for (index, degrees) in &inputs {
            let Some(label) = read(&dir.join(format!("temp{index}_label"))) else {
                continue;
            };
            if label.is_empty() || by_label.iter().any(|o: &SourceOption| o.value == label) {
                continue;
            }
            by_label.push(option(label, Some(format!("{chip} · {degrees:.0}°C"))));
        }
    }
    by_chip.extend(by_label);
    by_chip
}

fn gpus(root: &Path) -> Vec<SourceOption> {
    let mut cards: Vec<SourceOption> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?.to_string();
            if !name.starts_with("card") || name.contains('-') {
                return None;
            }
            // Only cards that can answer; a card without the counter would
            // just be a way to make the widget show nothing.
            let busy = read(&path.join("device/gpu_busy_percent"))?;
            let driver = std::fs::read_link(path.join("device/driver"))
                .ok()
                .and_then(|p| p.file_name()?.to_str().map(str::to_string));
            let detail = match driver {
                Some(driver) => format!("{driver} · {busy}% busy"),
                None => format!("{busy}% busy"),
            };
            Some(option(name, Some(detail)))
        })
        .collect();
    cards.sort_by(|a, b| a.value.cmp(&b.value));
    cards
}

fn interfaces(root: &Path) -> Vec<SourceOption> {
    let mut found: Vec<SourceOption> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().into_string().ok()?;
            (name != "lo").then(|| option(&name, read(&entry.path().join("operstate"))))
        })
        .collect();
    found.sort_by(|a, b| a.value.cmp(&b.value));
    found
}

/// Mounted filesystems backed by a device, which is what "disk" means to a
/// person: not `/proc`, `tmpfs` or the dozen cgroup mounts.
fn filesystems() -> Vec<SourceOption> {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };
    let mut found: Vec<SourceOption> = Vec::new();
    for line in mounts.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [device, mount, kind, ..] = fields[..] else {
            continue;
        };
        // Octal escapes are how /proc/mounts writes a space in a path.
        let mount = mount.replace("\\040", " ");
        if !device.starts_with("/dev/") || found.iter().any(|o| o.value == mount) {
            continue;
        }
        // A snap or a loop device is a mount, but not somewhere anyone runs
        // out of space.
        if kind == "squashfs" || mount.starts_with("/snap/") || mount.starts_with("/boot/efi") {
            continue;
        }
        let used = super::system::disk_used(Some(&mount))
            .map(|used| format!("{kind} · {used:.0}% used"))
            .unwrap_or_else(|| kind.to_string());
        found.push(option(mount, Some(used)));
    }
    found
}

fn players() -> Vec<SourceOption> {
    let mut seen: Vec<SourceOption> = Vec::new();
    for (name, status, title) in super::media::players() {
        // Browsers register as `firefox.instance_1_85`, and the instance
        // number changes every launch; the stable part is what to match on.
        let stable = name.split(".instance").next().unwrap_or(&name).to_string();
        if seen.iter().any(|o| o.value == stable) {
            continue;
        }
        let detail = match title {
            Some(title) => format!("{status:?} · {title}"),
            None => format!("{status:?}"),
        };
        seen.push(option(stable, Some(detail.to_lowercase())));
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("galdeck-sources-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn chips_come_before_labels_and_each_carries_a_reading() {
        let root = fixture("hwmon");
        for (dir, chip, sensors) in [
            ("hwmon0", "nvme", vec![(1, Some("Composite"), 41_000)]),
            (
                "hwmon1",
                "k10temp",
                vec![(1, Some("Tctl"), 55_000), (3, Some("Tccd1"), 50_000)],
            ),
            ("hwmon2", "acpitz", vec![]),
        ] {
            let dir = root.join(dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("name"), chip).unwrap();
            for (index, label, value) in sensors {
                std::fs::write(dir.join(format!("temp{index}_input")), value.to_string()).unwrap();
                if let Some(label) = label {
                    std::fs::write(dir.join(format!("temp{index}_label")), label).unwrap();
                }
            }
        }
        let found = temperatures(&root);
        let values: Vec<&str> = found.iter().map(|o| o.value.as_str()).collect();
        // acpitz has no inputs and is left out rather than offered broken.
        assert_eq!(values, ["k10temp", "nvme", "Tctl", "Tccd1", "Composite"]);
        assert_eq!(found[0].detail.as_deref(), Some("55°C"));
        assert_eq!(found[2].detail.as_deref(), Some("k10temp · 55°C"));
    }

    #[test]
    fn loopback_is_not_offered() {
        let root = fixture("net");
        for name in ["lo", "eth0"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(root.join(name).join("operstate"), "up\n").unwrap();
        }
        let found = interfaces(&root);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].value, "eth0");
        assert_eq!(found[0].detail.as_deref(), Some("up"));
    }

    #[test]
    fn the_root_filesystem_is_offered() {
        // Every machine that runs these tests has one, and it is backed by a
        // device everywhere except inside some containers.
        let found = filesystems();
        if !found.is_empty() {
            assert!(found.iter().all(|o| o.detail.is_some()));
        }
    }
}

//! Mains/battery state, read from sysfs.
//!
//! The kernel exposes every power supply as a directory under
//! `/sys/class/power_supply`, each with a `type` (`Mains`, `Battery`,
//! `USB`, …) and a `status`. We care about exactly one question: *is this
//! machine running unplugged?*
//!
//! The answer is "yes" only when a battery exists **and** is discharging.
//! That covers both halves of the request in one test:
//!   * a desktop has no `Battery` supply at all, so it never reports
//!     power saving no matter what the charger is doing;
//!   * a laptop that is plugged in reports `Charging`, `Full` or
//!     `Not charging` (a full battery sitting on AC does not discharge), so
//!     it only reports power saving while it is genuinely running on the
//!     battery.
//!
//! Reading sysfs is cheap but not free, so [`PowerMonitor`] throttles it:
//! [`PowerMonitor::poll`] only touches the filesystem at most once per
//! interval, and answers from the cached value in between.

use std::time::{Duration, Instant};

/// Where the kernel lists power supplies.
const POWER_SUPPLY_DIR: &str = "/sys/class/power_supply";

/// How often [`PowerMonitor::poll`] is allowed to touch the filesystem.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Cached view of whether the machine is running on battery.
pub struct PowerMonitor {
    on_battery: bool,
    last_poll: Option<Instant>,
}

impl Default for PowerMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl PowerMonitor {
    /// Create a monitor and take the first reading straight away, so callers
    /// never act on a default `false` they haven't checked.
    pub fn new() -> Self {
        let on_battery = on_battery_now();
        Self {
            on_battery,
            last_poll: Some(Instant::now()),
        }
    }

    /// Re-read the state if the poll interval has elapsed, and return the
    /// current answer. Cheap enough to call from the render timer: between
    /// polls it just hands back the cached value.
    pub fn poll(&mut self) -> bool {
        if self
            .last_poll
            .is_none_or(|t| t.elapsed() >= DEFAULT_POLL_INTERVAL)
        {
            self.on_battery = on_battery_now();
            self.last_poll = Some(Instant::now());
        }
        self.on_battery
    }
}

/// Read the real power supplies once: true if some battery is discharging.
fn on_battery_now() -> bool {
    on_battery_in(std::path::Path::new(POWER_SUPPLY_DIR))
}

/// The detection itself, over a given power supply directory. Split out from
/// [`on_battery_now`] so the tests can build a fake supply tree instead of
/// depending on whatever hardware happens to be running them.
fn on_battery_in(dir: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        // No sysfs power supply class (non-Linux, container, restricted
        // sandbox): assume mains, which is the safe default — a wallpaper
        // should never quietly throttle itself because it can't tell.
        return false;
    };
    for entry in entries.flatten() {
        let supply = entry.path();
        let Ok(kind) = read_trimmed(&supply.join("type")) else {
            continue;
        };
        if kind != "Battery" {
            continue;
        }
        // A battery that exists but isn't discharging means we're on AC.
        if read_trimmed(&supply.join("status")).is_ok_and(|s| s == "Discharging") {
            return true;
        }
    }
    false
}

fn read_trimmed(path: &std::path::Path) -> std::io::Result<String> {
    std::fs::read_to_string(path).map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake `/sys/class/power_supply` tree: one directory per supply.
    fn fake_supplies(entries: &[(&str, &str, &str)]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rustwall-power-test-{}-{:p}",
            std::process::id(),
            entries
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for (name, kind, status) in entries {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("type"), format!("{kind}\n")).unwrap();
            std::fs::write(dir.join("status"), format!("{status}\n")).unwrap();
        }
        root
    }

    #[test]
    fn discharging_battery_means_on_battery() {
        let root = fake_supplies(&[("BAT0", "Battery", "Discharging")]);
        assert!(on_battery_in(&root));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plugged_in_laptop_is_not_on_battery() {
        // A full battery sitting on AC reports Full or Not charging — the
        // case that would otherwise make a desktop-mode laptop think it is
        // unplugged.
        for status in ["Full", "Charging", "Not charging", "Unknown"] {
            let root = fake_supplies(&[("AC", "Mains", "Unknown"), ("BAT0", "Battery", status)]);
            assert!(
                !on_battery_in(&root),
                "status {status:?} must not count as on battery"
            );
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn a_machine_with_no_battery_never_throttles() {
        // A desktop: mains supplies only, or nothing at all.
        let desktop = fake_supplies(&[("AC", "Mains", "Unknown")]);
        assert!(!on_battery_in(&desktop));
        let _ = std::fs::remove_dir_all(&desktop);

        let empty = fake_supplies(&[]);
        assert!(!on_battery_in(&empty));
        let _ = std::fs::remove_dir_all(&empty);

        // And a path that doesn't exist at all (no sysfs class).
        assert!(!on_battery_in(std::path::Path::new(
            "/nonexistent/power_supply"
        )));
    }

    #[test]
    fn one_discharging_battery_among_several_is_enough() {
        let root = fake_supplies(&[
            ("AC", "Mains", "Unknown"),
            ("BAT0", "Battery", "Full"),
            ("BAT1", "Battery", "Discharging"),
        ]);
        assert!(on_battery_in(&root));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn poll_agrees_with_a_direct_read() {
        // Whatever this machine is, the monitor must answer something
        // consistent with reading sysfs directly.
        assert_eq!(PowerMonitor::new().poll(), on_battery_now());
    }

    #[test]
    fn repeated_polls_are_stable() {
        // Polling is throttled, so back-to-back polls can't disagree.
        let mut m = PowerMonitor::new();
        let first = m.poll();
        assert_eq!(m.poll(), first);
        assert_eq!(m.poll(), first);
    }
}

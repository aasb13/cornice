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
//! Reading sysfs is *not* reliably fast: on a laptop the power-supply
//! attributes are served by ACPI, and a read can occasionally block for tens of
//! milliseconds while the embedded controller is queried. Doing that on the
//! event loop thread freezes the wallpaper for the duration, so the polling
//! lives on its own thread and the event loop only ever reads an atomic.
//!
//! [`PowerMonitor::poll`] is therefore just a relaxed atomic load: it cannot
//! block, no matter what the kernel is doing.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Where the kernel lists power supplies.
const POWER_SUPPLY_DIR: &str = "/sys/class/power_supply";

/// How often the background thread re-reads the power supplies. Unplugging is
/// not something you notice within a second, and the slower we poll the less
/// we lean on ACPI.
const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Cached view of whether the machine is running on battery, refreshed by a
/// background thread. Cheap to ask, safe to ask from the render loop.
pub struct PowerMonitor {
    on_battery: Arc<AtomicBool>,
    /// Kept only so the thread isn't detached from the struct; the process
    /// exit ends it, and if it ever dies the cached value simply goes stale.
    _poller: Option<std::thread::JoinHandle<()>>,
}

impl Default for PowerMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl PowerMonitor {
    /// Start the poller and answer from its cache.
    ///
    /// The first reading is optimistic (`false` = on mains), so startup is
    /// never delayed by a slow ACPI query; the thread corrects it within
    /// milliseconds of spawning.
    pub fn new() -> Self {
        let on_battery = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&on_battery);
        let poller = std::thread::Builder::new()
            .name("cornice-power".to_string())
            .spawn(move || {
                loop {
                    flag.store(on_battery_now(), Ordering::Relaxed);
                    std::thread::sleep(POLL_INTERVAL);
                }
            })
            .ok();
        Self {
            on_battery,
            _poller: poller,
        }
    }

    /// The last known state. An atomic load: never touches the filesystem, so
    /// it cannot stall the event loop no matter how slow ACPI is being.
    pub fn poll(&mut self) -> bool {
        self.on_battery.load(Ordering::Relaxed)
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
            "cornice-power-test-{}-{:p}",
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
    fn monitor_converges_on_a_direct_read() {
        // The poller starts optimistic, so give it a moment to publish its
        // first real reading; then it must agree with reading sysfs directly.
        let expected = on_battery_now();
        let mut m = PowerMonitor::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while m.poll() != expected && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(m.poll(), expected);
    }

    #[test]
    fn poll_is_an_atomic_load_and_never_blocks() {
        // This is the whole point of the background thread: asking the
        // monitor must not touch the filesystem, so it cannot inherit ACPI's
        // tens-of-milliseconds stalls. A generous bound still catches a
        // regression back to synchronous I/O.
        let mut m = PowerMonitor::new();
        let t = std::time::Instant::now();
        for _ in 0..10_000 {
            std::hint::black_box(m.poll());
        }
        assert!(
            t.elapsed() < Duration::from_millis(50),
            "10k polls took {:?} — poll() is doing I/O on the caller's thread",
            t.elapsed()
        );
    }
}

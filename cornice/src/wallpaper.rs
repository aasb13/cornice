//! The backend-independent half of the wallpaper runtime.
//!
//! Both the Wayland and the X11 backend own a surface and a pixel buffer, but
//! what happens to the pixels — and when the config or the battery state has to
//! reach the source — is identical. Keeping that in one place means a reload or
//! a power transition behaves the same on either display server.

use std::time::Duration;

use log::info;

use crate::config::{Config, PowerSaveConfig, PowerState};
use crate::power::PowerMonitor;
use crate::source::FrameSource;
use cornice_config::Reload;

pub struct Wallpaper {
    pub source: Box<dyn FrameSource>,
    battery: PowerMonitor,
    power: PowerState,
    reload: Reload<Config>,
    interval: Option<Duration>,
    dimensions: (u32, u32),
}

impl Wallpaper {
    pub fn new(source: Box<dyn FrameSource>, reload: Reload<Config>) -> Self {
        Self {
            source,
            battery: PowerMonitor::new(),
            power: PowerState::default(),
            reload,
            interval: None,
            dimensions: (0, 0),
        }
    }

    /// The render interval the source asked for at `init`. Used as the timer's
    /// initial delay; the timer re-reads [`Self::tick_interval`] every frame.
    pub fn interval(&self) -> Option<Duration> {
        self.interval
    }

    pub fn tick_interval(&self) -> Option<Duration> {
        self.source.tick_interval()
    }

    /// First draw is about to happen. Tell the source its dimensions and its
    /// initial power budget, and remember its requested cadence.
    pub fn init(&mut self, width: u32, height: u32) {
        self.source.init(width, height);
        // The first battery reading happens before the first frame, so a
        // wallpaper that boots on battery starts throttled. There is no prior
        // belief to compare against, so the source is told either way.
        let discharging = self.battery.poll();
        let power = self.reload.config().power_save.resolve(discharging);
        self.power = power;
        self.source.set_power_saving(power);
        if power.throttling {
            info!("running on battery: starting throttled");
        }
        self.interval = self.source.tick_interval();
    }

    /// Re-read the config if it changed on disk. `Reload::poll` returns `None`
    /// both when nothing changed and when the file is broken — the latter is
    /// logged inside, which is the whole point of doing this live: saving a
    /// half-finished file must not blank the wallpaper or kill the process.
    fn check_reload(&mut self) {
        if self.reload.poll().is_some() {
            self.apply_config("config file changed");
        }
    }

    /// Re-read and apply the config unconditionally. `SIGHUP` uses this, since
    /// it has to work whether or not `[reload] watch` is on and whether or not
    /// the file looks changed.
    pub fn force_reload(&mut self) {
        if self.reload.force().is_some() {
            self.apply_config("SIGHUP");
        }
    }

    /// Adopt the config `Reload` just installed. The power-save table, the
    /// reload settings and the source's own settings all come out of the same
    /// new value, so they cannot disagree about which config is in force.
    fn apply_config(&mut self, reason: &str) {
        info!("applying config ({reason})");
        self.check_power();
        let (w, h) = self.dimensions;
        self.source.reconfigure(self.reload.config(), w, h);
        info!("config reloaded");
    }

    /// Current surface dimensions, for `FrameSource::reconfigure`.
    pub fn set_dimensions(&mut self, width: u32, height: u32) {
        self.dimensions = (width, height);
    }

    /// The window manager resized the surface: remember the new dimensions and
    /// tell the source so it can rebuild any size-dependent caches.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.dimensions = (width, height);
        self.source.reconfigure(self.reload.config(), width, height);
    }

    /// Notice a change of power source and tell the source about it. Unplugging
    /// costs a throttled wallpaper; plugging back in always restores full speed.
    /// Called once per frame, and it costs one atomic load.
    fn check_power(&mut self) {
        let discharging = self.battery.poll();
        let Some(now) = next_power_state(self.power, &self.reload.config().power_save, discharging)
        else {
            return;
        };
        self.power = now;
        info!(
            "power state: {}",
            if now.throttling {
                "on battery, throttling"
            } else {
                "on mains, full speed"
            }
        );
        self.source.set_power_saving(now);
    }

    /// Draw one frame: notice a config or power change first, then let the
    /// source paint. `false` means it had nothing new, so the caller skips the
    /// commit.
    pub fn render(&mut self, canvas: &mut [u8], width: u32, height: u32, stride: u32) -> bool {
        self.check_power();
        self.check_reload();
        self.source.render(canvas, width, height, stride)
    }
}

/// The power state to push to the source, or `None` if it already has it.
///
/// Comparing the whole [`PowerState`] rather than just the on/off half is what
/// lets a `[power_save]` edit reach the source while the machine is still
/// unplugged: with a bool as the key, retuning the battery frame rate produced
/// the same key and so nothing was sent.
fn next_power_state(
    last: PowerState,
    power_save: &PowerSaveConfig,
    discharging: bool,
) -> Option<PowerState> {
    let now = power_save.resolve(discharging);
    (now != last).then_some(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn power_save(enabled: bool, fps_cap: u32) -> PowerSaveConfig {
        PowerSaveConfig {
            enabled,
            fps_cap,
            ..Default::default()
        }
    }

    /// The source only hears about the power budget when the answer actually
    /// changes, so this table is the whole notification contract. Each row is
    /// (what the source currently believes, `enabled`, the battery cap, whether a
    /// battery is discharging, and what the source must be told).
    ///
    /// The last two rows are the bugs this exists to pin down, both cases where
    /// the *battery* never moves but the answer does:
    ///
    /// * `enabled` turned off while unplugged. A notification keyed only on
    ///   battery transitions never fires, and the wallpaper stays throttled
    ///   forever.
    /// * The battery cap retuned while still unplugged. With the on/off half as
    ///   the key this produced an identical key, so the new cap stayed invisible.
    #[test]
    fn the_source_is_told_exactly_when_the_budget_moves() {
        let idle = PowerState::default();
        let throttled_at = |cap| PowerState {
            throttling: true,
            fps_cap: cap,
            density_divisor: 4,
        };
        let cases: &[(PowerState, bool, u32, bool, Option<PowerState>)] = &[
            (idle, true, 30, false, None),
            (idle, true, 30, true, Some(throttled_at(30))),
            (throttled_at(30), true, 30, false, Some(idle)),
            (throttled_at(30), false, 30, true, Some(idle)),
            (throttled_at(30), true, 15, true, Some(throttled_at(15))),
            (throttled_at(30), true, 30, true, None),
        ];
        for &(last, enabled, cap, discharging, expected) in cases {
            let got = next_power_state(last, &power_save(enabled, cap), discharging);
            assert_eq!(
                got, expected,
                "last={last:?} enabled={enabled} cap={cap} discharging={discharging}"
            );
        }
    }
}

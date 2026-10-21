//! Configuration for cornice.
//!
//! The file lives at `~/.config/cornice/config.toml` by default (the XDG
//! config directory on Linux), or wherever `--config` points. If it doesn't
//! exist it is created on first run as a fully commented-out dump of every
//! option, so the file is always a complete, self-documenting template for the
//! version of cornice that wrote it.
//!
//! Unknown keys are a hard error rather than a silent no-op: a typo in a
//! config that quietly does nothing is worse than a config that refuses to
//! start.
//!
//! What happens *around* the file — finding it, creating it, watching it,
//! re-reading it, surviving a bad edit — is [`cornice_config`]'s job, not this
//! module's. What's here is what's specific to cornice: the options, and the
//! rules about which combinations of them can't be rendered.

mod color;
pub(crate) mod cube3d;

pub use color::Color;
pub use cube3d::Cube3dConfig;
pub use cornice_config::ReloadConfig;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// The built-in font search list used when no explicit `font_path` is
/// configured. Ordered by preference.
pub const FONT_PATHS: &[&str] = &[
    "/usr/share/fonts/TTF/TerminusTTF.ttf",
    "/usr/share/fonts/TTF/TerminusTTF-Bold.ttf",
    "/usr/share/fonts/TTF/TerminusTTF-Bold-Italic.ttf",
    "/usr/share/fonts/TTF/TerminusTTF-Italic.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
];

/// The built-in waterfall (rain) font path used by `cube3d` when no explicit
/// `waterfall_font_path` is configured.
pub const WATERFALL_FONT_PATH: &str = "/usr/share/fonts/TTF/matrix code nfi.ttf";

/// Every wallpaper source this build knows about. `source` in the config has to
/// name one of these.
pub const SOURCES: &[&str] = &["cube3d"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Which built-in wallpaper source to use. One of [`SOURCES`].
    #[serde(default = "default_source")]
    pub source: String,

    /// Upper bound on how many times per second the wallpaper redraws.
    /// `0` (the default) means no cap: frames are pushed as fast as the
    /// compositor will take them, which is the monitor's refresh rate.
    /// Capping below that is a straight CPU saving on high-refresh displays,
    /// because rendering is redone from scratch every frame. The rain falls at
    /// 180-440 px/s, so it reads the same at 60fps as at 144fps.
    #[serde(default)]
    pub fps_cap: u32,

    /// What to dial back while the machine is running unplugged. See
    /// [`PowerSaveConfig`].
    #[serde(default)]
    pub power_save: PowerSaveConfig,

    /// Tuning for the `cube3d` source. Ignored by any other source, so the
    /// settings survive switching `source` back and forth.
    #[serde(default)]
    pub cube3d: Cube3dConfig,

    /// How the config is re-read while running. See [`ReloadConfig`].
    #[serde(default)]
    pub reload: ReloadConfig,
}

/// Behaviour while the machine is running on battery: a laptop (or any box
/// with a battery) that is not plugged in throttles the redraw rate and, for
/// the Matrix rain, thins the rain out.
///
/// The backend resolves this against the battery (see
/// [`PowerSaveConfig::resolve`]) and hands the source a [`PowerState`]; the
/// source decides *what* to scale rather than knowing about `[power_save]` or
/// sysfs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerSaveConfig {
    /// Master switch. When false, `cornice` ignores the battery entirely and
    /// always runs at full speed.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Frame rate ceiling while on battery. `0` means "don't cap on battery",
    /// leaving `fps_cap` (or the display's refresh rate) in charge.
    #[serde(default = "default_battery_fps")]
    pub fps_cap: u32,

    /// Divide the rain's column count by this while on battery. The rain hands
    /// out screen columns round-robin, so keeping every Nth wall thins it
    /// evenly across the whole screen instead of punching holes in it.
    /// `1` disables the thinning.
    #[serde(default = "default_battery_rain_divisor")]
    pub rain_divisor: u32,
}

impl PowerSaveConfig {
    /// Resolve against whether a battery is discharging.
    ///
    /// `enabled` is folded in here rather than left to the caller: turning it off
    /// has to be able to *stop* power saving on a machine that is still
    /// unplugged, so the switch cannot be treated as "only affects future
    /// transitions".
    ///
    /// Not throttling resolves to [`PowerState::default`] and nothing else, so
    /// the two numbers in it are absent rather than merely ignored. That makes
    /// "not throttling" a fixed point: retuning the battery cap while plugged in
    /// resolves to the same state it already had, so it doesn't look like a
    /// change the source needs telling about. It also makes [`PowerState::default`]
    /// a true starting state rather than one that differs from any real reading.
    ///
    /// The backend resolves on every check and compares against what the source
    /// was last told, so *any* change that matters — the switch, or the numbers
    /// while the machine is still unplugged — reaches the source.
    pub fn resolve(&self, discharging: bool) -> PowerState {
        if self.enabled && discharging {
            PowerState {
                throttling: true,
                fps_cap: self.fps_cap,
                density_divisor: self.rain_divisor,
            }
        } else {
            PowerState::default()
        }
    }
}

/// How hard to scale back, resolved against the machine's actual battery.
///
/// This is what a `FrameSource` is handed: the backend resolves `[power_save]`
/// and the battery together and passes the answer down, so a source never reads
/// the table or the sysfs itself, and every source is treated the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerState {
    /// Whether to scale back at all. While false this is exactly
    /// [`PowerState::default`] and the two numbers below are absent.
    pub throttling: bool,
    /// Frame-rate ceiling while throttling. `0` means "don't cap on battery",
    /// leaving the source's own limit in charge.
    pub fps_cap: u32,
    /// Scale animated content down to every Nth unit while throttling; `1`
    /// disables thinning. `rain_divisor` in the config is one interpretation of
    /// this — divide the rain's column count by it — named generically here so
    /// the trait contract isn't specific to one source's content.
    pub density_divisor: u32,
}

impl Default for PowerState {
    /// Not throttling, and nothing to scale: the state a machine on mains, or on
    /// a desktop with no battery, is always in.
    fn default() -> Self {
        PowerState {
            throttling: false,
            fps_cap: 0,
            density_divisor: 1,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_battery_fps() -> u32 {
    30
}

fn default_battery_rain_divisor() -> u32 {
    4
}

fn default_source() -> String {
    "cube3d".to_string()
}

impl Default for PowerSaveConfig {
    fn default() -> Self {
        PowerSaveConfig {
            enabled: default_true(),
            fps_cap: default_battery_fps(),
            rain_divisor: default_battery_rain_divisor(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            source: default_source(),
            fps_cap: 0,
            power_save: PowerSaveConfig::default(),
            cube3d: Cube3dConfig::default(),
            reload: ReloadConfig::default(),
        }
    }
}

impl cornice_config::Config for Config {
    /// Reject a config that parses but can't be rendered from — an unknown
    /// source, inverted ranges, empty glyph sets. All of these would otherwise
    /// surface as a blank wallpaper or a division by zero, hours later, with no
    /// clue which key caused it.
    fn validate(&self) -> Result<()> {
        if !SOURCES.contains(&self.source.as_str()) {
            bail!(
                "source = {:?} is not a known source (have: {})",
                self.source,
                SOURCES.join(", ")
            );
        }
        self.cube3d.validate()?;
        self.reload.validate()?;
        Ok(())
    }

    fn reload(&self) -> &ReloadConfig {
        &self.reload
    }
}

/// Serialize an `f32` at a precision a human can read.
///
/// The TOML serializer widens `f32` to `f64` before formatting, so a `0.12`
/// default comes back out as `0.11999999731779098` — a config template nobody
/// wants to edit. Rounding to six decimals keeps the printed value identical for
/// every magnitude in this config while staying exact in `f64`, so it still
/// parses straight back to the same `f32`.
pub fn ser_float<S: serde::Serializer>(v: &f32, s: S) -> Result<S::Ok, S::Error> {
    const SCALE: f64 = 1e6;
    s.serialize_f64((*v as f64 * SCALE).round() / SCALE)
}

pub const CONFIG_HEADER: &str = "\
# Cornice configuration
#
# Every option is shown with its default value, commented out: uncomment a line
# to change it. Keys cornice does not recognise are an error, so a typo fails
# loudly instead of being silently ignored.
";

/// The commented template a missing config file is created from, and what
/// `--print-default-config` prints.
pub fn default_config_text() -> String {
    cornice_config::commented_template::<Config>(CONFIG_HEADER)
}

#[cfg(test)]
mod tests {
    use super::*;
    // Anonymous import: brings `validate` into scope without colliding with the
    // `Config` struct of the same name.
    use cornice_config::Config as _;

    #[test]
    fn empty_config_uses_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.source, "cube3d");
        assert_eq!(config.fps_cap, 0);
        assert!(config.cube3d.font_path.is_none());
        assert!(config.cube3d.waterfall_font_path.is_none());
        assert_eq!(config.cube3d.ramp, "@$#%8&oa*+=-:. ");
    }

    #[test]
    fn power_save_defaults_to_throttling_on_battery() {
        let config: Config = toml::from_str("").unwrap();
        let ps = &config.power_save;
        assert!(ps.enabled);
        assert_eq!(ps.fps_cap, 30);
        assert_eq!(ps.rain_divisor, 4);
    }

    #[test]
    fn power_save_is_overridable() {
        let toml = r#"
[power_save]
enabled = false
fps_cap = 24
rain_divisor = 8
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let ps = &config.power_save;
        assert!(!ps.enabled);
        assert_eq!(ps.fps_cap, 24);
        assert_eq!(ps.rain_divisor, 8);
    }

    /// Resolving has to be a fixed point when not throttling, because the backend
    /// compares each resolution against the last to decide whether the source
    /// needs telling. If "not throttling" carried the configured numbers, then
    /// retuning `[power_save]` while plugged in would look like a change and wake
    /// the source for nothing — and the starting value would differ from any real
    /// reading, firing one spurious wakeup at startup.
    #[test]
    fn not_throttling_resolves_to_the_default_however_the_config_is_set() {
        for ps in [
            PowerSaveConfig::default(),
            PowerSaveConfig {
                enabled: true,
                fps_cap: 15,
                rain_divisor: 8,
            },
            PowerSaveConfig {
                enabled: false,
                fps_cap: 30,
                rain_divisor: 4,
            },
        ] {
            // On mains, whatever the numbers say.
            assert_eq!(ps.resolve(false), PowerState::default(), "on mains: {ps:?}");
            // Unplugged, the switch alone decides.
            assert_eq!(ps.resolve(true).throttling, ps.enabled, "unplugged: {ps:?}");
            if !ps.enabled {
                assert_eq!(ps.resolve(true), PowerState::default(), "{ps:?}");
            }
        }
    }

    /// Unplugged, the numbers come through — that is the whole point of
    /// resolving, and what a retune while on battery has to be able to change.
    #[test]
    fn resolving_while_unplugged_carries_the_configured_budget() {
        let ps = PowerSaveConfig {
            enabled: true,
            fps_cap: 15,
            rain_divisor: 8,
        };
        assert_eq!(
            ps.resolve(true),
            PowerState {
                throttling: true,
                fps_cap: 15,
                density_divisor: 8,
            }
        );
        // And a retune is a different state, so it reaches the source.
        assert_ne!(ps.resolve(true), PowerSaveConfig::default().resolve(true));
    }

    /// The generated template is only useful if it round-trips: every key in it
    /// must be a real key, and uncommenting everything must give back the
    /// defaults rather than a different config.
    #[test]
    fn generated_template_is_valid_and_complete() {
        let text = default_config_text();
        // It parses as-is: the header and the commented body are both comments.
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed, Config::default());

        // Uncommenting every line gives back exactly the defaults.
        let live: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .map(|l| format!("{l}\n"))
            .collect();
        let uncommented: Config = toml::from_str(&live).unwrap();
        assert_eq!(uncommented, Config::default());
    }

    /// A typo has to be an error, not a silently-ignored key. This is the whole
    /// reason `deny_unknown_fields` is on every config struct.
    #[test]
    fn unknown_keys_are_rejected_with_the_offending_name() {
        let err = toml::from_str::<Config>("[cube3d]\nrain_colour = \"#fff\"\n").unwrap_err();
        let err = err.to_string();
        assert!(err.contains("rain_colour"), "{err}");

        let err = toml::from_str::<Config>("[cube3d.rain]\nnope = 1\n").unwrap_err();
        let err = err.to_string();
        assert!(err.contains("nope"), "{err}");
    }

    #[test]
    fn validate_rejects_an_unknown_source() {
        let config = Config {
            source: "screensaver".to_string(),
            ..Default::default()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("screensaver") && err.contains("cube3d"),
            "{err}"
        );
    }
}

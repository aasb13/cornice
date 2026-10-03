//! Configuration for rustwall.
//!
//! Config lives at `~/.config/rustwall/config.toml` by default (the XDG
//! config directory on Linux), or wherever `--config` points. If the file
//! doesn't exist it is created on first run as a fully commented-out dump of
//! every option, so the file is always a complete, self-documenting template
//! for the version of rustwall that wrote it.
//!
//! Unknown keys are a hard error rather than a silent no-op: a typo in a
//! config that quietly does nothing is worse than a config that refuses to
//! start.

mod color;
mod cube3d;

pub use color::Color;
pub use cube3d::Cube3dConfig;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
}

/// Behaviour while the machine is running on battery: a laptop (or any box
/// with a battery) that is not plugged in throttles the redraw rate and, for
/// the Matrix rain, thins the rain out. Applied by `rustwall` itself, not by
/// the source, so every `FrameSource` benefits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerSaveConfig {
    /// Master switch. When false, `rustwall` ignores the battery entirely and
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
        }
    }
}

impl Config {
    /// Reject a config that parses but can't be rendered from — an unknown
    /// source, inverted ranges, empty glyph sets. All of these would otherwise
    /// surface as a blank wallpaper or a division by zero, hours later, with no
    /// clue which key caused it.
    pub fn validate(&self) -> Result<()> {
        if !SOURCES.contains(&self.source.as_str()) {
            bail!(
                "source = {:?} is not a known source (have: {})",
                self.source,
                SOURCES.join(", ")
            );
        }
        self.cube3d.validate()?;
        Ok(())
    }

    /// Absolute path of the config file, or the one `--config` named.
    pub fn path(explicit: Option<&Path>) -> Result<PathBuf> {
        if let Some(p) = explicit {
            return Ok(p.to_path_buf());
        }
        let dir = dirs::config_dir().context("could not determine config directory")?;
        Ok(dir.join("rustwall").join("config.toml"))
    }

    /// Load and validate the config, creating a commented template if there
    /// isn't one yet.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = Self::path(explicit)?;

        let config = match std::fs::read_to_string(&path) {
            Ok(contents) => {
                let config: Config = toml::from_str(&contents)
                    .with_context(|| format!("failed to parse config file {}", path.display()))?;
                log::info!("loaded config from {}", path.display());
                config
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!("no config found at {}, creating default", path.display());
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).context("failed to create config directory")?;
                }
                std::fs::write(&path, default_config_text())
                    .context("failed to write default config file")?;
                Config::default()
            }
            Err(e) => anyhow::bail!("failed to read config file {}: {e}", path.display()),
        };

        config.validate()?;
        Ok(config)
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

const CONFIG_HEADER: &str = "\
# Rustwall configuration
#
# Every option is shown with its default value, commented out: uncomment a line
# to change it. Keys rustwall does not recognise are an error, so a typo fails
# loudly instead of being silently ignored.
";

/// A ready-to-write config file: the header, then every option dumped from the
/// `Default` impls and commented out.
///
/// Generating it from `Default` rather than hand-writing it means the template
/// cannot drift out of sync with the code — a new option shows up here the
/// moment someone adds it, and a renamed one stops being advertised at the same
/// moment.
pub fn default_config_text() -> String {
    let dumped = toml::to_string_pretty(&Config::default()).expect("Default must always serialize");
    let mut out = String::from(CONFIG_HEADER);
    for line in dumped.lines() {
        if line.trim().is_empty() {
            out.push('\n');
        } else {
            out.push_str("# ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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

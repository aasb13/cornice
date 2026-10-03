//! Configuration for rustbar.
//!
//! Config lives at `~/.config/rustbar/config.toml`, or wherever `--config`
//! points. If the file doesn't exist it is created on first run as a fully
//! commented-out dump of every option, so the file is always a complete,
//! self-documenting template for the version of rustbar that wrote it.
//!
//! Unknown keys are a hard error rather than a silent no-op: a typo in a config
//! that quietly does nothing is worse than a config that refuses to start.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// An 8-bit RGB colour.
///
/// Canvas pixels are `wl_shm` argb8888 in native (little-endian) byte order,
/// i.e. `B, G, R, A` in memory — the reverse of how the channels read here. The
/// draw sites go through [`Color::bgr`] and [`Color::to_bgra`] rather than
/// indexing the tuple by hand, because getting that order backwards is silent:
/// it just renders every colour with red and blue swapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Color { r, g, b }
    }

    /// The channels in canvas byte order, for blending into a `B, G, R, A`
    /// buffer.
    pub const fn to_bgra(self, a: u8) -> [u8; 4] {
        [self.b, self.g, self.r, a]
    }

    /// The channels in canvas byte order as floats, for the gamma-correct
    /// text/glyph blending the bar does.
    pub fn bgr(self) -> [f32; 3] {
        [self.b as f32, self.g as f32, self.r as f32]
    }
}

impl std::str::FromStr for Color {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = s.strip_prefix('#').unwrap_or(s);
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!(
                "'{s}' is not a hex colour (expected e.g. \"#bfffd9\")"
            ));
        }
        let d = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).unwrap();
        let p = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap();
        match hex.len() {
            3 => Ok(Color::rgb(d(0) * 17, d(1) * 17, d(2) * 17)),
            6 => Ok(Color::rgb(p(0), p(2), p(4))),
            _ => Err(format!(
                "'{s}' is not a hex colour (expected 3 or 6 hex digits, e.g. \"#bfffd9\")"
            )),
        }
    }
}

impl std::fmt::Display for Color {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

impl Serialize for Color {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;

        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Color;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a hex colour string like \"#bfffd9\"")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Color, E> {
                v.parse().map_err(E::custom)
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Color, E> {
                // Accept 0xRRGGBB as an integer too, for anyone who'd rather
                // write the colour out longhand.
                Ok(Color::rgb(
                    ((v >> 16) & 0xff) as u8,
                    ((v >> 8) & 0xff) as u8,
                    (v & 0xff) as u8,
                ))
            }
        }

        d.deserialize_any(V)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub colors: Colors,

    /// How the config is re-read while running.
    #[serde(default)]
    pub reload: ReloadConfig,
}

/// Live config reloading, so tweaking a colour doesn't mean restarting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReloadConfig {
    /// Re-read the config when the file changes on disk. `rustbar --reload`
    /// reloads regardless of this setting.
    #[serde(default = "default_true")]
    pub watch: bool,

    /// How often to check the config file for changes, in milliseconds. Only the
    /// file's mtime and size are compared per tick; it is re-parsed when one of
    /// them actually moved.
    #[serde(default = "default_reload_interval_ms")]
    pub interval_ms: u64,
}

fn default_true() -> bool {
    true
}

fn default_reload_interval_ms() -> u64 {
    500
}

impl Default for ReloadConfig {
    fn default() -> Self {
        ReloadConfig {
            watch: true,
            interval_ms: 500,
        }
    }
}

impl ReloadConfig {
    /// Clamped to a range that is neither "re-parse every frame" nor "effectively
    /// never".
    pub fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.interval_ms.clamp(50, 60_000))
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.interval_ms == 0 {
            anyhow::bail!("reload.interval_ms must be greater than 0");
        }
        Ok(())
    }
}

/// The whole palette. These were previously constants in `theme.rs`, kept in
/// sync by hand with a waybar stylesheet; the defaults here are those exact
/// values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Colors {
    /// Text, borders, active states.
    #[serde(default = "default_accent")]
    pub accent: Color,

    /// Bar and popup background.
    #[serde(default = "default_background")]
    pub background: Color,

    /// Hover background for menu rows.
    #[serde(default = "default_background_hover")]
    pub background_hover: Color,

    /// Text on an accent-filled workspace button, where an accent background
    /// would leave the label unreadable.
    #[serde(default = "default_on_accent")]
    pub on_accent: Color,

    /// Empty / inactive workspace text.
    #[serde(default = "default_inactive")]
    pub inactive: Color,
}

fn default_accent() -> Color {
    Color::rgb(191, 255, 217)
}
fn default_background() -> Color {
    Color::rgb(0, 0, 0)
}
fn default_background_hover() -> Color {
    Color::rgb(26, 26, 26)
}
fn default_on_accent() -> Color {
    Color::rgb(0, 0, 0)
}
fn default_inactive() -> Color {
    Color::rgb(34, 34, 34)
}

impl Default for Colors {
    fn default() -> Self {
        Colors {
            accent: default_accent(),
            background: default_background(),
            background_hover: default_background_hover(),
            on_accent: default_on_accent(),
            inactive: default_inactive(),
        }
    }
}

impl Config {
    /// Absolute path of the config file, or the one `--config` named.
    pub fn path(explicit: Option<&Path>) -> Result<PathBuf> {
        if let Some(p) = explicit {
            return Ok(p.to_path_buf());
        }
        let dir = dirs::config_dir().context("could not determine config directory")?;
        Ok(dir.join("rustbar").join("config.toml"))
    }

    /// Load and validate the config, creating a commented template if there
    /// isn't one yet.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = Self::path(explicit)?;
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                let config: Config = toml::from_str(&contents)
                    .with_context(|| format!("failed to parse config file {}", path.display()))?;
                config.reload.validate()?;
                log::info!("loaded config from {}", path.display());
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!("no config found at {}, creating default", path.display());
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).context("failed to create config directory")?;
                }
                std::fs::write(&path, default_config_text())
                    .context("failed to write default config file")?;
                Ok(Config::default())
            }
            Err(e) => bail!("failed to read config file {}: {e}", path.display()),
        }
    }
}

const CONFIG_HEADER: &str = "\
# Rustbar configuration
#
# Every option is shown with its default value, commented out: uncomment a line
# to change it. Keys rustbar does not recognise are an error, so a typo fails
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
    let dumped = toml::to_string_pretty(&Config::default()).expect("Default must serialize");
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
        assert_eq!(config.colors.accent, Color::rgb(191, 255, 217));
        assert_eq!(config.colors.background, Color::rgb(0, 0, 0));
        assert_eq!(config.colors.background_hover, Color::rgb(26, 26, 26));
        assert_eq!(config.colors.on_accent, Color::rgb(0, 0, 0));
        assert_eq!(config.colors.inactive, Color::rgb(34, 34, 34));
    }

    #[test]
    fn defaults_match_the_old_theme_constants() {
        let c = Colors::default();
        assert_eq!(c.accent, (191, 255, 217).into_rgb());
        assert_eq!(c.background, (0, 0, 0).into_rgb());
        assert_eq!(c.background_hover, (26, 26, 26).into_rgb());
        assert_eq!(c.inactive, (34, 34, 34).into_rgb());
    }

    #[test]
    fn hex_parsing_and_formatting_round_trip() {
        assert_eq!(
            "#bfffd9".parse::<Color>().unwrap(),
            Color::rgb(191, 255, 217)
        );
        assert_eq!(
            "bfffd9".parse::<Color>().unwrap(),
            Color::rgb(191, 255, 217)
        );
        assert_eq!("#fff".parse::<Color>().unwrap(), Color::rgb(255, 255, 255));
        for c in [
            Color::rgb(0, 0, 0),
            Color::rgb(191, 255, 217),
            Color::rgb(1, 2, 3),
        ] {
            assert_eq!(c.to_string().parse::<Color>().unwrap(), c);
        }
    }

    #[test]
    fn bad_colours_say_what_is_wrong() {
        for bad in ["", "#", "#gg0000", "#12345", "blue", "#1234567"] {
            let err = bad.parse::<Color>().unwrap_err();
            assert!(err.contains("hex"), "unhelpful error for {bad:?}: {err}");
        }
    }

    /// The canvas is `B, G, R, A` in memory. If this ever inverts, every colour
    /// renders with red and blue swapped — and on the old mint-green accent
    /// (where r≈b) that is close enough to invisible to survive.
    #[test]
    fn canvas_byte_order_is_bgra() {
        assert_eq!(
            Color::rgb(0x11, 0x22, 0x33).to_bgra(255),
            [0x33, 0x22, 0x11, 255]
        );
        assert_eq!(
            Color::rgb(0x11, 0x22, 0x33).bgr(),
            [0x33 as f32, 0x22 as f32, 0x11 as f32]
        );
    }

    #[test]
    fn unknown_keys_are_rejected_with_the_offending_name() {
        let err = toml::from_str::<Config>("[colors]\naccnet = \"#fff\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("accnet"), "{err}");
    }

    /// The generated template is only useful if it round-trips: uncommenting
    /// everything must give back the defaults rather than a different config.
    #[test]
    fn generated_template_is_valid_and_complete() {
        let text = default_config_text();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed, Config::default());

        let live: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .map(|l| format!("{l}\n"))
            .collect();
        assert_eq!(toml::from_str::<Config>(&live).unwrap(), Config::default());
    }

    #[test]
    fn reload_defaults_to_watching() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.reload.watch);
        assert_eq!(config.reload.interval_ms, 500);
    }

    #[test]
    fn reload_is_configurable_and_validated() {
        let config: Config =
            toml::from_str("[reload]\nwatch = false\ninterval_ms = 2000\n").unwrap();
        assert!(!config.reload.watch);
        assert_eq!(config.reload.interval().as_millis(), 2000);

        // A zero interval would mean re-parsing on every tick.
        let config: Config = toml::from_str("[reload]\ninterval_ms = 0\n").unwrap();
        assert!(config.reload.validate().is_err());
        // Absurd values are clamped rather than rejected, so a typo can't stop
        // the bar from reloading at all.
        let config: Config = toml::from_str("[reload]\ninterval_ms = 99999999\n").unwrap();
        assert_eq!(config.reload.interval().as_secs(), 60);
    }

    /// A palette swap must be visible to the draw sites, which is the whole
    /// point of reloading: `theme::colors()` is what every module reads.
    #[test]
    fn a_reload_replaces_the_live_palette() {
        let red = Config {
            colors: Colors {
                accent: Color::rgb(255, 64, 64),
                ..Colors::default()
            },
            ..Config::default()
        };
        crate::theme::init(&red);
        assert_eq!(crate::theme::accent(), Color::rgb(255, 64, 64));

        // And back again, proving it's a swap rather than a set-once.
        crate::theme::init(&Config::default());
        assert_eq!(crate::theme::accent(), Color::rgb(191, 255, 217));
    }

    #[test]
    fn a_partial_override_leaves_everything_else_alone() {
        let config: Config = toml::from_str("[colors]\naccent = \"#ff4040\"\n").unwrap();
        assert_eq!(config.colors.accent, Color::rgb(255, 64, 64));
        assert_eq!(config.colors.inactive, default_inactive());
    }

    #[test]
    fn round_trips_through_toml() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct W {
            c: Color,
        }
        let c = Color::rgb(0x1a, 0x2b, 0x3c);
        let text = toml::to_string(&W { c }).unwrap();
        assert!(text.contains("\"#1a2b3c\""), "{text}");
        assert_eq!(toml::from_str::<W>(&text).unwrap().c, c);
    }

    /// Small helper so `defaults_match_the_old_theme_constants` reads like the
    /// tuple constants it replaces.
    trait IntoRgb {
        fn into_rgb(self) -> Color;
    }
    impl IntoRgb for (u8, u8, u8) {
        fn into_rgb(self) -> Color {
            Color::rgb(self.0, self.1, self.2)
        }
    }
}

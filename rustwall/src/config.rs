//! Configuration for rustwall.
//!
//! Config lives at `~/.config/rustwall/config.toml` by default (the XDG
//! config directory on Linux). If the file doesn't exist it is created on
//! first run with every option commented out so defaults apply, giving the
//! user a self-documenting template to tweak.
//!
//! Everything a `FrameSource` needs to know comes from here — which source to
//! run and its parameters — so changing wallpapers no longer requires editing
//! `main.rs` or recompiling.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Wallpaper-specific configuration.
    #[serde(default)]
    pub wallpaper: WallpaperConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WallpaperConfig {
    /// Which built-in wallpaper source to use. Only `cube3d` is built in.
    #[serde(default = "default_source")]
    pub source: String,

    /// Optional explicit font file path for the `cube3d` source. If unset,
    /// the built-in `FONT_PATHS` search list is used.
    #[serde(default)]
    pub font_path: Option<String>,

    /// Optional explicit waterfall font path for the `cube3d` source's rain
    /// glyphs. If unset, the built-in `WATERFALL_FONT_PATH` is tried and falls
    /// back to the main font.
    #[serde(default)]
    pub waterfall_font_path: Option<String>,

    /// What to dial back while the machine is running unplugged. See
    /// [`PowerSaveConfig`].
    #[serde(default)]
    pub power_save: PowerSaveConfig,

    /// Upper bound on how many times per second the wallpaper redraws.
    /// `0` (the default) means no cap: frames are pushed as fast as the
    /// compositor will take them, which is the monitor's refresh rate.
    /// Capping below that is a straight CPU saving on high-refresh displays,
    /// because rendering is redone from scratch every frame. The rain falls at
    /// 180-440 px/s, so it reads the same at 60fps as at 144fps.
    #[serde(default)]
    pub fps_cap: u32,
}

/// Behaviour while the machine is running on battery: a laptop (or any box
/// with a battery) that is not plugged in throttles the redraw rate and, for
/// the Matrix rain, thins the rain out. Applied by `rustwall` itself, not by
/// the source, so every `FrameSource` benefits.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl Default for PowerSaveConfig {
    fn default() -> Self {
        PowerSaveConfig {
            enabled: default_true(),
            fps_cap: default_battery_fps(),
            rain_divisor: default_battery_rain_divisor(),
        }
    }
}

fn default_source() -> String {
    "cube3d".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            wallpaper: WallpaperConfig::default(),
        }
    }
}

impl Default for WallpaperConfig {
    fn default() -> Self {
        WallpaperConfig {
            source: default_source(),
            font_path: None,
            waterfall_font_path: None,
            fps_cap: 0,
            power_save: PowerSaveConfig::default(),
        }
    }
}

const DEFAULT_CONFIG_TEXT: &str = r#"# Rustwall configuration
# Location: ~/.config/rustwall/config.toml

[wallpaper]

# Which built-in wallpaper source to use.
# Options: cube3d
source = "cube3d"

# --- Font source (cube3d) ---
# Optional explicit font file path. If unset, a built-in font search
# list is used instead.
# font_path = "/usr/share/fonts/TTF/TerminusTTF.ttf"
# font_path = "/usr/share/fonts/TTF/DejaVuSansMono.ttf"
#
# Optional waterfall font for the cube3d source's rain glyphs.
# If unset, falls back to the main font.
# waterfall_font_path = "/usr/share/fonts/TTF/matrix code nfi.ttf"
#
# --- Performance ---
# Cap on redraws per second. 0 (the default) means no cap: the wallpaper
# redraws at your monitor's refresh rate. Setting e.g. 60 roughly halves CPU
# on a 144Hz display, and the rain looks the same.
# fps_cap = 60
#
# While running unplugged (a laptop on battery, not charging), the frame rate
# drops to `power_save.fps_cap` and the rain is thinned to one column in
# `power_save.rain_divisor`. Machines with no battery never enter this mode.
# [wallpaper.power_save]
# enabled = true
# fps_cap = 30
# rain_divisor = 4
"#;

/// Resolve the on-disk path to the config file:
/// `~/.config/rustwall/config.toml` (or whatever `$XDG_CONFIG_HOME`
/// points at).
pub fn config_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("could not determine config directory")?;
    Ok(dir.join("rustwall").join("config.toml"))
}

/// Load the config from `~/.config/rustwall/config.toml`.
///
/// If the file doesn't exist it is created with a commented-out default
/// template and the in-memory defaults are returned.
pub fn load() -> Result<Config> {
    let path = config_path()?;

    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            let config: Config = toml::from_str(&contents)
                .with_context(|| format!("failed to parse config file {}", path.display()))?;
            log::info!("loaded config from {}", path.display());
            Ok(config)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log::info!("no config found at {}, creating default", path.display());
            std::fs::create_dir_all(path.parent().unwrap())
                .context("failed to create config directory")?;
            std::fs::write(&path, DEFAULT_CONFIG_TEXT)
                .context("failed to write default config file")?;
            Ok(Config::default())
        }
        Err(e) => anyhow::bail!("failed to read config file {}: {e}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_uses_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.wallpaper.source, "cube3d");
        assert!(config.wallpaper.font_path.is_none());
        assert!(config.wallpaper.waterfall_font_path.is_none());
    }

    #[test]
    fn fps_cap_defaults_to_unlimited() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.wallpaper.fps_cap, 0);
    }

    #[test]
    fn power_save_defaults_to_throttling_on_battery() {
        let config: Config = toml::from_str("").unwrap();
        let ps = &config.wallpaper.power_save;
        assert!(ps.enabled);
        assert_eq!(ps.fps_cap, 30);
        assert_eq!(ps.rain_divisor, 4);
    }

    #[test]
    fn power_save_is_overridable() {
        let toml = r#"
[wallpaper.power_save]
enabled = false
fps_cap = 24
rain_divisor = 8
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let ps = &config.wallpaper.power_save;
        assert!(!ps.enabled);
        assert_eq!(ps.fps_cap, 24);
        assert_eq!(ps.rain_divisor, 8);
    }

    #[test]
    fn parse_cube3d_with_font_overrides() {
        let toml = r#"
[wallpaper]
source = "cube3d"
font_path = "/usr/share/fonts/TTF/DejaVuSansMono.ttf"
waterfall_font_path = "/custom/matrix.ttf"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.wallpaper.source, "cube3d");
        assert_eq!(
            config.wallpaper.font_path.as_deref(),
            Some("/usr/share/fonts/TTF/DejaVuSansMono.ttf")
        );
        assert_eq!(
            config.wallpaper.waterfall_font_path.as_deref(),
            Some("/custom/matrix.ttf")
        );
    }
}

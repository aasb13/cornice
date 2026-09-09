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
    fn parse_cube3d_with_font_overrides() {
        let toml = r#"
[wallpaper]
source = "cube3d"
font_path = "/usr/share/fonts/TTF/DejaVuSansMono.ttf"
waterfall_font_path = "/custom/matrix.ttf"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.wallpaper.source, "cube3d");
        assert_eq!(config.wallpaper.font_path.as_deref(), Some("/usr/share/fonts/TTF/DejaVuSansMono.ttf"));
        assert_eq!(config.wallpaper.waterfall_font_path.as_deref(), Some("/custom/matrix.ttf"));
    }
}

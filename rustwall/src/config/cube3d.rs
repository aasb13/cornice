//! Config for the `cube3d` source: everything about how the tesseract and the
//! rain look and move.
//!
//! Ranges are expressed as `{ min, max }` tables rather than bare numbers so
//! the "pick a random value in this span" knobs stay self-describing:
//!
//! ```toml
//! [cube3d.rain.speed]
//! min = 180.0
//! max = 440.0
//! ```

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::Color;

/// An inclusive `[min, max]` span a value is randomized within.
///
/// Both ends are required: a range is one knob, and guessing the other end from
/// a default would silently make `speed.min = 400` mean something different than
/// it reads.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct F32Range {
    #[serde(serialize_with = "crate::config::ser_float")]
    pub min: f32,
    #[serde(serialize_with = "crate::config::ser_float")]
    pub max: f32,
}

impl F32Range {
    pub const fn new(min: f32, max: f32) -> Self {
        F32Range { min, max }
    }

    fn validate(&self, what: &str) -> Result<()> {
        if self.min > self.max {
            bail!(
                "{what}: min ({}) is greater than max ({})",
                self.min,
                self.max
            );
        }
        if !self.min.is_finite() || !self.max.is_finite() {
            bail!("{what}: min/max must be finite numbers");
        }
        Ok(())
    }

    pub fn sample(&self, r: f32) -> f32 {
        self.min + (self.max - self.min) * r.clamp(0.0, 1.0)
    }

    /// Where `v` sits in the span, as 0..1. A zero-width span reports 0, which
    /// keeps callers that derive brightness from depth finite.
    pub fn norm(&self, v: f32) -> f32 {
        let span = self.max - self.min;
        if span.abs() < f32::EPSILON {
            0.0
        } else {
            ((v - self.min) / span).clamp(0.0, 1.0)
        }
    }
}

/// Integer counterpart of [`F32Range`], for counts.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct U32Range {
    pub min: u32,
    pub max: u32,
}

impl U32Range {
    pub const fn new(min: u32, max: u32) -> Self {
        U32Range { min, max }
    }

    fn validate(&self, what: &str) -> Result<()> {
        if self.min > self.max {
            bail!(
                "{what}: min ({}) is greater than max ({})",
                self.min,
                self.max
            );
        }
        Ok(())
    }

    /// Inclusive on both ends.
    pub fn sample(&self, r: u32) -> u32 {
        if self.max <= self.min {
            return self.min;
        }
        self.min + r % (self.max - self.min + 1)
    }
}

/// Angular velocity of each rotation, in radians per second.
///
/// The three `*w` planes are the 4D rotations that make the hypercube's shadow
/// turn inside out; the three plain ones are an ordinary 3D tumble on top.
/// Setting a value to 0 freezes that axis.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RotationConfig {
    #[serde(default = "default_xw", serialize_with = "crate::config::ser_float")]
    pub xw: f32,
    #[serde(default = "default_yw", serialize_with = "crate::config::ser_float")]
    pub yw: f32,
    #[serde(default = "default_zw", serialize_with = "crate::config::ser_float")]
    pub zw: f32,
    #[serde(default = "default_rx", serialize_with = "crate::config::ser_float")]
    pub x: f32,
    #[serde(default = "default_ry", serialize_with = "crate::config::ser_float")]
    pub y: f32,
    #[serde(default = "default_rz", serialize_with = "crate::config::ser_float")]
    pub z: f32,
}

impl Default for RotationConfig {
    fn default() -> Self {
        RotationConfig {
            xw: 0.9,
            yw: 0.6,
            zw: 0.35,
            x: 0.4,
            y: 0.7,
            z: 0.2,
        }
    }
}

fn default_xw() -> f32 {
    0.9
}
fn default_yw() -> f32 {
    0.6
}
fn default_zw() -> f32 {
    0.35
}
fn default_rx() -> f32 {
    0.4
}
fn default_ry() -> f32 {
    0.7
}
fn default_rz() -> f32 {
    0.2
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cube3dConfig {
    /// Optional explicit font file path. If unset, the built-in `FONT_PATHS`
    /// search list is used.
    #[serde(default)]
    pub font_path: Option<String>,

    /// Optional explicit waterfall font path for the rain glyphs. If unset, the
    /// built-in `WATERFALL_FONT_PATH` is tried and falls back to this font.
    #[serde(default)]
    pub waterfall_font_path: Option<String>,

    /// Font size, in pixels, of the tesseract's glyph cells. Everything else
    /// scales from this, including the rain's depth range.
    #[serde(serialize_with = "crate::config::ser_float")]
    #[serde(default = "default_font_size")]
    pub font_size: f32,

    /// Glyph pool for the tesseract's edges, brightest first. The character a
    /// cell gets is picked from this ramp by its depth, so a single-character
    /// ramp gives a flat fill and a long one gives smooth shading. Must contain
    /// `vertex_glyph`.
    #[serde(default = "default_ramp")]
    pub ramp: String,

    /// Glyph stamped at each of the sixteen vertices, where several edges
    /// converge. Falls back to the ramp's first character if it isn't in the
    /// ramp.
    #[serde(default = "default_vertex_glyph")]
    pub vertex_glyph: String,

    /// Background colour, filled before anything is drawn.
    #[serde(default = "default_background")]
    pub background: Color,

    /// Colour of the tesseract.
    #[serde(default = "default_tesseract_color")]
    pub tesseract_color: Color,

    /// How big the tesseract is, as a fraction of the shorter screen dimension.
    /// Bigger looks closer.
    #[serde(serialize_with = "crate::config::ser_float")]
    #[serde(default = "default_scale")]
    pub scale: f32,

    /// How far back the 4D observer sits. Larger is a flatter, more
    /// orthographic projection (less "the near cells are bigger" depth cue);
    /// smaller pushes the near cells toward the camera.
    #[serde(serialize_with = "crate::config::ser_float")]
    #[serde(default = "default_perspective")]
    pub perspective: f32,

    /// Per-axis rotation speed.
    #[serde(default)]
    pub rotation: RotationConfig,

    #[serde(default)]
    pub rain: RainConfig,
}

impl Cube3dConfig {
    /// The subset of settings that determines what gets rasterized.
    ///
    /// A live config reload compares this against the running config's key to
    /// decide whether the glyph caches and cell grid have to be rebuilt. Two
    /// configs with equal keys rasterize identically, so anything that changes
    /// *what is drawn* rather than *how it is coloured or moved* belongs here —
    /// and conversely, anything absent can be swapped in without re-tracing a
    /// single font outline.
    pub fn rasterization_key(&self) -> RasterizationKey<'_> {
        RasterizationKey {
            font_size: self.font_size.to_bits(),
            font_path: self.font_path.as_deref(),
            waterfall_font_path: self.waterfall_font_path.as_deref(),
            ramp: &self.ramp,
            vertex_glyph: &self.vertex_glyph,
            rain_glyphs: &self.rain.glyphs,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if !(4.0..=96.0).contains(&self.font_size) {
            bail!(
                "cube3d.font_size must be between 4 and 96 (got {})",
                self.font_size
            );
        }
        if self.ramp.is_empty() {
            bail!("cube3d.ramp must contain at least one character");
        }
        if self.vertex_glyph.is_empty() {
            bail!("cube3d.vertex_glyph must be a single character");
        }
        if !(0.01..=1.0).contains(&self.scale) {
            bail!(
                "cube3d.scale must be between 0.01 and 1.0 (got {})",
                self.scale
            );
        }
        if self.perspective < 1.05 {
            bail!(
                "cube3d.perspective must be at least 1.05 (the observer \
                 sits at that distance from the origin; closer makes the projection \
                 singular), got {}",
                self.perspective
            );
        }
        if !self.rotation.xw.is_finite()
            || !self.rotation.yw.is_finite()
            || !self.rotation.zw.is_finite()
            || !self.rotation.x.is_finite()
            || !self.rotation.y.is_finite()
            || !self.rotation.z.is_finite()
        {
            bail!("cube3d.rotation.* must be finite numbers");
        }
        self.rain.validate()
    }

    /// The vertex glyph's index into [`Self::ramp`], falling back to 0.
    pub fn vertex_index(&self) -> usize {
        let first = self.vertex_glyph.chars().next();
        self.ramp
            .chars()
            .position(|c| Some(c) == first)
            .unwrap_or(0)
    }

    pub fn ramp_bytes(&self) -> Vec<u8> {
        self.ramp.chars().map(|c| c as u32 as u8).collect()
    }
}

/// What [`RainConfig::population_key`] compares.
///
/// Floats are compared as their bit patterns rather than by value: `==` on `f32`
/// makes `-0.0 == 0.0` and `NaN != NaN`, both of which would silently report
/// "unchanged" and leave the rain in the old configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PopulationKey {
    wall_count: u32,
    speed: (u32, u32),
    length: (u32, u32),
    depth: (u32, u32),
    fade: (u32, u32),
    gradient_speed: (u32, u32),
    grow_interval: (u32, u32),
    mutate_interval: (u32, u32),
    morph_duration: u32,
    flash_duration: u32,
    flash_rate: u32,
    flash_brightness: u32,
    color: Color,
    filler_color: Color,
    flash_color: Color,
}

/// What [`Cube3dConfig::rasterization_key`] compares. Derived `PartialEq` over
/// only the fields that decide which glyphs get traced at which sizes, so
/// adding a new visual setting doesn't silently require a rebuild (and adding a
/// cached setting doesn't silently skip one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RasterizationKey<'a> {
    font_size: u32,
    font_path: Option<&'a str>,
    waterfall_font_path: Option<&'a str>,
    ramp: &'a str,
    vertex_glyph: &'a str,
    rain_glyphs: &'a str,
}

impl Default for Cube3dConfig {
    fn default() -> Self {
        Cube3dConfig {
            font_path: None,
            waterfall_font_path: None,
            font_size: 24.0,
            ramp: default_ramp(),
            vertex_glyph: default_vertex_glyph(),
            background: default_background(),
            tesseract_color: default_tesseract_color(),
            scale: 0.12,
            perspective: 4.0,
            rotation: RotationConfig::default(),
            rain: RainConfig::default(),
        }
    }
}

fn default_font_size() -> f32 {
    24.0
}
fn default_scale() -> f32 {
    0.12
}
fn default_perspective() -> f32 {
    4.0
}
fn default_ramp() -> String {
    "@$#%8&oa*+=-:. ".to_string()
}
fn default_vertex_glyph() -> String {
    "@".to_string()
}
fn default_background() -> Color {
    Color::rgb(0, 0, 0)
}
fn default_tesseract_color() -> Color {
    // (r, g, b) — a mint green, kept bright so it's always the visual subject.
    Color::rgb(191, 255, 217)
}

/// The Matrix-style rain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RainConfig {
    /// Master switch for the rain. Turning it off leaves just the tesseract.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// How many falling walls exist at once, spread across the screen. More
    /// walls means a denser, busier rain; the count is capped by how many
    /// distinct character columns the screen has.
    #[serde(default = "default_wall_count")]
    pub wall_count: u32,

    /// Characters the rain draws from. Latin letters, digits and symbols so
    /// every glyph exists in a plain monospace font (katakana would not).
    #[serde(default = "default_rain_glyphs")]
    pub glyphs: String,

    /// Colour at the leading (bottom) edge of a wall.
    #[serde(default = "default_rain_color")]
    pub color: Color,

    /// Colour at the top (oldest) end of a wall, where it fades out.
    #[serde(default = "default_rain_filler_color")]
    pub filler_color: Color,

    /// Colour a glyph flashes toward while it glints.
    #[serde(default = "default_rain_flash_color")]
    pub flash_color: Color,

    /// Falloff speed down a wall: `[min, max]` pixels per second.
    #[serde(default = "default_speed")]
    pub speed: F32Range,

    /// Wall length in glyphs.
    #[serde(default = "default_length")]
    pub length: U32Range,

    /// Depth from the viewer, as a multiplier on glyph size and brightness.
    /// Nearer walls are bigger and brighter. Must not contain 0.
    #[serde(default = "default_depth")]
    pub depth: F32Range,

    /// Per-glyph brightness falloff going up the wall. Lower values fade to
    /// nothing faster.
    #[serde(default = "default_fade")]
    pub fade: F32Range,

    /// How fast a wall's colour blends from `color` to `filler_color` with
    /// distance from its leading edge.
    #[serde(default = "default_gradient_speed")]
    pub gradient_speed: F32Range,

    /// Seconds between one symbol appearing at a wall's leading edge and the
    /// next, as a wall writes itself on entry.
    #[serde(default = "default_grow_interval")]
    pub grow_interval: F32Range,

    /// Seconds before a settled glyph quietly re-rolls to a different one.
    #[serde(default = "default_mutate_interval")]
    pub mutate_interval: F32Range,

    /// Crossfade time for a glyph changing to a new one, in seconds.
    #[serde(serialize_with = "crate::config::ser_float")]
    #[serde(default = "default_morph_duration")]
    pub morph_duration: f32,

    /// Seconds a flash glint lasts.
    #[serde(serialize_with = "crate::config::ser_float")]
    #[serde(default = "default_flash_duration")]
    pub flash_duration: f32,

    /// How often each glyph flashes, as glints per glyph per second. At 1 this
    /// would be constant, so the useful range is roughly 0..0.2.
    #[serde(serialize_with = "crate::config::ser_float")]
    #[serde(default = "default_flash_rate")]
    pub flash_rate: f32,

    /// Extra brightness added at the peak of a flash.
    #[serde(serialize_with = "crate::config::ser_float")]
    #[serde(default = "default_flash_brightness")]
    pub flash_brightness: f32,
}

impl RainConfig {
    /// The settings a wall's random parameters are drawn from.
    ///
    /// A live reload compares this to decide whether existing walls are still
    /// consistent with each other. Walls are randomized once, at spawn, from
    /// these values — so a change to any of them leaves survivors drawn under the
    /// old rules, mixing two densities or two speeds on one screen. Everything
    /// here is cheap to re-randomize (no font work), which is why these are kept
    /// separate from [`RainConfig`]'s glyph settings: changing those costs a full
    /// re-rasterization.
    pub fn population_key(&self) -> PopulationKey {
        PopulationKey {
            wall_count: self.wall_count,
            speed: (self.speed.min.to_bits(), self.speed.max.to_bits()),
            length: (self.length.min, self.length.max),
            depth: (self.depth.min.to_bits(), self.depth.max.to_bits()),
            fade: (self.fade.min.to_bits(), self.fade.max.to_bits()),
            gradient_speed: (
                self.gradient_speed.min.to_bits(),
                self.gradient_speed.max.to_bits(),
            ),
            grow_interval: (
                self.grow_interval.min.to_bits(),
                self.grow_interval.max.to_bits(),
            ),
            mutate_interval: (
                self.mutate_interval.min.to_bits(),
                self.mutate_interval.max.to_bits(),
            ),
            morph_duration: self.morph_duration.to_bits(),
            flash_duration: self.flash_duration.to_bits(),
            flash_rate: self.flash_rate.to_bits(),
            flash_brightness: self.flash_brightness.to_bits(),
            color: self.color,
            filler_color: self.filler_color,
            flash_color: self.flash_color,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.glyphs.is_empty() {
            bail!("cube3d.rain.glyphs must contain at least one character");
        }
        if self.wall_count == 0 {
            bail!("cube3d.rain.wall_count must be at least 1");
        }
        for (what, r) in [
            ("rain.speed", &self.speed),
            ("rain.depth", &self.depth),
            ("rain.fade", &self.fade),
            ("rain.gradient_speed", &self.gradient_speed),
            ("rain.grow_interval", &self.grow_interval),
            ("rain.mutate_interval", &self.mutate_interval),
        ] {
            r.validate(what)?;
        }
        self.length.validate("rain.length")?;
        if self.depth.min <= 0.0 || self.depth.max <= 0.0 {
            bail!(
                "cube3d.rain.depth must be entirely above 0 (it scales \
                 glyph size; 0 would collapse a wall to nothing), got {}..={}",
                self.depth.min,
                self.depth.max
            );
        }
        if !(0.0..=1.0).contains(&self.fade.min) || !(0.0..=1.0).contains(&self.fade.max) {
            bail!("cube3d.rain.fade must be within 0.0..=1.0");
        }
        for (what, v) in [
            ("rain.flash_rate", self.flash_rate),
            ("rain.flash_brightness", self.flash_brightness),
        ] {
            if !v.is_finite() || v < 0.0 {
                bail!("cube3d.{what} must be a non-negative number, got {v}");
            }
        }
        // These are divisors of dt every frame, so 0 would produce a division by
        // zero rather than a merely wrong-looking wallpaper.
        for (what, v) in [
            ("rain.morph_duration", self.morph_duration),
            ("rain.flash_duration", self.flash_duration),
        ] {
            if !v.is_finite() || v <= 0.0 {
                bail!("cube3d.{what} must be greater than 0, got {v}");
            }
        }
        Ok(())
    }
}

impl Default for RainConfig {
    fn default() -> Self {
        RainConfig {
            enabled: true,
            wall_count: default_wall_count(),
            glyphs: default_rain_glyphs(),
            color: default_rain_color(),
            filler_color: default_rain_filler_color(),
            flash_color: default_rain_flash_color(),
            speed: default_speed(),
            length: default_length(),
            depth: default_depth(),
            fade: default_fade(),
            gradient_speed: default_gradient_speed(),
            grow_interval: default_grow_interval(),
            mutate_interval: default_mutate_interval(),
            morph_duration: default_morph_duration(),
            flash_duration: default_flash_duration(),
            flash_rate: default_flash_rate(),
            flash_brightness: default_flash_brightness(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_wall_count() -> u32 {
    260
}
fn default_rain_glyphs() -> String {
    // ASCII 33-63, 90-95, 97-126.
    "!\"#$%&'()*+,-./0123456789:;<=>?@Z[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~".to_string()
}
fn default_rain_color() -> Color {
    // Same hue family as the tesseract, dimmer and desaturated, so the rain
    // reads as background texture rather than the subject.
    Color::rgb(160, 210, 185)
}
fn default_rain_filler_color() -> Color {
    Color::rgb(20, 100, 20)
}
fn default_rain_flash_color() -> Color {
    Color::rgb(220, 255, 230)
}
fn default_speed() -> F32Range {
    F32Range::new(180.0, 440.0)
}
fn default_length() -> U32Range {
    U32Range::new(12, 42)
}
fn default_depth() -> F32Range {
    F32Range::new(0.32, 1.55)
}
fn default_fade() -> F32Range {
    F32Range::new(0.62, 0.85)
}
fn default_gradient_speed() -> F32Range {
    F32Range::new(0.01, 0.05)
}
fn default_grow_interval() -> F32Range {
    F32Range::new(0.05, 0.14)
}
fn default_mutate_interval() -> F32Range {
    F32Range::new(3.0, 9.0)
}
fn default_morph_duration() -> f32 {
    0.35
}
fn default_flash_duration() -> f32 {
    0.45
}
fn default_flash_rate() -> f32 {
    0.04
}
fn default_flash_brightness() -> f32 {
    0.55
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    // Anonymous import: brings `validate` into scope without colliding with the
    // `Config` struct of the same name.
    use rwconfig::Config as _;

    fn parse(toml_text: &str) -> Config {
        toml::from_str::<Config>(toml_text).unwrap()
    }

    #[test]
    fn defaults_match_the_previous_hardcoded_constants() {
        let c = Cube3dConfig::default();
        assert_eq!(c.font_size, 24.0);
        assert_eq!(c.ramp, "@$#%8&oa*+=-:. ");
        assert_eq!(c.vertex_glyph, "@");
        assert_eq!(c.tesseract_color, Color::rgb(191, 255, 217));
        assert_eq!(c.background, Color::rgb(0, 0, 0));
        assert_eq!(c.scale, 0.12);
        assert_eq!(c.perspective, 4.0);
        assert_eq!(c.rotation, RotationConfig::default());

        let r = &c.rain;
        assert_eq!(r.wall_count, 260);
        assert_eq!(r.color, Color::rgb(160, 210, 185));
        assert_eq!(r.filler_color, Color::rgb(20, 100, 20));
        assert_eq!(r.flash_color, Color::rgb(220, 255, 230));
        assert_eq!(r.speed, F32Range::new(180.0, 440.0));
        assert_eq!(r.length, U32Range::new(12, 42));
        assert_eq!(r.depth, F32Range::new(0.32, 1.55));
        assert_eq!(r.fade, F32Range::new(0.62, 0.85));
        assert_eq!(r.gradient_speed, F32Range::new(0.01, 0.05));
        assert_eq!(r.grow_interval, F32Range::new(0.05, 0.14));
        assert_eq!(r.mutate_interval, F32Range::new(3.0, 9.0));
        assert_eq!(r.morph_duration, 0.35);
        assert_eq!(r.flash_duration, 0.45);
        assert_eq!(r.flash_rate, 0.04);
        assert_eq!(r.flash_brightness, 0.55);
    }

    #[test]
    fn colours_and_ranges_parse_from_toml() {
        let c = parse(
            r##"
[cube3d]
font_size = 12.0
scale = 0.2
background = "#101010"
tesseract_color = "#ff0000"
ramp = "#o. "

[cube3d.rotation]
xw = 2.0
z = 0.0

[cube3d.rain]
enabled = false
wall_count = 40
color = "#00ff00"
speed.min = 10.0
speed.max = 20.0
"##,
        );
        let cube = &c.cube3d;
        assert_eq!(cube.font_size, 12.0);
        assert_eq!(cube.scale, 0.2);
        assert_eq!(cube.background, Color::rgb(16, 16, 16));
        assert_eq!(cube.tesseract_color, Color::rgb(255, 0, 0));
        assert_eq!(cube.ramp, "#o. ");
        assert_eq!(cube.rotation.xw, 2.0);
        // The z axis was set explicitly (to zero, i.e. frozen).
        assert_eq!(cube.rotation.z, 0.0);
        // Axes that weren't mentioned keep their defaults.
        assert_eq!(cube.rotation.yw, 0.6);
        assert!(!cube.rain.enabled);
        assert_eq!(cube.rain.wall_count, 40);
        assert_eq!(cube.rain.color, Color::rgb(0, 255, 0));
        assert_eq!(cube.rain.speed, F32Range::new(10.0, 20.0));
        // Untouched ranges keep their defaults.
        assert_eq!(cube.rain.length, U32Range::new(12, 42));
    }

    #[test]
    fn a_partial_override_leaves_everything_else_alone() {
        let c = parse("[cube3d]\nfont_size = 8.0\n");
        assert_eq!(c.cube3d.font_size, 8.0);
        assert_eq!(
            c.cube3d.tesseract_color,
            Cube3dConfig::default().tesseract_color
        );
        assert_eq!(c.cube3d.rain, RainConfig::default());
    }

    #[test]
    fn ranges_reject_inverted_spans() {
        let c = parse("[cube3d.rain]\nspeed.min = 400.0\nspeed.max = 100.0\n");
        assert!(c.validate().unwrap_err().to_string().contains("speed"));

        let c = parse("[cube3d.rain]\nlength.min = 40\nlength.max = 10\n");
        assert!(c.validate().unwrap_err().to_string().contains("length"));
    }

    #[test]
    fn validation_rejects_the_values_that_would_break_rendering() {
        let cases = [
            ("[cube3d]\nfont_size = 0.0\n", "font_size"),
            ("[cube3d]\nscale = 4.0\n", "scale"),
            ("[cube3d]\nperspective = 1.0\n", "perspective"),
            ("[cube3d]\nramp = \"\"\n", "ramp"),
            ("[cube3d]\nvertex_glyph = \"\"\n", "vertex_glyph"),
            ("[cube3d.rain]\nglyphs = \"\"\n", "glyphs"),
            ("[cube3d.rain]\nwall_count = 0\n", "wall_count"),
            (
                "[cube3d.rain]\ndepth.min = -1.0\ndepth.max = 1.0\n",
                "depth",
            ),
            ("[cube3d.rain]\nfade.min = 0.5\nfade.max = 3.0\n", "fade"),
            ("[cube3d.rain]\nflash_rate = -1.0\n", "flash_rate"),
            ("[cube3d.rain]\nmorph_duration = 0.0\n", "morph_duration"),
            ("[cube3d.rotation]\nxw = 1e40\n", "rotation"),
        ];
        for (text, expected) in cases {
            let c = parse(text);
            let err = c.validate().unwrap_err().to_string();
            assert!(err.contains(expected), "for {text:?} got: {err}");
        }
    }

    #[test]
    fn the_vertex_glyph_resolves_against_the_ramp() {
        let mut c = Cube3dConfig {
            ramp: ".:-=+*#%@".to_string(),
            ..Default::default()
        };
        assert_eq!(c.vertex_index(), 8);
        // A ramp that doesn't contain the vertex glyph falls back to the first
        // entry rather than indexing out of bounds.
        c.vertex_glyph = "Q".to_string();
        assert_eq!(c.vertex_index(), 0);
    }

    #[test]
    fn sampling_stays_inside_the_span() {
        let r = F32Range::new(2.0, 5.0);
        assert_eq!(r.sample(0.0), 2.0);
        assert_eq!(r.sample(1.0), 5.0);
        assert!((r.sample(0.5) - 3.5).abs() < 1e-6);

        let r = U32Range::new(3, 5);
        assert_eq!(r.sample(0), 3);
        assert!((3..=5).contains(&r.sample(12345)));
        assert!((3..=5).contains(&r.sample(u32::MAX)));
        // A zero-width span must still return its only value rather than
        // dividing by zero.
        assert_eq!(U32Range::new(4, 4).sample(7), 4);
    }
}

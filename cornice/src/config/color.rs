//! Colours in config files.
//!
//! Written as hex (`#bfffd9`) because that is what every other wallpaper tool
//! uses and what a colour picker hands you, but kept as floats in memory
//! because the render path multiplies channels by a brightness every glyph.

use std::fmt;
use std::str::FromStr;

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self {
            r: r as f32,
            g: g as f32,
            b: b as f32,
        }
    }

    /// Channels scaled by `bright` and clamped to 0..=255, in RGB order.
    pub fn scaled(self, bright: f32) -> [u8; 3] {
        let c = |v: f32| (v * bright).clamp(0.0, 255.0) as u8;
        [c(self.r), c(self.g), c(self.b)]
    }

    pub fn lerp(self, to: Self, t: f32) -> Self {
        Self {
            r: self.r + (to.r - self.r) * t,
            g: self.g + (to.g - self.g) * t,
            b: self.b + (to.b - self.b) * t,
        }
    }

    pub fn is_black(&self) -> bool {
        self.r == 0.0 && self.g == 0.0 && self.b == 0.0
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [r, g, b] = self.scaled(1.0);
        write!(f, "#{r:02x}{g:02x}{b:02x}")
    }
}

impl FromStr for Color {
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

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;

        impl<'de> Visitor<'de> for V {
            type Value = Color;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a hex colour string like \"#bfffd9\", or [r, g, b] in 0..=255")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Color, E> {
                Color::from_str(v).map_err(E::custom)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Color, A::Error> {
                let mut ch = [0u8; 3];
                for slot in ch.iter_mut() {
                    *slot = seq
                        .next_element::<u16>()?
                        .ok_or_else(|| de::Error::invalid_length(1, &self))?
                        .min(255) as u8;
                }
                Ok(Color::rgb(ch[0], ch[1], ch[2]))
            }
        }

        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_in_either_length_with_or_without_hash() {
        assert_eq!(
            Color::from_str("#bfffd9").unwrap(),
            Color::rgb(191, 255, 217)
        );
        assert_eq!(
            Color::from_str("bfffd9").unwrap(),
            Color::rgb(191, 255, 217)
        );
        assert_eq!(Color::from_str("#fff").unwrap(), Color::rgb(255, 255, 255));
        assert_eq!(Color::from_str("000").unwrap(), Color::rgb(0, 0, 0));
    }

    #[test]
    fn rejects_nonsense_with_a_usable_message() {
        for bad in ["", "#", "#gg0000", "#12345", "blue", "#1234567"] {
            let err = Color::from_str(bad).unwrap_err();
            assert!(err.contains("hex"), "unhelpful error for {bad:?}: {err}");
        }
    }

    #[test]
    fn round_trips_through_toml() {
        // Serialization goes through a wrapper struct: TOML has no bare scalar
        // at the document root, so `toml::to_string(&color)` alone would fail
        // for reasons that have nothing to do with Color.
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct W {
            c: Color,
        }
        let c = Color::from_str("#1a2b3c").unwrap();
        let text = toml::to_string(&W { c }).unwrap();
        assert!(text.contains("\"#1a2b3c\""), "{text}");
        assert_eq!(toml::from_str::<W>(&text).unwrap().c, c);
    }

    #[test]
    fn also_accepts_an_rgb_triple() {
        #[derive(Deserialize)]
        struct W {
            c: Color,
        }
        let w: W = toml::from_str("c = [10, 20, 30]").unwrap();
        assert_eq!(w.c, Color::rgb(10, 20, 30));
    }

    #[test]
    fn scaling_clamps_like_the_blit_does() {
        assert_eq!(Color::rgb(200, 100, 50).scaled(0.5), [100, 50, 25]);
        assert_eq!(Color::rgb(200, 100, 50).scaled(4.0), [255, 255, 200]);
    }
}

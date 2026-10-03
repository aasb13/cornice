//! Drawing primitives shared by every bar module.
//!
//! The palette itself is not here — it comes from `~/.config/rustbar/config.toml`
//! (see [`crate::config`]) and is reached through the accessors below, so a
//! colour change is a config edit rather than a recompile. The defaults still
//! match the waybar style this bar was modelled on (`~/.config/waybar/style.css`).
//!
//! Canvas pixels are `wl_shm` argb8888 in native (little-endian) byte order,
//! i.e. `B, G, R, A` in memory — the reverse of how channels read in a config
//! file, so everything here converts via [`Color::to_bgra`] rather than
//! unpacking a tuple by hand.

use std::sync::RwLock;

use crate::config::{Color, Colors, Config};

/// The loaded palette.
///
/// A `RwLock` rather than a `OnceLock` because the config is re-read live: a
/// colour edit has to swap the palette while the bar is still running, which a
/// set-once cell cannot express. Reads are an uncontended atomic load plus a
/// `Colors` copy (five bytes' worth of `Color`), and there are only a handful of
/// draws per frame — irrelevant next to rasterizing glyphs.
static COLORS: RwLock<Option<Colors>> = RwLock::new(None);

/// Install or replace the palette. Called at startup, and again on every reload.
pub fn init(config: &Config) {
    let mut slot = match COLORS.write() {
        Ok(slot) => slot,
        // A poisoned lock means a draw panicked while holding the read guard. The
        // stored palette is still a valid value, so recover rather than taking
        // the whole bar down over it.
        Err(poisoned) => poisoned.into_inner(),
    };
    *slot = Some(config.colors.clone());
}

/// The active palette.
///
/// # Panics
/// If called before [`init`]. Every draw site is reached from
/// `wayland_backend::run`, which `main` only calls after loading the config, so
/// this cannot happen in practice.
pub fn colors() -> Colors {
    let slot = match COLORS.read() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    slot.as_ref()
        .expect("theme::init must run before any drawing")
        .clone()
}

/// Primary accent — text, borders, active states.
pub fn accent() -> Color {
    colors().accent
}

/// Bar / popup background.
pub fn bg() -> Color {
    colors().background
}

/// Hover background for menu rows.
pub fn bg_hover() -> Color {
    colors().background_hover
}

/// Text drawn on top of an accent-filled workspace button.
pub fn on_accent() -> Color {
    colors().on_accent
}

/// Empty / inactive workspace text.
pub fn inactive() -> Color {
    colors().inactive
}

/// Side margin — pixels of empty space left on the left and right edges of
/// the bar (waybar `config.jsonc` `margin-left` / `margin-right: 20`). Modules
/// start/end inside this inset instead of being smudged against the screen.
pub const SIDE_MARGIN: u32 = 20;

/// Top margin — waybar `config.jsonc` margin-top: 10. Gap between the bar
/// and the top of the output.
pub const TOP_MARGIN: i32 = 10;

/// Opaque rectangle fill. Bounds-checked against the canvas; out-of-range
/// coordinates are skipped. Shared by every module that draws a waybar-style
/// outlined box (clock, workspaces, …).
#[allow(clippy::too_many_arguments)]
pub(crate) fn fill_rect(
    canvas: &mut [u8],
    stride: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    color: Color,
    a: u8,
) {
    let px = color.to_bgra(a);
    for yy in y..y.saturating_add(h) {
        for xx in x..x.saturating_add(w) {
            let off = (yy * stride + xx * 4) as usize;
            if off + 3 < canvas.len() {
                canvas[off..off + 4].copy_from_slice(&px);
            }
        }
    }
}

/// A filled rectangle with a 1px border on all four edges — the waybar
/// outlined-box primitive (clock, workspaces, cava, tray, widget strip). The
/// border is drawn on the outer 1px of the rect.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_bordered_box(
    canvas: &mut [u8],
    stride: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    fill: Color,
    border: Color,
) {
    const B: u32 = 1;
    fill_rect(canvas, stride, x, y, w, h, fill, 255);
    // top & bottom edges
    fill_rect(canvas, stride, x, y, w, B, border, 255);
    fill_rect(
        canvas,
        stride,
        x,
        y + h.saturating_sub(B),
        w,
        B,
        border,
        255,
    );
    // left & right edges
    fill_rect(canvas, stride, x, y, B, h, border, 255);
    fill_rect(
        canvas,
        stride,
        x + w.saturating_sub(B),
        y,
        B,
        h,
        border,
        255,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rect and box primitives are the only places colours become canvas
    /// bytes, and they get the byte order backwards easily and silently.
    #[test]
    fn primitives_write_bgra() {
        let mut canvas = [0u8; 4 * 4];
        let color = Color::rgb(0x11, 0x22, 0x33);
        fill_rect(&mut canvas, 4, 1, 1, 1, 1, color, 255);
        // One 4x4 row of pixels; pixel (x=1, y=1) is bytes 8..12.
        assert_eq!(&canvas[8..12], &[0x33, 0x22, 0x11, 0xff]);
        assert!(
            canvas
                .iter()
                .enumerate()
                .all(|(i, &b)| b != 0 || !(8..12).contains(&i)),
            "the fill leaked outside its single pixel: {canvas:?}"
        );
    }

    /// Out-of-range writes are skipped rather than panicking; a colour change
    /// that made a box bigger must not take the bar down with it.
    #[test]
    fn primitives_clip_instead_of_panicking() {
        let mut canvas = [0u8; 4];
        fill_rect(
            &mut canvas,
            4,
            100,
            100,
            10,
            10,
            Color::rgb(255, 255, 255),
            255,
        );
        draw_bordered_box(
            &mut canvas,
            4,
            100,
            100,
            10,
            10,
            Color::rgb(255, 255, 255),
            Color::rgb(255, 255, 255),
        );
        assert_eq!(canvas, [0; 4]);
    }
}

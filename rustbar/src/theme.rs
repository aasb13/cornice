//! Shared color palette, kept in sync with the waybar style
//! (`~/.config/waybar/style.css`). The accent is the mint/teal green
//! `rgb(191, 255, 217)` (`#BFFFD9`).
//!
//! Canvas pixels are `wl_shm` argb8888 in native (little-endian) byte order,
//! i.e. `B, G, R, A` in memory — so the draw sites write `ACCENT.2` into the
//! blue byte, `ACCENT.1` into green, `ACCENT.0` into red.

/// Primary accent — text, borders, active states. (R, G, B) in 0..=255.
pub const ACCENT: (u8, u8, u8) = (191, 255, 217);

/// Bar / popup background (waybar `#000000`).
pub const BG: (u8, u8, u8) = (0, 0, 0);

/// Hover background for menu rows (waybar `#1a1a1a`).
pub const BG_HOVER: (u8, u8, u8) = (26, 26, 26);

/// Side margin — pixels of empty space left on the left and right edges of
/// the bar (waybar `config.jsonc` `margin-left` / `margin-right: 20`). Modules
/// start/end inside this inset instead of being smudged against the screen.
pub const SIDE_MARGIN: u32 = 20;

/// Top margin — waybar `config.jsonc` margin-top: 10. Gap between the bar
/// and the top of the output.
pub const TOP_MARGIN: i32 = 10;

/// Empty / inactive workspace text (waybar `#222222`).
pub const INACTIVE: (u8, u8, u8) = (34, 34, 34);

/// Opaque rectangle fill in BGRA (argb8888 little-endian). Bounds-checked
/// against the canvas; out-of-range coordinates are skipped. Shared by every
/// module that draws a waybar-style outlined box (clock, workspaces, …).
#[allow(clippy::too_many_arguments)]
pub(crate) fn fill_rect(
    canvas: &mut [u8],
    stride: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    (r, g, b): (u8, u8, u8),
    a: u8,
) {
    for yy in y..y.saturating_add(h) {
        for xx in x..x.saturating_add(w) {
            let off = (yy * stride + xx * 4) as usize;
            if off + 3 < canvas.len() {
                canvas[off] = b;
                canvas[off + 1] = g;
                canvas[off + 2] = r;
                canvas[off + 3] = a;
            }
        }
    }
}

/// An outlined box with NO interior fill — the waybar `#cava` primitive
/// (`background: transparent; border: 1px solid accent`). Only the 1px border
/// is drawn on all four edges; the interior is left untouched (transparent),
/// so the desktop/bar shows through the box while the block glyphs composite
/// over it. Used by the cava module.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_border_only(
    canvas: &mut [u8],
    stride: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    border: (u8, u8, u8),
) {
    const B: u32 = 1;
    // top & bottom edges
    fill_rect(canvas, stride, x, y, w, B, border, 255);
    fill_rect(canvas, stride, x, y + h.saturating_sub(B), w, B, border, 255);
    // left & right edges
    fill_rect(canvas, stride, x, y, B, h, border, 255);
    fill_rect(canvas, stride, x + w.saturating_sub(B), y, B, h, border, 255);
}

/// A filled rectangle with a 1px border on all four edges — the waybar
/// outlined-box primitive (clock, workspaces, tray, widget strip). The border
/// is drawn on the outer 1px of the rect.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_bordered_box(
    canvas: &mut [u8],
    stride: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    fill: (u8, u8, u8),
    border: (u8, u8, u8),
) {
    const B: u32 = 1;
    fill_rect(canvas, stride, x, y, w, h, fill, 255);
    // top & bottom edges
    fill_rect(canvas, stride, x, y, w, B, border, 255);
    fill_rect(canvas, stride, x, y + h.saturating_sub(B), w, B, border, 255);
    // left & right edges
    fill_rect(canvas, stride, x, y, B, h, border, 255);
    fill_rect(canvas, stride, x + w.saturating_sub(B), y, B, h, border, 255);
}
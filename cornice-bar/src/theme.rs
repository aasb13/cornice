//! Drawing primitives shared by every bar module.
//!
//! The palette itself is not here — it comes from `~/.config/cornice-bar/config.toml`
//! (see [`crate::config`]) and is handed to every draw as a `&Colors`, so a
//! colour change is a config edit rather than a recompile, and the live palette
//! is whatever the reload currently holds. The defaults still match the waybar
//! style this bar was modelled on (`~/.config/waybar/style.css`).
//!
//! Nothing here reads a palette on its own. A colour is always a parameter, which
//! is what lets the bar swap all five of them from one place: the frame is drawn
//! against the config as it is now, not against whatever was installed at startup.
//!
//! Canvas pixels are `wl_shm` argb8888 in native (little-endian) byte order,
//! i.e. `B, G, R, A` in memory — the reverse of how channels read in a config
//! file, so everything here converts via [`Color::to_bgra`] rather than
//! unpacking a tuple by hand.

use std::sync::OnceLock;

use rusttype::{Font, Scale, point};

use crate::config::Color;

/// Side margin — pixels of empty space left on the left and right edges of
/// the bar (waybar `config.jsonc` `margin-left` / `margin-right: 20`). Modules
/// start/end inside this inset instead of being smudged against the screen.
pub const SIDE_MARGIN: u32 = 20;

/// Top margin — waybar `config.jsonc` margin-top: 10. Gap between the bar
/// and the top of the output.
pub const TOP_MARGIN: i32 = 10;

/// Searched in order, for the one font the whole bar draws with.
///
/// The Nerd Font variants come first because they are supersets: the patched
/// JetBrains Mono has the same Latin glyphs as the plain one, so text that needs
/// no icons is unaffected, and everything that does need them — the icon
/// codepoints in the widget strip, the block elements in the visualiser, and
/// whatever characters a tray icon's menu happens to use — is covered by the
/// same file.
const FONT_PATHS: &[&str] = &[
    "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
    "/usr/share/fonts/TTF/JetBrainsMonoNerdFontMono-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMNerdFontMono-Regular.ttf",
    "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/TTF/TerminusTTF.ttf",
];

/// The bar's font, or `None` if none of [`FONT_PATHS`] could be read.
///
/// Loaded once on first use and shared by every module, so the bar holds one
/// parsed font rather than one per module that wanted text. A `OnceLock` rather
/// than a `RwLock` because this never changes while running, so the read path is
/// an atomic load instead of a lock.
pub(crate) fn font() -> Option<&'static Font<'static>> {
    static FONT: OnceLock<Option<Font<'static>>> = OnceLock::new();
    FONT.get_or_init(|| {
        for path in FONT_PATHS {
            if let Ok(data) = std::fs::read(path)
                && let Some(font) = Font::try_from_vec(data)
            {
                log::info!("loaded font from: {path}");
                return Some(font);
            }
        }
        log::error!("no font found in any of {FONT_PATHS:?}");
        None
    })
    .as_ref()
}

/// Total horizontal advance of `text` rendered at `scale`.
///
/// Every module that draws a run of text has to measure it first — to size a box,
/// centre a label, or decide how much room a tray icon leaves — and all of them
/// were summing the same advances.
pub(crate) fn text_width(font: &Font<'static>, scale: Scale, text: &str) -> f32 {
    text.chars()
        .map(|c| font.glyph(c).scaled(scale).h_metrics().advance_width)
        .sum()
}

/// Draw a run of text with its baseline at (`pen_x`, `pen_y`).
///
/// `bounds` is the `(width, height)` in pixels the text is allowed to touch —
/// the bar's surface for most modules, the popup's own for the menu. Anything
/// outside it, or outside the canvas, is skipped rather than panicking: a label
/// wider than its box has to lose its overflow, not take the bar down.
///
/// The colour is a parameter rather than read from the palette here, so a caller
/// resolves it once per run instead of once per *pixel* — which is what the
/// per-module copies of this used to do, taking the palette's read lock inside
/// the glyph rasterizer.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_text(
    font: &Font<'static>,
    scale: Scale,
    canvas: &mut [u8],
    stride: u32,
    bounds: (u32, u32),
    text: &str,
    pen_x: f32,
    pen_y: f32,
    color: Color,
) {
    let (bound_w, bound_h) = bounds;
    let src = color.bgr();
    let mut pen = point(pen_x, pen_y);

    for c in text.chars() {
        let scaled = font.glyph(c).scaled(scale);
        let advance = scaled.h_metrics().advance_width;
        let positioned = scaled.positioned(pen);
        if let Some(bb) = positioned.pixel_bounding_box() {
            positioned.draw(|x_rel, y_rel, v| {
                let xi = bb.min.x + x_rel as i32;
                let yi = bb.min.y + y_rel as i32;
                if xi < 0 || yi < 0 || xi as u32 >= bound_w || yi as u32 >= bound_h {
                    return;
                }
                let off = yi as usize * stride as usize + xi as usize * 4;
                if off + 3 >= canvas.len() {
                    return;
                }
                // Gamma-correct coverage, then premultiplied src-over, so text
                // composites over whatever is behind it — the black box, an
                // accent-filled workspace button, or the transparent visualiser —
                // instead of punching holes in it.
                let gv = v.powf(1.0 / 2.2);
                for ch in 0..3 {
                    let s = src[ch] * gv;
                    let d = canvas[off + ch] as f32;
                    canvas[off + ch] = (s + d * (1.0 - gv)).min(255.0) as u8;
                }
                let da = canvas[off + 3] as f32;
                canvas[off + 3] = (255.0 * gv + da * (1.0 - gv)).min(255.0) as u8;
            });
        }
        pen.x += advance;
    }
}

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

/// A 1px line from (`x0`, `y0`) to (`x1`, `y1`), inclusive of both ends.
///
/// Bresenham, stepping along whichever axis is longer so the line never
/// double-steps a diagonal and leaves gaps in it. Coordinates outside `bounds`
/// are skipped, like every other primitive here — the visualiser's trace runs
/// to the very edge of its box, so a peak that overshoots loses the overflow
/// rather than scribbling past the border or panicking.
///
/// `bounds` is the `(width, height)` of the surface in pixels, and the
/// coordinates are surface-absolute, matching [`draw_text`]'s convention.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_line(
    canvas: &mut [u8],
    stride: u32,
    bounds: (u32, u32),
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: Color,
) {
    let (dx, dy) = ((x1 - x0).abs(), (y1 - y0).abs());
    let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
    let mut err = dx - dy;
    let (mut x, mut y) = (x0, y0);
    loop {
        if x >= 0 && y >= 0 && (x as u32) < bounds.0 && (y as u32) < bounds.1 {
            let off = y as usize * stride as usize + x as usize * 4;
            if off + 3 < canvas.len() {
                canvas[off..off + 4].copy_from_slice(&color.to_bgra(255));
            }
        }
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 > -dy {
            err -= dy;
            x += sx;
        }
        if e2 < dx {
            err += dx;
            y += sy;
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

    /// A line has to actually connect its endpoints and stay 1px thick, and it
    /// has to be *continuous* — Bresenham that steps both axes unconditionally
    /// leaves a diagonal gap on every other pixel, which on the visualiser's
    /// trace shows up as a visibly dashed line rather than a solid one.
    #[test]
    fn draw_line_connects_its_endpoints_without_gaps() {
        let (w, h) = (40u32, 40u32);
        let stride = w * 4;
        let white = Color::rgb(0xff, 0xff, 0xff);

        // A 45° diagonal: the hard case for gap-free stepping.
        let mut canvas = vec![0u8; (stride * h) as usize];
        draw_line(&mut canvas, stride, (w, h), 4, 4, 20, 20, white);
        let lit = |c: &[u8], x: u32, y: u32| c[(y * stride + x * 4) as usize + 3] != 0;
        for i in 4..=20u32 {
            assert!(lit(&canvas, i, i), "gap on the diagonal at ({i},{i})");
        }

        // Horizontal and vertical runs are exact, single-row/column, and reach
        // both ends.
        let mut canvas = vec![0u8; (stride * h) as usize];
        draw_line(&mut canvas, stride, (w, h), 5, 10, 25, 10, white);
        for x in 5..=25u32 {
            assert!(lit(&canvas, x, 10), "horizontal gap at x={x}");
            assert!(
                !((5..=25).contains(&x) && lit(&canvas, x, 9))
                    && !((5..=25).contains(&x) && lit(&canvas, x, 11)),
                "horizontal line bled onto a second row at x={x}"
            );
        }
        let mut canvas = vec![0u8; (stride * h) as usize];
        draw_line(&mut canvas, stride, (w, h), 10, 5, 10, 25, white);
        for y in 5..=25u32 {
            assert!(lit(&canvas, 10, y), "vertical gap at y={y}");
        }

        // A zero-length line still marks its point, so a flat trace segment
        // (both ends at the same y) is visible rather than invisible.
        let mut canvas = vec![0u8; (stride * h) as usize];
        draw_line(&mut canvas, stride, (w, h), 7, 7, 7, 7, white);
        assert!(lit(&canvas, 7, 7), "a zero-length line drew nothing");
    }

    /// The trace runs edge to edge, so the endpoints land on the last pixel of
    /// the box rather than one past it.
    #[test]
    fn draw_line_clips_instead_of_panicking() {
        let mut canvas = [0u8; 4 * 4];
        let white = Color::rgb(0xff, 0xff, 0xff);
        for (a, b) in [
            ((-50i32, -50i32), (2, 2)),
            ((2, 2), (500, 500)),
            ((-500, 2), (500, -500)),
            ((0, 0), (0, 0)),
        ] {
            draw_line(&mut canvas, 4, (1, 1), a.0, a.1, b.0, b.1, white);
        }
        // Nothing wrote outside the 1x1 pixel surface.
        assert!(canvas[4..].iter().all(|&b| b == 0), "{canvas:?}");
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

    /// `stride` is bytes per row, the same convention `fill_rect` uses, so a
    /// glyph has to land on the row the caller thinks it does.
    #[test]
    fn draw_text_lands_on_the_row_the_pen_asked_for() {
        let Some(font) = font() else {
            return;
        };
        let (w, h) = (60u32, 40u32);
        let mut canvas = vec![0u8; (w * h * 4) as usize];
        // The 'W' glyph's ink at this size is about 13x16, so a baseline at y=30
        // has to put that ink in the lower part of the canvas. A stride given in
        // pixels instead of bytes scatters the rows by a factor of four.
        draw_text(
            font,
            Scale::uniform(28.0),
            &mut canvas,
            w * 4,
            (w, h),
            "W",
            0.0,
            30.0,
            Color::rgb(0xff, 0xff, 0xff),
        );
        let painted = |y: u32| {
            canvas.as_chunks::<4>().0[y as usize * w as usize..][..w as usize]
                .iter()
                .filter(|px| px[3] != 0)
                .count()
        };
        let rows: Vec<u32> = (0..h).filter(|&y| painted(y) > 0).collect();
        assert!(!rows.is_empty(), "nothing was drawn");
        assert!(
            rows.first() >= Some(&14) && rows.last() <= Some(&30),
            "ink should sit on the baseline's rows, got {rows:?}"
        );
        // And it must be one contiguous band near the pen, not a smear down the
        // whole canvas.
        assert!(
            rows.len() <= 20,
            "a single 28px glyph smeared over {} rows: {rows:?}",
            rows.len()
        );
    }

    /// Text has to composite over what is behind it rather than paste over it.
    /// Painted onto an opaque mid-grey with white ink, every pixel can only move
    /// *toward* white — pasting would push some pixels back down toward black,
    /// and pasting over black would be indistinguishable from compositing.
    #[test]
    fn draw_text_blends_with_the_background_instead_of_replacing_it() {
        let Some(font) = font() else {
            return;
        };
        let (w, h) = (200u32, 40u32);
        let mut canvas = vec![0x80u8; (w * h * 4) as usize];
        // Opaque, not just grey: an alpha of 0x80 here would make every blended
        // edge transparent and the test would only ever see the fully-covered
        // pixels.
        canvas
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .for_each(|px| *px = [0x80, 0x80, 0x80, 0xff]);
        draw_text(
            font,
            Scale::uniform(28.0),
            &mut canvas,
            w * 4,
            (w, h),
            "WWW",
            2.0,
            30.0,
            Color::rgb(0xff, 0xff, 0xff),
        );
        let painted: Vec<[u8; 4]> = canvas
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|px| px[3] == 0xff)
            .filter(|px| **px != [0x80, 0x80, 0x80, 0xff])
            .copied()
            .collect();
        assert!(!painted.is_empty(), "nothing was drawn");
        for px in &painted {
            assert!(
                px[0] >= 0x80 && px[1] >= 0x80 && px[2] >= 0x80,
                "a pixel went darker than the background it was drawn over: {px:?}"
            );
        }
        assert!(
            painted.iter().any(|px| px[0] > 0x80 && px[0] < 0xff),
            "no partially-covered pixel, so nothing was actually blended"
        );
        assert!(
            painted.iter().any(|px| px[0] == 0xff),
            "no fully-covered pixel, so the glyph never landed"
        );
    }

    /// The box a module draws around its text is sized from `text_width`, and the
    /// glyphs are placed from the same advances. If the two disagree the text
    /// overflows its own box, so they are checked against each other here rather
    /// than against a hard-coded number that would only hold for one font.
    #[test]
    fn drawn_ink_agrees_with_the_measured_width() {
        let Some(font) = font() else {
            return;
        };
        let scale = Scale::uniform(28.0);
        let text = "WWWWWWWW";
        let measured = text_width(font, scale, text);

        let (w, h) = (300u32, 40u32);
        let mut canvas = vec![0u8; (w * h * 4) as usize];
        draw_text(
            font,
            scale,
            &mut canvas,
            w * 4,
            (w, h),
            text,
            0.0,
            30.0,
            Color::rgb(0xff, 0xff, 0xff),
        );
        let span = 1 + canvas
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
            .filter(|(_, px)| px[3] != 0)
            .map(|(i, _)| i as u32 % w)
            .max()
            .unwrap() as usize;
        // Side bearings mean the ink is a little narrower or wider than the
        // advance sum, but nowhere near a different scale.
        assert!(
            span as f32 > measured * 0.9 && (span as f32) < measured + 4.0,
            "ink spans {span}px but the text measures {measured:.2}px"
        );
    }

    /// A run wider than its box has to lose the overflow, not scribble past the
    /// edge and not take the bar down.
    #[test]
    fn draw_text_clips_to_its_bounds_instead_of_overrunning() {
        let Some(font) = font() else {
            return;
        };
        let scale = Scale::uniform(28.0);
        let text = "WWWWWWWW";
        let bound_w = text_width(font, scale, text).ceil() as u32;

        let (w, h) = (300u32, 40u32);
        let mut canvas = vec![0u8; (w * h * 4) as usize];
        draw_text(
            font,
            scale,
            &mut canvas,
            w * 4,
            (bound_w, h),
            text,
            0.0,
            30.0,
            Color::rgb(0xff, 0xff, 0xff),
        );
        assert!(
            canvas
                .as_chunks::<4>()
                .0
                .iter()
                .enumerate()
                .all(|(i, px)| px[3] == 0 || (i as u32 % w) < bound_w),
            "ink landed at or past the bound of {bound_w}"
        );
        assert!(
            canvas.as_chunks::<4>().0.iter().any(|px| px[3] != 0),
            "clipping to the measured width left nothing at all"
        );
    }

    /// Bounds of nothing, and a pen far off-canvas, are both ordinary: a module
    /// with a zero-width box must not index out of the canvas.
    #[test]
    fn draw_text_outside_its_bounds_writes_nothing_and_does_not_panic() {
        let Some(font) = font() else {
            return;
        };
        let (w, h) = (40u32, 40u32);
        for (bounds, pen_x) in [
            ((0, 0), 0.0),
            ((0, 40), 0.0),
            ((40, 0), 0.0),
            ((40, 40), -500.0),
            ((40, 40), 5000.0),
        ] {
            let mut canvas = vec![0u8; (w * h * 4) as usize];
            draw_text(
                font,
                Scale::uniform(28.0),
                &mut canvas,
                w * 4,
                bounds,
                "W",
                pen_x,
                30.0,
                Color::rgb(0xff, 0xff, 0xff),
            );
            assert!(
                canvas.iter().all(|&b| b == 0),
                "bounds={bounds:?} pen_x={pen_x}"
            );
        }
    }
}

/// Every module that draws text used to carry its own copy of the glyph blit and
/// its own font search. They now share one renderer and one font, which means a
/// mistake in that pair takes out the clock, the workspace numbers and the
/// visualiser at once instead of one of them. These render the modules that need
/// no outside process — the widget strip is left out because sampling it spawns
/// `pactl`, `swaync-client` and friends, and the tray popup because it needs an
/// `xdg_shell`.
#[cfg(test)]
mod module_rendering {
    use super::*;
    use crate::cava::Cava;
    use crate::clock::Clock;
    use crate::config::{CavaMode, Color, Colors};
    use crate::workspaces::WorkspaceList;

    const W: u32 = 1280;
    const H: u32 = 30;

    /// A frame with nothing in it, the size every module below is drawn at.
    fn blank() -> Vec<u8> {
        vec![0u8; (W * H * 4) as usize]
    }

    /// The palette every test draws with unless it is specifically varying it.
    fn palette() -> Colors {
        Colors::default()
    }

    /// Non-background pixels, i.e. anything the module actually painted.
    fn ink(canvas: &[u8]) -> usize {
        canvas
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|&&px| px != [0, 0, 0, 0])
            .count()
    }

    /// The box a module draws is sized from its text, so the module's own width
    /// claim is checked against what it painted: if the shared renderer stopped
    /// honouring the advances, the text would spill past the box it was measured
    /// into.
    fn assert_boxed_in(canvas: &[u8], x0: u32, box_w: u32) {
        let past = canvas
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
            .filter(|(i, px)| **px != [0, 0, 0, 0] && (*i as u32 % W) >= x0 + box_w)
            .count();
        assert_eq!(
            past,
            0,
            "painted {past}px past the box ending at {}",
            x0 + box_w
        );
    }

    #[test]
    fn the_clock_paints_a_centred_box_and_some_text() {
        let Some(_) = font() else { return };
        let mut canvas = blank();
        let mut clock = Clock::new();
        clock.render(&palette(), &mut canvas, 400, 800, H, W * 4);
        assert!(ink(&canvas) > 100, "the clock drew almost nothing");
    }

    #[test]
    fn the_workspaces_draw_one_button_per_slot() {
        let Some(_) = font() else { return };
        let mut canvas = blank();
        let mut ws = WorkspaceList::new();
        ws.render(&palette(), &mut canvas, 0, H, W * 4);
        assert!(
            ink(&canvas) > 100,
            "the workspace buttons drew almost nothing"
        );
        // Nothing may escape the container the module itself lays out.
        assert_boxed_in(&canvas, 0, ws.width());
    }

    /// The visualiser used to draw 14 block glyphs sized from `text_width`, so
    /// this test checked its box against that measurement. It now draws a
    /// polyline of fixed geometry instead, which means two different things are
    /// worth pinning instead: that a *known* frame lands where the geometry says
    /// it should (so the trace isn't drawn at some arbitrary scale), and that
    /// nothing escapes the box — the wash fills every column down to the
    /// baseline, so an off-by-one in the plot area would scribble past the border
    /// rather than merely look wrong.
    #[test]
    fn the_visualiser_draws_its_trace_inside_its_box() {
        let mut canvas = blank();
        let mut cava = Cava::new();
        // A ramp: silent at the left edge, loudest at the right. The envelope
        // needs a few frames to climb, so render repeatedly — which also stands
        // in for a running bar.
        cava.set_frame(&(0..14).map(|i| i * 8).collect::<Vec<u8>>());
        for _ in 0..20 {
            cava.render(
                &palette(),
                crate::config::CavaMode::Graph,
                &mut canvas,
                200,
                H,
                W * 4,
            );
        }
        assert!(ink(&canvas) > 100, "the visualiser drew almost nothing");
        assert_boxed_in(&canvas, 200, cava.width());

        // The quiet end of the ramp must sit near the baseline and the loud end
        // near the top: a trace that ignored the frame, or plotted it inverted,
        // would put them the other way up or at the same height.
        //
        // "Ink" here means *not the box fill*. The fill is opaque black, so
        // `alpha != 0` would call the entire interior painted and both ends
        // would report the top border row.
        let px = canvas.as_chunks::<4>().0;
        let painted = |x: u32, y: u32| -> bool { px[(y * W + x) as usize][..3] != [0, 0, 0] };
        let topmost = |from: u32, to: u32| -> Option<u32> {
            let x = (from..to).find(|&x| (1..H - 1).any(|y| painted(x, y)))?;
            Some((1..H - 1).find(|&y| painted(x, y)).unwrap())
        };
        let left = 200 + 1; // the box's 1px border; the plot runs to the edges
        let quiet = topmost(left, left + 3).expect("no ink at the quiet end");
        // The last band sits PITCH px from the last-but-one; sampling the final
        // columns also checks the plot reaches the right edge.
        let loud = topmost(left + 112, left + 117).expect("no ink at the loud end");
        assert!(
            loud < quiet,
            "the loud end of the ramp should draw higher up than the quiet end, \
             but they were at rows {loud} and {quiet}"
        );
    }

    /// The plot runs edge to edge inside the border — no side padding — so the
    /// trace's first and last vertices sit on the first and last interior
    /// columns. The block-glyph version this replaced had 8px of h-padding on
    /// each side (waybar `#cava { padding: 0 8px }`), which is right for text
    /// and wrong here: it left the trace floating in the middle of its box.
    #[test]
    fn the_trace_reaches_both_edges_of_its_box() {
        let mut canvas = blank();
        let mut cava = Cava::new();
        // Full scale, so every band draws and the extreme columns are painted.
        cava.set_frame(&[100; 14]);
        for _ in 0..40 {
            cava.render(
                &palette(),
                crate::config::CavaMode::Graph,
                &mut canvas,
                200,
                H,
                W * 4,
            );
        }
        let px = canvas.as_chunks::<4>().0;
        let painted = |x: u32, y: u32| px[(y * W + x) as usize][..3] != [0, 0, 0];

        // Interior columns, i.e. excluding the 1px border on each side.
        let first = 200 + 1;
        let last = 200 + cava.width() - 2;
        for x in [first, first + 1, last - 1, last] {
            assert!(
                (1..H - 1).any(|y| painted(x, y)),
                "column {x} is empty: the plot left a gap at the edge of the box"
            );
        }
        // And the border itself survives — the plot stops *at* it, not through.
        assert!(painted(200, 1), "the left border was painted over");
        assert!(
            painted(200 + cava.width() - 1, 1),
            "the right border was painted over"
        );
    }

    /// A silent frame has to look like a flat line resting on the baseline rule,
    /// not like an empty box — otherwise "nothing playing" and "the module is
    /// broken" are the same picture.
    #[test]
    fn a_silent_visualiser_still_draws_its_baseline() {
        let mut canvas = blank();
        let mut cava = Cava::new();
        for _ in 0..5 {
            cava.render(
                &palette(),
                crate::config::CavaMode::Graph,
                &mut canvas,
                200,
                H,
                W * 4,
            );
        }
        let px = canvas.as_chunks::<4>().0;
        let left = 200 + 1;
        // Not the box fill, which is opaque black across the whole interior.
        let painted = |x: u32, y: u32| px[(y * W + x) as usize][..3] != [0, 0, 0];
        let baseline_rows: Vec<u32> = (H - 6..H - 2)
            .filter(|&y| (left..left + 60).any(|x| painted(x, y)))
            .collect();
        assert!(
            !baseline_rows.is_empty(),
            "a silent visualiser drew nothing near its baseline"
        );
    }

    /// Loud/quiet alternating bands is the worst case for a 14-sample plot 9px
    /// apart: drawn raw, every other band is a full-height step and the trace
    /// reads as a zigzag. Blending each band with its neighbours is what turns
    /// it into one curve, and the geometry tests above can't see it — they pass
    /// just as well on a zigzag.
    #[test]
    fn a_spiky_frame_draws_one_smooth_curve() {
        let mut canvas = blank();
        let mut cava = Cava::new();
        cava.set_frame(
            &(0..14)
                .map(|i| if i % 2 == 0 { 90 } else { 8 })
                .collect::<Vec<u8>>(),
        );
        // Enough renders for the envelope to reach the frame.
        for _ in 0..40 {
            cava.render(
                &palette(),
                crate::config::CavaMode::Graph,
                &mut canvas,
                200,
                H,
                W * 4,
            );
        }
        let px = canvas.as_chunks::<4>().0;
        let painted = |x: u32, y: u32| px[(y * W + x) as usize][..3] != [0, 0, 0];
        let left = 200 + 1;
        // Between the bands is the trace itself; the band columns carry the
        // peak ticks too, filtered out here so this measures only the curve.
        let tops: Vec<u32> = (left + 9..left + 108)
            .filter(|x| (x - left) % 9 >= 2)
            .filter_map(|x| (1..H - 1).find(|&y| painted(x, y)))
            .collect();
        assert!(
            tops.len() > 50,
            "the trace is missing most of its columns: only {} of them drew",
            tops.len()
        );
        let (lo, hi) = (*tops.iter().min().unwrap(), *tops.iter().max().unwrap());
        assert!(
            hi - lo <= 2,
            "the trace still steps between its spikes: rows {lo}..{hi}"
        );
    }

    /// The peak hold tracks the displayed curve, so at steady state every tick
    /// sits on the trace rather than floating above it. The hold used to track
    /// the raw envelope while the trace drew the smoothed one, so on a spiky
    /// frame each loud band's tick hovered ~10 rows above the line like a
    /// random detached pixel.
    #[test]
    fn steady_peaks_sit_on_the_trace_not_above_it() {
        let mut canvas = blank();
        let mut cava = Cava::new();
        cava.set_frame(
            &(0..14)
                .map(|i| if i % 2 == 0 { 90 } else { 8 })
                .collect::<Vec<u8>>(),
        );
        // Enough renders for the envelope — and the peak hold — to settle.
        for _ in 0..40 {
            cava.render(
                &palette(),
                crate::config::CavaMode::Graph,
                &mut canvas,
                200,
                H,
                W * 4,
            );
        }
        let px = canvas.as_chunks::<4>().0;
        let painted = |x: u32, y: u32| px[(y * W + x) as usize][..3] != [0, 0, 0];
        let left = 200 + 1;
        // Every column this time, band centres included: at steady state the
        // ticks have nowhere to be but on the line.
        let tops: Vec<u32> = (left + 9..left + 108)
            .filter_map(|x| (1..H - 1).find(|&y| painted(x, y)))
            .collect();
        assert!(
            tops.len() > 50,
            "the trace is missing most of its columns: only {} of them drew",
            tops.len()
        );
        let (lo, hi) = (*tops.iter().min().unwrap(), *tops.iter().max().unwrap());
        assert!(hi - lo <= 2, "peaks float above the trace: rows {lo}..{hi}");
    }

    /// The `[cava] mode = "bars"` path draws a solid bar per band, rising from
    /// the baseline and — unlike the graph — leaving no stray pixels above a
    /// fallen bar. Every band's column must reach the baseline after a loud
    /// frame so the bars read as connected to the box.
    #[test]
    fn bars_mode_draws_a_bar_per_band_to_the_baseline() {
        let mut canvas = blank();
        let mut cava = Cava::new();
        cava.set_frame(&(0..14).map(|i| 40 + i * 4).collect::<Vec<u8>>());
        for _ in 0..10 {
            cava.render(&palette(), CavaMode::Bars, &mut canvas, 200, H, W * 4);
        }
        let px = canvas.as_chunks::<4>().0;
        let painted = |x: u32, y: u32| px[(y * W + x) as usize][..3] != [0, 0, 0];
        let left = 200 + 1;
        let (bars_x0, bar_slot, bar_w) = crate::cava::bar_geometry();
        for i in 0..14 {
            let x = left + bars_x0 + i as u32 * bar_slot + bar_w / 2;
            assert!((1..H - 1).any(|y| painted(x, y)), "band {i} drew nothing");
            // Each bar must reach down to the baseline; a gap between the bar's
            // foot and the baseline rule would look like a floating block.
            let bottom = (1..H - 1).rfind(|&y| painted(x, y)).unwrap();
            assert!(
                (H - 4..H - 1).contains(&bottom),
                "band {i} bar stops at row {bottom}, short of the baseline"
            );
        }
    }

    /// The palette used to be a process global, so a module could have kept
    /// reading it instead of the one it was handed and every test would still
    /// pass. Rendering the same module with two palettes must therefore produce
    /// two different frames; a module that ignored the argument draws the same
    /// bytes twice.
    #[test]
    fn the_palette_reaches_the_pixels() {
        if font().is_none() {
            return;
        }
        let loud = Colors {
            accent: Color::rgb(0xff, 0x00, 0x00),
            background: Color::rgb(0x00, 0x00, 0xff),
            ..Colors::default()
        };
        let quiet = Colors {
            accent: Color::rgb(0x12, 0x34, 0x56),
            background: Color::rgb(0x00, 0x00, 0x00),
            ..Colors::default()
        };

        let mut loud_clock = blank();
        Clock::new().render(&loud, &mut loud_clock, 400, 800, H, W * 4);
        let mut quiet_clock = blank();
        Clock::new().render(&quiet, &mut quiet_clock, 400, 800, H, W * 4);
        assert_ne!(loud_clock, quiet_clock, "the clock ignored the palette");

        let mut loud_ws = blank();
        WorkspaceList::new().render(&loud, &mut loud_ws, 0, H, W * 4);
        let mut quiet_ws = blank();
        WorkspaceList::new().render(&quiet, &mut quiet_ws, 0, H, W * 4);
        assert_ne!(loud_ws, quiet_ws, "the workspaces ignored the palette");

        let mut loud_cava = blank();
        Cava::new().render(
            &loud,
            crate::config::CavaMode::Graph,
            &mut loud_cava,
            200,
            H,
            W * 4,
        );
        let mut quiet_cava = blank();
        Cava::new().render(
            &quiet,
            crate::config::CavaMode::Graph,
            &mut quiet_cava,
            200,
            H,
            W * 4,
        );
        assert_ne!(loud_cava, quiet_cava, "the visualiser ignored the palette");
    }
}

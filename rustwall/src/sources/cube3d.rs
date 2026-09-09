//! A properly-rotating 4D ASCII tesseract (hypercube).
//!
//! This is a real 4D render, not a sketch: the sixteen tesseract vertices are
//! rotated through the XW, YW and ZW planes every frame (the signature
//! rotations that make a hypercube visibly "turn inside out"), perspective-
//! projected from 4D down to 3D along the fourth axis, spun again with the
//! ordinary 3D rotation, and then the 32 edges are stroked onto the cell grid.
//! Each edge is depth-shaded to the ASCII ramp so the whole thing reads as a
//! solid, rotating tesseract in glyphs; that same brightness also scales the
//! glyph colour, so a dimmer cell is drawn with a dimmer colour too.

use crate::config::{FONT_PATHS, WATERFALL_FONT_PATH};
use crate::source::FrameSource;
use rusttype::{Font, Scale, point};
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

/// Classic ASCII-art ramp: brightest on the left, darkest (space) on right.
/// Wider than before (14 steps) so brightness falloff along a single edge
/// reads as a smooth taper instead of a couple of abrupt jumps.
const RAMP: &[u8] = b"@$#%8&oa*+=-:. ";

/// Glyph pool for the Matrix-style rain. Latin letters, digits and symbols so
/// every glyph exists in the bundled monospace fonts (katakana would not).
/// Characters from ASCII ranges: 33-63, 90-95, 97-126
const RAIN: &[u8] = b"!\"#$%&'()*+,-./0123456789:;<=>?@Z[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~";

/// One falling wall of rain: a solid, one-symbol-wide column of glyphs that
/// slides down smoothly and only resets once it is fully off the bottom of the
/// screen. It enters from the top (clipped, so it appears to emerge smoothly)
/// and it lives at a random depth beside the tesseract — farther walls are
/// drawn smaller and dimmer.
struct Waterfall {
    x: f32,    // wall's pixel x (left edge of the glyph cell)
    y: f32,    // top edge of the wall, in pixels (slides downward)
    len: usize, // final number of symbols the wall grows to
    pitch: f32, // vertical spacing between symbols, in pixels
    speed: f32, // downward speed, pixels per second
    factor: f32, // font scale relative to the tesseract (depth)
    atten: f32, // overall dimming from depth, 0..1
    fade: f32,  // brightness falloff from the leading (bottom) edge upward
    gradient_speed: f32, // speed at which color transitions from RAIN_COLOR to RAIN_FILLER
    cells: Vec<char>, // the wall's symbols; index 0 = top, last = bottom
    prev_cells: Vec<char>, // previous glyph for each slot, for crossfade during a change
    morph_t: Vec<f32>, // 0..1 crossfade progress from prev_cells -> cells (>=1 = settled)
    mutate_t: Vec<f32>, // per-symbol countdown (seconds) until it re-rolls to a new glyph
    flash_t: Vec<f32>,  // per-symbol remaining pulse time (seconds); 0 = not flashing
    grown: usize,       // how many symbols, counting from the top, are active/visible so far
    grow_t: f32,        // countdown to the next symbol appearing at the bottom
    grow_interval: f32, // seconds between new symbols appearing while writing itself
    column: usize,      // grid column index this wall is locked to
}

/// The colour of the tesseract — kept bright so it's always the visual subject.
const TESSERACT_COLOR: (f32, f32, f32) = (191.0, 255.0, 217.0);

/// The colour of the rain — same hue family as the tesseract but dimmer and a
/// touch desaturated, so the rain reads as background texture, not subject.
const RAIN_COLOR: (f32, f32, f32) = (160.0, 210.0, 185.0);

/// Dark green color for waterfall body (non-bottom glyphs).
const RAIN_FILLER: (f32, f32, f32) = (20.0, 100.0, 20.0);

/// How long a single "flash" glint pulse lasts, in seconds. Shared between
/// update_waterfalls (which counts it down) and draw_waterfall (which turns
/// the remaining time into a smooth bump-shaped brightness curve).
const FLASH_DURATION: f32 = 0.45;

/// A pre-rasterized glyph: an 8-bit coverage mask `data` (0..255 per pixel)
/// plus its base position offsets, ready to be blitted each frame. Building
/// these once avoids re-tracing/re-rasterizing the font outline every frame,
/// which is what lets us hit high frame rates.
struct CachedGlyph {
    min_x: i32,
    min_y: i32,
    w: u32,
    h: u32,
    data: Vec<u8>,
}

/// The sixteen corners of a unit tesseract around the origin. Index bits are
/// 0=x, 1=y, 2=z, 3=w; each coordinate is +1 when its bit is set else -1.
fn vertices() -> Vec<[f32; 4]> {
    (0..16u8)
        .map(|i| {
            [
                if i & 1 == 0 { -1.0 } else { 1.0 },
                if i & 2 == 0 { -1.0 } else { 1.0 },
                if i & 4 == 0 { -1.0 } else { 1.0 },
                if i & 8 == 0 { -1.0 } else { 1.0 },
            ]
        })
        .collect()
}

/// The 32 edges: each vertex connects to the four neighbours that differ in
/// exactly one coordinate, pairing each undirected edge once.
fn edges() -> Vec<[usize; 2]> {
    let mut e = Vec::new();
    for v in 0..16u16 {
        for bit in 0..4u16 {
            let nbr = v ^ (1 << bit);
            if v < nbr {
                e.push([v as usize, nbr as usize]);
            }
        }
    }
    e
}

pub struct RotatingTesseract {
    font: Option<Font<'static>>,
    waterfall_font: Option<Font<'static>>,
    font_path: Option<String>,
    waterfall_font_path: Option<String>,
    start_time: Instant,
    width: u32,
    height: u32,
    grid_cols: u32,
    grid_rows: u32,
    char_width: f32,
    char_height: f32,
    offset_x: f32,
    offset_y: f32,
    ascent: f32,
    /// Tiny xorshift PRNG state (no external rand dependency).
    rng: u64,
    /// Timestamp of the previous frame, to compute per-frame dt.
    last_frame: Instant,
    /// The Matrix-style rain columns around the tesseract.
    waterfalls: Vec<Waterfall>,
    /// A shuffled cycle of grid-column indices, so walls are handed out
    /// columns round-robin instead of landing at fully random x positions.
    /// This is what keeps them from clumping: every column gets a turn
    /// before any column repeats, so piles only ever happen by the rare
    /// luck of a short cycle wrapping, never systematically.
    column_cycle: Vec<usize>,
    column_cursor: usize,
    /// How many screen columns the rain grid has (== w / base char width).
    total_columns: usize,
    /// Rasterized glyph bitmaps for the tesseract, keyed by (char, rounded scale).
    glyphs: HashMap<(char, u32), CachedGlyph>,
    /// Rasterized glyph bitmaps for waterfalls, keyed by (char, rounded scale).
    waterfall_glyphs: HashMap<(char, u32), CachedGlyph>,
}

impl RotatingTesseract {
    pub fn new(font_path: Option<String>, waterfall_font_path: Option<String>) -> Self {
        let seed = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15);
        Self {
            font: None,
            waterfall_font: None,
            font_path,
            waterfall_font_path,
            start_time: Instant::now(),
            width: 0,
            height: 0,
            grid_cols: 0,
            grid_rows: 0,
            char_width: 0.0,
            char_height: 0.0,
            offset_x: 0.0,
            offset_y: 0.0,
            ascent: 0.0,
            rng: seed.max(1),
            last_frame: Instant::now(),
            waterfalls: Vec::new(),
            column_cycle: Vec::new(),
            column_cursor: 0,
            total_columns: 0,
            glyphs: HashMap::new(),
            waterfall_glyphs: HashMap::new(),
        }
    }

    /// Advance the xorshift64 PRNG.
    fn next_rng(rng: &mut u64) -> u64 {
        let mut x = *rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *rng = x;
        x
    }

    /// Uniform float in [lo, hi).
    fn rand_range(rng: &mut u64, lo: f32, hi: f32) -> f32 {
        let r = (Self::next_rng(rng) >> 40) as f32 / ((1u64 << 24) as f32);
        lo + (hi - lo) * r
    }

    fn rand_char(rng: &mut u64) -> char {
        let i = (Self::next_rng(rng) as usize) % RAIN.len();
        RAIN[i] as char
    }

    /// Fisher-Yates shuffle using the xorshift PRNG.
    fn shuffle(rng: &mut u64, v: &mut [usize]) {
        for i in (1..v.len()).rev() {
            let j = (Self::next_rng(rng) as usize) % (i + 1);
            v.swap(i, j);
        }
    }

    /// Hand out the next grid column for a wall to spawn in. Columns are
    /// drawn from a shuffled cycle so every column is used once before any
    /// column repeats — this is what spreads walls evenly across the
    /// screen instead of letting pure randomness clump several onto the
    /// same column, which is what made the rain look un-grid-like.
    fn next_column(&mut self) -> usize {
        if self.column_cycle.is_empty() {
            return 0;
        }
        if self.column_cursor >= self.column_cycle.len() {
            Self::shuffle(&mut self.rng, &mut self.column_cycle);
            self.column_cursor = 0;
        }
        let c = self.column_cycle[self.column_cursor];
        self.column_cursor += 1;
        c
    }

    /// (Re)initialise every rain wall with fresh random parameters.
    fn spawn_waterfalls(&mut self) {
        let base = self.char_height.max(1.0);
        let col_w = self.char_width.max(1.0);
        let w = self.width as f32;
        let h = self.height as f32;
        let count = (10 + (Self::next_rng(&mut self.rng) % 7) as usize) * 20; // 200..=320

        // Lock rain to the same column grid as the screen's own character
        // cells. total_columns is deliberately generous (more columns than
        // walls need) so walls read as sitting in a lattice of discrete
        // lanes rather than free-floating at arbitrary x positions.
        self.total_columns = ((w / col_w).floor() as usize).max(1);
        self.column_cycle = (0..self.total_columns).collect();
        Self::shuffle(&mut self.rng, &mut self.column_cycle);
        self.column_cursor = 0;

        self.waterfalls = Vec::with_capacity(count);
        for _ in 0..count {
            let col = self.next_column();
            let mut wf = Waterfall {
                x: 0.0,
                y: 0.0,
                len: 1,
                pitch: base,
                speed: 100.0,
                factor: 1.0,
                atten: 1.0,
                fade: 0.7,
                gradient_speed: 1.0,
                cells: Vec::new(),
                prev_cells: Vec::new(),
                morph_t: Vec::new(),
                mutate_t: Vec::new(),
                flash_t: Vec::new(),
                grown: 0,
                grow_t: 0.0,
                grow_interval: 0.08,
                column: col,
            };
            Self::randomise_waterfall(&mut self.rng, &mut wf, base, col_w, w, h, col);
            self.waterfalls.push(wf);
        }
    }

    /// Randomize a wall's position, depth (size + dim), speed, length, glyphs.
    fn randomise_waterfall(rng: &mut u64, wf: &mut Waterfall, base: f32, col_w: f32, w: f32, h: f32, col: usize) {
        // Random depth along the view axis: nearer -> larger factor & brighter.
        let z = Self::rand_range(rng, -5.5, 3.0);
        wf.factor = (1.0 + z * 0.11).clamp(0.32, 1.55);
        let norm = ((wf.factor - 0.32) / (1.55 - 0.32)).clamp(0.0, 1.0);
        // Capped well below the tesseract's brightness range (see RAMP-driven
        // edge brightness in render()) so the rain never competes with it.
        wf.atten = (0.15 + 0.70 * norm) * 1.4;

        // Locked to its assigned grid column rather than a free-floating
        // random x — this is what keeps the rain reading as a lattice of
        // lanes (like the screen's own character grid) instead of walls
        // drifting to arbitrary, overlapping positions.
        wf.column = col;
        wf.x = (col as f32 * col_w).clamp(0.0, (w - col_w).max(0.0));
        wf.speed = Self::rand_range(rng, 180.0, 440.0);
        wf.fade = Self::rand_range(rng, 0.62, 0.85);
        wf.gradient_speed = Self::rand_range(rng, 0.01, 0.05);
        wf.pitch = base * wf.factor;

        // A wall is a solid stack of symbols, fully-filled (no gaps). Noticeably
        // tall (~3x earlier) so it reads as a true falling column.
        let len = (Self::rand_range(rng, 12.0, 42.0).round()) as usize;
        wf.len = len.max(2);
        wf.cells = (0..wf.len).map(|_| Self::rand_char(rng)).collect();
        wf.prev_cells = vec![' '; wf.len];
        // Every slot starts fully settled (no crossfade in progress) except
        // slot 0, which is the very first symbol to appear and gets its own
        // fade-in the moment the wall starts growing (see below).
        wf.morph_t = vec![1.0; wf.len];
        // Mutation is now rare and per-symbol so the column reads as mostly
        // stable text with the occasional glyph quietly re-deciding itself,
        // not a flicker. Each change crossfades (see draw_waterfall) instead
        // of popping.
        wf.mutate_t = (0..wf.len).map(|_| Self::rand_range(rng, 3.0, 9.0)).collect();
        wf.flash_t = vec![0.0; wf.len];

        // The wall "writes itself": it starts with nothing visible and a new
        // symbol appears below the previous one every grow_interval seconds,
        // extending the column downward, until it reaches its full length.
        // Only then does the whole block begin sliding as a rigid body.
        wf.grown = 0;
        wf.grow_interval = Self::rand_range(rng, 0.05, 0.14);
        wf.grow_t = wf.grow_interval;

        // Start with the wall's anchor above the screen so it slides in
        // smoothly rather than popping into existence mid-frame. Offsets are
        // spread across one screen height so many walls enter staggered
        // instead of all at once.
        let wall_px = wf.len as f32 * wf.pitch;
        wf.y = -Self::rand_range(rng, wall_px * 0.3, wall_px + h);
    }

    /// Advance every rain wall by dt seconds.
    fn update_waterfalls(&mut self, dt: f32) {
        let base = self.char_height.max(1.0);
        let col_w = self.char_width.max(1.0);
        let w = self.width as f32;
        let h = self.height as f32;
        for i in 0..self.waterfalls.len() {
            // Only respawn once the wall's top edge has cleared the bottom of
            // the screen — i.e. the wall is fully off-screen. It never
            // vanishes part-way down. Grab a fresh column up front (needs a
            // &mut self borrow of its own) before touching the wall/rng.
            let needs_respawn = self.waterfalls[i].y > h;
            let new_col = if needs_respawn { Some(self.next_column()) } else { None };

            let wf = &mut self.waterfalls[i];
            let rng = &mut self.rng;

            // The whole wall slides down as a rigid, contiguous block.
            wf.y += wf.speed * dt;

            if let Some(col) = new_col {
                Self::randomise_waterfall(rng, wf, base, col_w, w, h, col);
                continue;
            }

            // Growth: a new symbol appears below the current bottom every
            // grow_interval seconds until the wall reaches full length. The
            // newly revealed slot starts blank and crossfades in via the
            // same morph mechanism as an ordinary mutation, so it fades into
            // place instead of popping.
            if wf.grown < wf.cells.len() {
                wf.grow_t -= dt;
                if wf.grow_t <= 0.0 {
                    let j = wf.grown;
                    wf.grown += 1;
                    wf.grow_t = wf.grow_interval;
                    wf.prev_cells[j] = ' ';
                    wf.morph_t[j] = 0.0;
                    // Give the freshly-revealed glyph a long, staggered wait
                    // before it's eligible to mutate again.
                    wf.mutate_t[j] = Self::rand_range(rng, 3.0, 9.0);
                }
            }

            let visible = wf.grown;
            for j in 0..visible {
                // Crossfade progress for an in-flight glyph change.
                if wf.morph_t[j] < 1.0 {
                    wf.morph_t[j] = (wf.morph_t[j] + dt / 0.35).min(1.0);
                }

                // Per-glyph mutation: rare, staggered, and only for glyphs
                // that have already settled from their last change — this is
                // the occasional "the column quietly rewrites a symbol"
                // effect, not a flicker.
                wf.mutate_t[j] -= dt;
                if wf.mutate_t[j] <= 0.0 && wf.morph_t[j] >= 1.0 {
                    wf.prev_cells[j] = wf.cells[j];
                    wf.cells[j] = Self::rand_char(rng);
                    wf.morph_t[j] = 0.0;
                    wf.mutate_t[j] = Self::rand_range(rng, 3.0, 9.0);
                }

                // Smooth flash pulse: once triggered, flash_t counts down
                // from FLASH_DURATION to 0 over several frames, and the
                // brightness follows a bump curve (draw_waterfall reads
                // flash_t), so it visibly ramps up and fades out instead of
                // slamming on and off in a single frame.
                if wf.flash_t[j] > 0.0 {
                    wf.flash_t[j] = (wf.flash_t[j] - dt).max(0.0);
                } else if Self::rand_range(rng, 0.0, 1.0) < dt * 0.04 {
                    wf.flash_t[j] = FLASH_DURATION;
                }
            }
        }
    }

    /// Rotate a 4D point in the XW, YW, then ZW planes (radians). These are
    /// the rotations that keep the hypercube inside its own 4D space while
    /// making its 3D shadow appear to turn inside out.
    fn rotate4(p: [f32; 4], axw: f32, ayw: f32, azw: f32) -> [f32; 4] {
        let (c1, s1) = (axw.cos(), axw.sin());
        let (x, w1) = (p[0] * c1 - p[3] * s1, p[0] * s1 + p[3] * c1); // XW
        let (c2, s2) = (ayw.cos(), ayw.sin());
        let (y, w2) = (p[1] * c2 - w1 * s2, p[1] * s2 + w1 * c2); // YW
        let (c3, s3) = (azw.cos(), azw.sin());
        let (z, w3) = (p[2] * c3 - w2 * s3, p[2] * s3 + w2 * c3); // ZW
        [x, y, z, w3]
    }

    /// Rotate a 3D point/direction about X, then Y, then Z (radians).
    fn rotate(p: [f32; 3], ax: f32, ay: f32, az: f32) -> [f32; 3] {
        let (cx, sx) = (ax.cos(), ax.sin());
        let (cy, sy) = (ay.cos(), ay.sin());
        let (cz, sz) = (az.cos(), az.sin());

        let (x, y, z) = (p[0], p[1], p[2]);
        let (y1, z1) = (y * cx - z * sx, y * sx + z * cx); // about X
        let (x2, z2) = (x * cy + z1 * sy, -x * sy + z1 * cy); // about Y
        let (x3, y3) = (x2 * cz - y1 * sz, x2 * sz + y1 * cz); // about Z
        [x3, y3, z2]
    }

    /// Stroke a straight line between two grid points, marking each cell it
    /// passes through. Depth (and therefore the ramp character and
    /// brightness) is interpolated per-step along the line rather than fixed
    /// for the whole edge — otherwise a long edge is just one character
    /// copy-pasted end to end, which is what made edges read as flat
    /// typographic blocks instead of shaded lines. `dmin`/`span` are the
    /// scene-wide depth range (same as used for the edge sort) so every
    /// edge's gradient is calibrated consistently against every other edge.
    fn draw_line(
        grid: &mut Vec<Option<(char, f32)>>,
        cols: u32,
        a: [f32; 2],
        b: [f32; 2],
        depth_a: f32,
        depth_b: f32,
        dmin: f32,
        span: f32,
    ) {
        let steps = ((b[0] - a[0]).abs().max((b[1] - a[1]).abs())).round() as i32;
        let steps = steps.max(1);
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let x = (a[0] + (b[0] - a[0]) * t).round() as i32;
            let y = (a[1] + (b[1] - a[1]) * t).round() as i32;
            let depth = depth_a + (depth_b - depth_a) * t;
            let bt = ((depth - dmin) / span).clamp(0.0, 1.0);
            // Floor raised from 0.5 to 0.35 so far cells still show *some*
            // faint texture rather than snapping straight to the brightest
            // 50%-and-up range everything used to live in.
            let bright = (0.35 + 0.65 * bt).clamp(0.0, 1.0);
            let idx_ramp = ((1.0 - bright) * (RAMP.len() - 1) as f32) as usize;
            let ch = RAMP[idx_ramp.min(RAMP.len() - 1)] as char;
            if x >= 0 && y >= 0 {
                let idx = (y as u32 * cols + x as u32) as usize;
                if idx < grid.len() {
                    grid[idx] = Some((ch, bright));
                }
            }
        }
    }

    /// Stamp a fixed, heavy marker glyph at a vertex's projected grid
    /// position. Vertices are where 3-4 edges converge into a couple of grid
    /// cells at once; without an explicit marker those cells just get
    /// overwritten repeatedly by whichever edge draws last, producing a
    /// smeared, inconsistent clump. A single deliberate glyph here turns
    /// each corner into a clean anchor point the eye can lock onto.
    fn draw_vertex(
        grid: &mut Vec<Option<(char, f32)>>,
        cols: u32,
        rows: u32,
        p: [f32; 2],
        depth: f32,
        dmin: f32,
        span: f32,
    ) {
        let t = ((depth - dmin) / span).clamp(0.0, 1.0);
        let bright = (0.65 + 0.35 * t).clamp(0.0, 1.0);
        let x = p[0].round() as i32;
        let y = p[1].round() as i32;
        if x >= 0 && y >= 0 && x < cols as i32 && y < rows as i32 {
            let idx = (y as u32 * cols + x as u32) as usize;
            if idx < grid.len() {
                grid[idx] = Some(('@', bright));
            }
        }
    }

    /// Perspective-project a 4D point to 3D along the w axis, with the
    /// observer at w = `dist`, the same way a 3D point projects to 2D.
    fn project4(p: [f32; 4], dist: f32) -> [f32; 3] {
        let s = dist / (dist - p[3]);
        [p[0] * s, p[1] * s, p[2] * s]
    }

    /// Draw one glyph at pixel position (x, y) (top-left of its cell) with the
    /// given font scale, brightness and base colour. The RGB is dimmed by the
    /// brightness and by the glyph's antialiasing coverage.
    /// Rasterize one glyph at the given scale into a cached coverage mask.
    /// This is the expensive part (font outline tracing + AA rasterize), so it
    /// runs exactly once per (char, scale) and is cached for the life of the
    /// wallpaper; per-frame drawing only copies these bitmaps.
    fn rasterize_glyph(font: &Font<'static>, ch: char, scale: f32) -> CachedGlyph {
        let glyph = font.glyph(ch).scaled(Scale::uniform(scale)).positioned(point(0.0, 0.0));
        let Some(bb) = glyph.pixel_bounding_box() else {
            return CachedGlyph { min_x: 0, min_y: 0, w: 0, h: 0, data: Vec::new() };
        };
        let w = (bb.max.x - bb.min.x).max(0) as u32;
        let h = (bb.max.y - bb.min.y).max(0) as u32;
        if w == 0 || h == 0 {
            return CachedGlyph { min_x: bb.min.x, min_y: bb.min.y, w: 0, h: 0, data: Vec::new() };
        }
        let area = (w as usize) * (h as usize);
        let mut data = vec![0u8; area];
        glyph.draw(|x_rel, y_rel, v| {
            let xi = x_rel as usize;
            let yi = y_rel as usize;
            if xi < w as usize && yi < h as usize {
                let c = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                data[yi * w as usize + xi] = c;
            }
        });
        CachedGlyph { min_x: bb.min.x, min_y: bb.min.y, w, h, data }
    }

    fn rasterize_glyph_pixelated(font: &Font<'static>, ch: char, display_scale: f32, pixel_size: f32) -> CachedGlyph {
        // Render at a tiny native resolution — this is where the "pixel grid" comes from.
        let tiny = Self::rasterize_glyph(font, ch, pixel_size); // e.g. pixel_size = 8.0
        if tiny.w == 0 || tiny.h == 0 {
            return tiny;
        }
        let block = (display_scale / pixel_size).max(1.0).round() as u32; // how many display-px per tiny-px
        let out_w = tiny.w * block;
        let out_h = tiny.h * block;
        let mut data = vec![0u8; (out_w * out_h) as usize];
        for y in 0..tiny.h {
            for x in 0..tiny.w {
                let v = tiny.data[(y * tiny.w + x) as usize];
                let v = if v > 128 { 255u8 } else { 0u8 }; // hard threshold at the TINY resolution
                for by in 0..block {
                    for bx in 0..block {
                        let ox = x * block + bx;
                        let oy = y * block + by;
                        data[(oy * out_w + ox) as usize] = v;
                    }
                }
            }
        }
        CachedGlyph {
            min_x: (tiny.min_x as f32 * block as f32) as i32,
            min_y: (tiny.min_y as f32 * block as f32) as i32,
            w: out_w,
            h: out_h,
            data,
        }
    }

    /// Build the full glyph atlas: every distinct character we draw, at every
    /// integer scale we use (tesseract at 24 + rain at 8..=38).
    fn build_glyph_cache(font: &Font<'static>) -> HashMap<(char, u32), CachedGlyph> {
        let mut chars = std::collections::HashSet::new();
        for b in RAMP.iter().chain(RAIN.iter()) {
            chars.insert(*b as char);
        }
        let mut map = HashMap::new();
        for ch in chars {
            for scale in 8..=38u32 {
                map.insert((ch, scale), Self::rasterize_glyph(font, ch, scale as f32));
            }
        }
        map
    }

    /// Build a glyph cache for only the RAIN characters (used by waterfalls).
    /// This allows using a different font for the Matrix-style rain.
    fn build_rain_glyph_cache(font: &Font<'static>) -> HashMap<(char, u32), CachedGlyph> {
        let mut chars = std::collections::HashSet::new();
        for b in RAIN.iter() {
            chars.insert(*b as char);
        }
        let mut map = HashMap::new();
        for ch in chars {
            for scale in 8..=38u32 {
                map.insert((ch, scale), Self::rasterize_glyph(font, ch, scale as f32));
            }
        }
        map
    }

    /// Blit a single cached glyph (at `(x, y)` top-left of its cell) to the
    /// canvas. RGB is dimmed by brightness and the stored per-pixel coverage,
    /// then copied as fast as the loop allows.
    fn draw_glyph(
        cache: &HashMap<(char, u32), CachedGlyph>,
        canvas: &mut [u8],
        width: u32,
        height: u32,
        stride: u32,
        x: f32,
        y: f32,
        ch: char,
        scale: f32,
        ascent: f32,
        bright: f32,
        base: (f32, f32, f32),
    ) {
        if bright <= 0.0 {
            return;
        }
        let key = (ch, scale.round().max(0.0) as u32);
        let Some(g) = cache.get(&key) else { return };
        if g.w == 0 || g.h == 0 {
            return;
        }
        let px0 = x as i32 + g.min_x;
        let py0 = (y + ascent * (scale / 24.0)) as i32 + g.min_y;

        // Precompute the color per unit coverage once; per pixel it's just one
        // multiply-and-store per channel.
        let cr = base.0 * bright / 255.0;
        let cg = base.1 * bright / 255.0;
        let cb = base.2 * bright / 255.0;
        let (w, h, data) = (g.w as i32, g.h as i32, &g.data);
        for yy in 0..h {
            let yi = py0 + yy;
            if yi < 0 || yi >= height as i32 {
                continue;
            }
            let row_off = yi as usize * stride as usize;
            for xx in 0..w {
                let xi = px0 + xx;
                if xi < 0 || xi >= width as i32 {
                    continue;
                }
                let cov = data[yy as usize * g.w as usize + xx as usize];
                if cov == 0 {
                    continue;
                }
                let off = row_off + xi as usize * 4;
                canvas[off] = (cr * cov as f32) as u8;
                canvas[off + 1] = (cg * cov as f32) as u8;
                canvas[off + 2] = (cb * cov as f32) as u8;
                canvas[off + 3] = 0xff;
            }
        }
    }

    /// Rasterise one rain wall. It is a solid, fully-filled column of symbols that
    /// slides down; the top edge (`wf.y`) and the glyphs below it are clipped
    /// against the screen bounds so the wall emerges from the top and exits the
    /// bottom smoothly. Brightness falls off from the leading (bottom) edge up,
    /// then is scaled by the wall's depth attenuation; the colour matches the
    /// tesseract.
    fn draw_waterfall(
        cache: &HashMap<(char, u32), CachedGlyph>,
        canvas: &mut [u8],
        width: u32,
        height: u32,
        stride: u32,
        wf: &Waterfall,
        ascent: f32,
    ) {
        let scale = 24.0 * wf.factor;
        let h = height as f32;
        // While the wall is still writing itself, the newest (bottom) slot
        // is whichever one was most recently revealed, not the final length.
        let bottom_i = (wf.grown.max(1) - 1) as i32;
        for i in 0..wf.grown {
            let y = wf.y + i as f32 * wf.pitch;
            // Skip glyphs entirely above or below the screen.
            if y + wf.pitch < 0.0 || y > h {
                continue;
            }
            // Calculate gradient position: 0.0 at bottom, 1.0 at top
            let distance_from_bottom = (bottom_i - i as i32) as f32;
            let gradient_pos = (distance_from_bottom * wf.gradient_speed).clamp(0.0, 1.0);

            // Interpolate color from RAIN_COLOR (at bottom) to RAIN_FILLER (at top)
            let color = (
                RAIN_COLOR.0 + (RAIN_FILLER.0 - RAIN_COLOR.0) * gradient_pos,
                RAIN_COLOR.1 + (RAIN_FILLER.1 - RAIN_COLOR.1) * gradient_pos,
                RAIN_COLOR.2 + (RAIN_FILLER.2 - RAIN_COLOR.2) * gradient_pos,
            );

            // Brightness falls off from the leading (bottom) edge upward
            let falloff = wf.fade.powi((bottom_i - i as i32) as i32);
            let mut bright = ((0.7 + 0.3 * falloff) * wf.atten).clamp(0.0, 1.0);

            // A flash is a smooth bump over FLASH_DURATION seconds (ramps up,
            // peaks, ramps back down), not an instant on/off — flash_t is
            // seconds remaining, so progress runs 0 -> 1 as it counts down.
            let color = if wf.flash_t[i] > 0.0 {
                let progress = 1.0 - (wf.flash_t[i] / FLASH_DURATION).clamp(0.0, 1.0);
                let pulse = (std::f32::consts::PI * progress).sin(); // 0 -> 1 -> 0
                bright = (bright + 0.55 * pulse).clamp(0.0, 1.0);
                (
                    RAIN_COLOR.0 + (220.0 - RAIN_COLOR.0) * pulse,
                    RAIN_COLOR.1 + (255.0 - RAIN_COLOR.1) * pulse,
                    RAIN_COLOR.2 + (230.0 - RAIN_COLOR.2) * pulse,
                )
            } else {
                color
            };

            // A glyph change (mutation or newly-grown symbol appearing)
            // crossfades from the previous glyph to the new one over the
            // morph window rather than popping instantly.
            if wf.morph_t[i] < 1.0 {
                let t = wf.morph_t[i].clamp(0.0, 1.0);
                if wf.prev_cells[i] != ' ' {
                    Self::draw_glyph(
                        cache, canvas, width, height, stride,
                        wf.x, y, wf.prev_cells[i], scale, ascent, bright * (1.0 - t), color,
                    );
                }
                Self::draw_glyph(
                    cache, canvas, width, height, stride,
                    wf.x, y, wf.cells[i], scale, ascent, bright * t, color,
                );
            } else {
                Self::draw_glyph(cache, canvas, width, height, stride, wf.x, y, wf.cells[i], scale, ascent, bright, color);
            }
        }
    }
}

impl FrameSource for RotatingTesseract {
    fn init(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;

        // Build the font search list: an explicit config path first, then the
        // built-in fallbacks. Strings are cloned so the list doesn't hold a
        // borrow of `self`, which would conflict with later `&mut self` calls.
        let search_paths: Vec<String> = if let Some(fp) = &self.font_path {
            std::iter::once(fp.clone())
                .chain(FONT_PATHS.iter().map(|s| s.to_string()))
                .collect()
        } else {
            FONT_PATHS.iter().map(|s| s.to_string()).collect()
        };

        let font_size = 24.0;
        let scale = Scale::uniform(font_size);

        for path in &search_paths {
            if let Ok(data) = std::fs::read(path) {
                if let Some(font) = Font::try_from_vec(data) {
                    let v_metrics = font.v_metrics(scale);
                    let char_width = font.glyph(' ').scaled(scale).h_metrics().advance_width;
                    let char_height = v_metrics.ascent - v_metrics.descent + v_metrics.line_gap;

                    let grid_cols = ((width as f32 / char_width) as u32).max(1);
                    let grid_rows = ((height as f32 / char_height) as u32).max(1);

                    let offset_x = (width as f32 - (grid_cols as f32 * char_width)) / 2.0;
                    let offset_y = (height as f32 - (grid_rows as f32 * char_height)) / 2.0;

                    self.glyphs = Self::build_glyph_cache(&font);
                    self.font = Some(font);
                    self.grid_cols = grid_cols;
                    self.grid_rows = grid_rows;
                    self.char_width = char_width;
                    self.char_height = char_height;
                    self.offset_x = offset_x;
                    self.offset_y = offset_y;
                    self.ascent = v_metrics.ascent;
                    self.start_time = Instant::now();
                    self.last_frame = Instant::now();
                    self.spawn_waterfalls();

                    log::info!("Loaded font from: {}", path);
                    
                    // Try to load a dedicated waterfall font (matrix code).
                    // Falls back to the config path or the built-in default.
                    let waterfall_path: String = self.waterfall_font_path
                        .clone()
                        .unwrap_or_else(|| WATERFALL_FONT_PATH.to_string());
                    if let Ok(wf_data) = std::fs::read(&waterfall_path) {
                        if let Some(wf_font) = Font::try_from_vec(wf_data) {
                            self.waterfall_font = Some(wf_font);
                            self.waterfall_glyphs = Self::build_rain_glyph_cache(&self.waterfall_font.as_ref().unwrap());
                            log::info!("Loaded waterfall font from: {}", waterfall_path);
                        } else {
                            log::warn!("Failed to parse waterfall font from: {}", waterfall_path);
                            // Fall back to using the main font for waterfalls
                            self.waterfall_font = self.font.clone();
                            self.waterfall_glyphs = Self::build_rain_glyph_cache(&self.font.as_ref().unwrap());
                        }
                    } else {
                        log::warn!("Waterfall font not found at: {}", waterfall_path);
                        // Fall back to using the main font for waterfalls
                        self.waterfall_font = self.font.clone();
                        self.waterfall_glyphs = Self::build_rain_glyph_cache(&self.font.as_ref().unwrap());
                    }
                    
                    return;
                }
            }
        }

        log::error!("No font found. Tesseract will not render.");
    }

    fn tick_interval(&self) -> Option<Duration> {
        if self.font.is_some() {
            // ~166 Hz pacing; the compositor/vsync caps it to the monitor rate
            // (e.g. 144 Hz). Any faster is wasted since commits don't display it.
            Some(Duration::from_micros(6000))
        } else {
            None
        }
    }

    fn render(&mut self, canvas: &mut [u8], width: u32, height: u32, stride: u32) -> bool {
        // Clear to black.
        for pixel in canvas.chunks_exact_mut(4) {
            pixel[0] = 0;
            pixel[1] = 0;
            pixel[2] = 0;
            pixel[3] = 0xff;
        }

        if !self.font.is_some() || self.grid_cols == 0 {
            return true;
        }

        // Advance the rain by per-frame elapsed time.
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;
        self.update_waterfalls(dt);

        let elapsed = self.start_time.elapsed().as_secs_f32();
        // 4D rotations through the three w-planes (the star of the show) plus
        // a slow ordinary 3D tumble so the shadow keeps moving on screen.
        let (axw, ayw, azw) = (elapsed * 0.9, elapsed * 0.6, elapsed * 0.35);
        let (ax, ay, az) = (elapsed * 0.4, elapsed * 0.7, elapsed * 0.2);

        // 1. Rotate all sixteen vertices in 4D.
        let rot4: Vec<[f32; 4]> =
            vertices().iter().map(|&v| Self::rotate4(v, axw, ayw, azw)).collect();

        // 2. Weak perspective-project 4D -> 3D. The observer sits a good way
        //    back at w = 4.0, so the far half of the hypercube shrinks and the
        //    near half enlarges only gently — enough to show the inner/outer
        //    cells sliding apart without the whole shape lunging at the camera.
        let mut p3 = [[0.0f32; 3]; 16];
        for (i, &v) in rot4.iter().enumerate() {
            p3[i] = Self::project4(v, 4.0);
        }

        // 3. Ordinary 3D rotation for extra motion.
        let mut pts3 = [[0.0f32; 3]; 16];
        for i in 0..16 {
            pts3[i] = Self::rotate(p3[i], ax, ay, az);
        }

        // 4. Map to grid with a *fixed* pixels-per-unit scale so the tesseract
        //    keeps a steady size and rotates in place. We never re-fit to the
        //    momentary extent each frame — that used to make the shape snap
        //    larger/smaller and appear to fling toward the camera. The factor
        //    below is sized for a world extent of ~2.8 units and keeps the
        //    shape smaller than the old cube.
        let units_px = (width as f32).min(height as f32) * 0.12; // px per world unit
        let kx = units_px / self.char_width; // cols per world unit
        let ky = units_px / self.char_height; // rows per world unit
        let cols_c = (self.grid_cols - 1) as f32 / 2.0;
        let rows_c = (self.grid_rows - 1) as f32 / 2.0;

        let mut grid_pts = [[0.0f32; 2]; 16];
        for (i, v) in pts3.iter().enumerate() {
            grid_pts[i][0] = v[0] * kx + cols_c;
            grid_pts[i][1] = -v[1] * ky + rows_c; // flip Y (screen is upside down)
        }

        // 5. Build the 32 stroked edges with a per-edge depth value.
        let edges = edges();
        let mut depths = [0.0f32; 32];
        for (k, e) in edges.iter().enumerate() {
            depths[k] = (pts3[e[0]][2] + pts3[e[1]][2]) * 0.5;
        }
        let dmin = depths.iter().fold(f32::INFINITY, |m, &x| m.min(x));
        let dmax = depths.iter().fold(f32::NEG_INFINITY, |m, &x| m.max(x));
        let span = (dmax - dmin).max(1e-6);

        #[derive(Clone, Copy)]
        struct Seg {
            depth: f32,
            depth_a: f32,
            depth_b: f32,
            a: [f32; 2],
            b: [f32; 2],
        }
        let mut segs: Vec<Seg> = Vec::with_capacity(32);
        for (k, e) in edges.iter().enumerate() {
            segs.push(Seg {
                depth: depths[k],
                depth_a: pts3[e[0]][2],
                depth_b: pts3[e[1]][2],
                a: grid_pts[e[0]],
                b: grid_pts[e[1]],
            });
        }

        // 6. Paint farthest -> nearest so nearer edges overlap the farther ones.
        segs.sort_by(|x, y| x.depth.partial_cmp(&y.depth).unwrap());

        let mut grid = vec![None; (self.grid_rows * self.grid_cols) as usize];
        for s in &segs {
            Self::draw_line(&mut grid, self.grid_cols, s.a, s.b, s.depth_a, s.depth_b, dmin, span);
        }

        // 6b. Stamp the 16 vertices last, unconditionally overwriting whatever
        // edge-cells piled up underneath them — vertices should always be the
        // crispest, brightest anchor points in the shape.
        for (i, p) in grid_pts.iter().enumerate() {
            Self::draw_vertex(&mut grid, self.grid_cols, self.grid_rows, *p, pts3[i][2], dmin, span);
        }

        // 7. Draw everything onto the canvas. Rain walls go down first and the
        //    tesseract is painted last, so it always sits above the rain and
        //    remains clearly visible no matter how many walls overlap it.
        let tesseract_color = TESSERACT_COLOR;

        // 7a. All rain walls (both nearer and farther than the tesseract).
        for wf in &self.waterfalls {
            Self::draw_waterfall(&self.waterfall_glyphs, canvas, width, height, stride, wf, self.ascent);
        }

        // 7b. The tesseract's cells, drawn last so the tesseract is always on top.
        for row in 0..self.grid_rows {
            for col in 0..self.grid_cols {
                let idx = (row * self.grid_cols + col) as usize;
                if idx >= grid.len() {
                    continue;
                }
                if let Some((ch, bright)) = grid[idx] {
                    let x_pixel = self.offset_x + col as f32 * self.char_width;
                    let y_pixel = self.offset_y + row as f32 * self.char_height;
                    Self::draw_glyph(
                        &self.glyphs, canvas, width, height, stride,
                        x_pixel, y_pixel, ch, 24.0, self.ascent, bright, tesseract_color,
                    );
                }
            }
        }

        true
    }
}

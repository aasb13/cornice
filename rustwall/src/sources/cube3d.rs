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
//!
//! Performance notes (this thing runs every frame, so the blit path is the
//! hot loop — several thousand glyphs are stamped per frame):
//!   * Glyphs are rasterized once and cached as coverage masks with the covered
//!     runs of each row, so blitting touches only the pixels a glyph actually
//!     covers, with no per-pixel bounds or transparency tests.
//!   * The blit clips once per glyph and then walks those runs directly.
//!   * Colour is a packed 24-bit integer and per-pixel attenuation is integer
//!     arithmetic — no floats, no divisions, no per-pixel clamping.
//!   * Glyph lookup is a plain array index, not a hash lookup, and each rain
//!     wall holds the direct-indexed glyph slice for its own scale.
//!   * A wall's per-slot colour and brightness are precomputed when it spawns,
//!     so the draw path is a lookup rather than a lerp and a `powi` per glyph.

use crate::config::{FONT_PATHS, PowerSaveConfig, WATERFALL_FONT_PATH};
use crate::source::FrameSource;
use rusttype::{Font, Scale, point};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// Classic ASCII-art ramp: brightest on the left, darkest (space) on right.
/// Wider than before (14 steps) so brightness falloff along a single edge
/// reads as a smooth taper instead of a couple of abrupt jumps.
const RAMP: &[u8] = b"@$#%8&oa*+=-:. ";

/// Glyph pool for the Matrix-style rain. Latin letters, digits and symbols so
/// every glyph exists in the bundled monospace fonts (katakana would not).
/// Characters from ASCII ranges: 33-63, 90-95, 97-126
const RAIN: &[u8] = b"!\"#$%&'()*+,-./0123456789:;<=>?@Z[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~";

/// Sentinel stored in a wall's cell buffers for "no glyph here" (a slot that
/// has not been revealed yet, or that is crossfading in from nothing). Chosen
/// outside the 33..=126 range the rain pool covers so it can never collide
/// with a real glyph index.
const NO_GLYPH: u8 = 0xff;

/// Integer glyph scales the rain atlas is built for. A wall's scale is
/// `24 * factor`, and `factor` is clamped to 0.32..=1.55, so every wall lands
/// inside 8..=37 after rounding.
const MIN_SCALE: u32 = 8;
const MAX_SCALE: u32 = 38;

/// Lookup table mapping an ASCII byte to its index in `RAMP` (used by the
/// tesseract grid, which stores ramp indices rather than characters).
const RAMP_INDEX: [u8; 128] = ramp_index_table();

const fn ramp_index_table() -> [u8; 128] {
    let mut t = [0u8; 128];
    let mut i = 0;
    while i < RAMP.len() {
        t[RAMP[i] as usize] = i as u8;
        i += 1;
    }
    t
}

/// One falling wall of rain: a solid, one-symbol-wide column of glyphs that
/// slides down smoothly and only resets once it is fully off the bottom of the
/// screen. It enters from the top (clipped, so it appears to emerge smoothly)
/// and it lives at a random depth beside the tesseract — farther walls are
/// drawn smaller and dimmer.
struct Waterfall {
    x: f32,              // wall's pixel x (left edge of the glyph cell)
    y: f32,              // top edge of the wall, in pixels (slides downward)
    len: usize,          // final number of symbols the wall grows to
    pitch: f32,          // vertical spacing between symbols, in pixels
    speed: f32,          // downward speed, pixels per second
    factor: f32,         // font scale relative to the tesseract (depth)
    atten: f32,          // overall dimming from depth, 0..1
    fade: f32,           // brightness falloff from the leading (bottom) edge upward
    gradient_speed: f32, // speed at which color transitions from RAIN_COLOR to RAIN_FILLER
    /// Glyphs for this wall's scale, indexed by rain-pool index. Resolved
    /// once at spawn so the per-frame draw path is a bare array index.
    glyphs: Vec<Arc<Glyph>>,
    /// Per-distance-from-the-bottom colour and brightness, precomputed at
    /// spawn. Slot `i` of a wall that is `grown` long reads entry
    /// `grown - 1 - i`, so a slot's look never needs recomputing per frame.
    style: Vec<SlotStyle>,
    cells: Vec<u8>, // the wall's symbols (rain-pool indices); index 0 = top, last = bottom
    prev_cells: Vec<u8>, // previous glyph for each slot, for crossfade during a change
    morph_t: Vec<f32>, // 0..1 crossfade progress from prev_cells -> cells (>=1 = settled)
    mutate_t: Vec<f32>, // per-symbol countdown (seconds) until it re-rolls to a new glyph
    flash_t: Vec<f32>, // per-symbol remaining pulse time (seconds); 0 = not flashing
    grown: usize,   // how many symbols, counting from the top, are active/visible so far
    grow_t: f32,    // countdown to the next symbol appearing at the bottom
    grow_interval: f32, // seconds between new symbols appearing while writing itself
    column: usize,  // grid column index this wall is locked to
}

/// A slot's steady-state look, indexed by distance from the wall's leading
/// (bottom) edge. Computed once per wall rather than per frame per slot.
#[derive(Clone, Copy)]
struct SlotStyle {
    rgb: (f32, f32, f32),
    bright: f32,
}

/// The font size the tesseract (and the rain's nominal size) is drawn at.
const TESSERACT_FONT_SIZE: f32 = 24.0;

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
///
/// `runs` lists every horizontal run of covered pixels, packed as
/// `row << 16 | start << 8 | end` so the hot loop reads one with a single
/// 4-byte load. Glyphs are a few dozen pixels on a side, so a byte per field is
/// ample. A row can hold more than one run (the two strokes of an `=`), which
/// is why this is a list of runs rather than one span per row: blitting walks
/// only covered pixels and never touches a transparent one, so whatever is
/// behind the glyph keeps showing through the gaps.
///
/// `row_start[y]` is the index of the first run belonging to row `y` (runs are
/// stored in row order), and `row_start[h]` is the total run count. That turns
/// the vertical clip into two lookups instead of a pair of tests per run.
struct Glyph {
    min_x: i32,
    min_y: i32,
    w: u32,
    h: u32,
    runs: Vec<u32>,
    row_start: Vec<u32>,
    data: Vec<u8>,
}

/// The sixteen corners of a unit tesseract around the origin. Index bits are
/// 0=x, 1=y, 2=z, 3=w; each coordinate is +1 when its bit is set else -1.
const VERTICES: [[f32; 4]; 16] = vertices();

const fn vertices() -> [[f32; 4]; 16] {
    let mut v = [[0.0f32; 4]; 16];
    let mut i = 0;
    while i < 16 {
        v[i] = [
            if i & 1 == 0 { -1.0 } else { 1.0 },
            if i & 2 == 0 { -1.0 } else { 1.0 },
            if i & 4 == 0 { -1.0 } else { 1.0 },
            if i & 8 == 0 { -1.0 } else { 1.0 },
        ];
        i += 1;
    }
    v
}

/// The 32 edges: each vertex connects to the four neighbours that differ in
/// exactly one coordinate, pairing each undirected edge once.
const EDGES: [[usize; 2]; 32] = edges();

const fn edges() -> [[usize; 2]; 32] {
    let mut e = [[0usize; 2]; 32];
    let mut n = 0;
    let mut v = 0;
    while v < 16 {
        let mut bit = 0;
        while bit < 4 {
            let nbr = v ^ (1 << bit);
            if v < nbr {
                e[n][0] = v;
                e[n][1] = nbr;
                n += 1;
            }
            bit += 1;
        }
        v += 1;
    }
    e
}

pub struct RotatingTesseract {
    font: Option<Font<'static>>,
    waterfall_font: Option<Font<'static>>,
    font_path: Option<String>,
    waterfall_font_path: Option<String>,
    /// Redraws per second ceiling; 0 = no cap (as fast as the compositor
    /// takes frames, i.e. the monitor's refresh rate).
    fps_cap: u32,
    /// What to scale back to while the machine is unplugged, from config.
    power_save: PowerSaveConfig,
    /// True while the machine is unplugged: the frame rate drops and the rain
    /// thins out. Driven by `set_power_saving`.
    power_saving: bool,
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
    /// The tesseract's ramp glyphs, indexed by `RAMP_INDEX`, at the tesseract's
    /// own font size. Direct-indexed, so the edge/vertex draw path never
    /// hashes.
    ramp_glyphs: Vec<Arc<Glyph>>,
    /// Rain glyphs keyed by integer font scale, each a direct-indexed slice
    /// over the rain pool. Built on demand (one scale at a time) and shared
    /// by every wall that draws at that scale.
    rain_atlas: HashMap<u32, Vec<Arc<Glyph>>>,
    /// The tesseract's cell grid, allocated once and cleared each frame.
    /// Holds `Some((ramp glyph index, brightness))` per lit cell.
    grid: Vec<Option<(u8, f32)>>,
}

impl RotatingTesseract {
    pub fn new(
        font_path: Option<String>,
        waterfall_font_path: Option<String>,
        fps_cap: u32,
        power_save: PowerSaveConfig,
    ) -> Self {
        let seed = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15);
        Self {
            font: None,
            waterfall_font: None,
            font_path,
            waterfall_font_path,
            fps_cap,
            power_save,
            power_saving: false,
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
            ramp_glyphs: Vec::new(),
            rain_atlas: HashMap::new(),
            grid: Vec::new(),
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

    fn rand_glyph(rng: &mut u64) -> u8 {
        (Self::next_rng(rng) % RAIN.len() as u64) as u8
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
    fn next_column(rng: &mut u64, cycle: &mut [usize], cursor: &mut usize) -> usize {
        if cycle.is_empty() {
            return 0;
        }
        if *cursor >= cycle.len() {
            Self::shuffle(rng, cycle);
            *cursor = 0;
        }
        let c = cycle[*cursor];
        *cursor += 1;
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
        let Self {
            waterfalls,
            rng,
            column_cycle,
            column_cursor,
            rain_atlas,
            waterfall_font,
            font,
            ..
        } = self;
        let wf_font: Option<&Font<'static>> = waterfall_font.as_ref().or(font.as_ref());
        for _ in 0..count {
            let c = Self::next_column(rng, column_cycle, column_cursor);
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
                glyphs: Vec::new(),
                style: Vec::new(),
                cells: Vec::new(),
                prev_cells: Vec::new(),
                morph_t: Vec::new(),
                mutate_t: Vec::new(),
                flash_t: Vec::new(),
                grown: 0,
                grow_t: 0.0,
                grow_interval: 0.08,
                column: c,
            };
            Self::randomise_waterfall(rng, rain_atlas, wf_font, &mut wf, base, col_w, w, h, c);
            waterfalls.push(wf);
        }
    }

    /// Randomize a wall's position, depth (size + dim), speed, length, glyphs,
    /// and precompute everything the draw path needs from it. Takes the pieces
    /// of `self` it touches rather than `&mut self`, so it can be called from
    /// both `spawn_waterfalls` and the per-frame update loop.
    fn randomise_waterfall(
        rng: &mut u64,
        rain_atlas: &mut HashMap<u32, Vec<Arc<Glyph>>>,
        font: Option<&Font<'static>>,
        wf: &mut Waterfall,
        base: f32,
        col_w: f32,
        w: f32,
        h: f32,
        col: usize,
    ) {
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
        wf.cells.clear();
        wf.cells.reserve(wf.len);
        for _ in 0..wf.len {
            wf.cells.push(Self::rand_glyph(rng));
        }
        // Every slot starts with nothing to crossfade from; slot 0 gets its
        // own fade-in the moment the wall starts growing (see below).
        wf.prev_cells.clear();
        wf.prev_cells.resize(wf.len, NO_GLYPH);
        wf.morph_t.clear();
        wf.morph_t.resize(wf.len, 1.0);
        // Mutation is rare and per-symbol so the column reads as mostly stable
        // text with the occasional glyph quietly re-deciding itself, not a
        // flicker. Each change crossfades (see draw_waterfall) instead of
        // popping.
        wf.mutate_t.clear();
        wf.mutate_t.reserve(wf.len);
        for _ in 0..wf.len {
            wf.mutate_t.push(Self::rand_range(rng, 3.0, 9.0));
        }
        wf.flash_t.clear();
        wf.flash_t.resize(wf.len, 0.0);

        // Bind this wall to the glyph set for its scale. Scales are rasterized
        // on first use and shared with every other wall at the same scale.
        let scale = ((TESSERACT_FONT_SIZE * wf.factor).round() as u32).clamp(MIN_SCALE, MAX_SCALE);
        wf.glyphs = match rain_atlas.get(&scale) {
            Some(v) => v.clone(),
            None => {
                let built: Vec<Arc<Glyph>> = match font {
                    Some(f) => (0..RAIN.len())
                        .map(|i| Arc::new(Self::rasterize_glyph(f, RAIN[i] as char, scale as f32)))
                        .collect(),
                    None => Vec::new(),
                };
                rain_atlas.insert(scale, built.clone());
                built
            }
        };

        // Precompute the per-slot colour and brightness, indexed by distance
        // from the leading (bottom) edge, so the draw path is a lookup rather
        // than a colour lerp plus a `powi` per glyph per frame.
        let mut falloff = 1.0f32;
        wf.style.clear();
        wf.style.reserve(wf.len);
        for d in 0..wf.len {
            let gp = (d as f32 * wf.gradient_speed).clamp(0.0, 1.0);
            let rgb = (
                RAIN_COLOR.0 + (RAIN_FILLER.0 - RAIN_COLOR.0) * gp,
                RAIN_COLOR.1 + (RAIN_FILLER.1 - RAIN_COLOR.1) * gp,
                RAIN_COLOR.2 + (RAIN_FILLER.2 - RAIN_COLOR.2) * gp,
            );
            let bright = ((0.7 + 0.3 * falloff) * wf.atten).clamp(0.0, 1.0);
            wf.style.push(SlotStyle { rgb, bright });
            falloff *= wf.fade;
        }

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
        let Self {
            waterfalls,
            rng,
            column_cycle,
            column_cursor,
            font,
            waterfall_font,
            rain_atlas,
            ..
        } = self;
        let wf_font: Option<&Font<'static>> = waterfall_font.as_ref().or(font.as_ref());

        for wf in waterfalls.iter_mut() {
            // Only respawn once the wall's top edge has cleared the bottom of
            // the screen — i.e. the wall is fully off-screen. It never
            // vanishes part-way down.
            let needs_respawn = wf.y > h;
            let new_col = if needs_respawn {
                Some(Self::next_column(rng, column_cycle, column_cursor))
            } else {
                None
            };

            // The whole wall slides down as a rigid, contiguous block.
            wf.y += wf.speed * dt;

            if let Some(col) = new_col {
                Self::randomise_waterfall(rng, rain_atlas, wf_font, wf, base, col_w, w, h, col);
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
                    wf.prev_cells[j] = NO_GLYPH;
                    wf.morph_t[j] = 0.0;
                    // Give the freshly-revealed glyph a long, staggered wait
                    // before it's eligible to mutate again.
                    wf.mutate_t[j] = Self::rand_range(rng, 3.0, 9.0);
                }
            }

            // A wall that hasn't started writing itself yet has no slots to
            // tick. (Off-screen walls are still ticked, so the random stream
            // stays independent of where the walls happen to be.)
            for j in 0..wf.grown {
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
                    wf.cells[j] = Self::rand_glyph(rng);
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
        grid: &mut [Option<(u8, f32)>],
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
        let inv = 1.0 / steps as f32;
        let inv_span = 1.0 / span;
        for i in 0..=steps {
            let t = i as f32 * inv;
            let x = (a[0] + (b[0] - a[0]) * t).round() as i32;
            let y = (a[1] + (b[1] - a[1]) * t).round() as i32;
            if x < 0 || y < 0 {
                continue;
            }
            let depth = depth_a + (depth_b - depth_a) * t;
            let bt = ((depth - dmin) * inv_span).clamp(0.0, 1.0);
            // Floor raised from 0.5 to 0.35 so far cells still show *some*
            // faint texture rather than snapping straight to the brightest
            // 50%-and-up range everything used to live in.
            let bright = (0.35 + 0.65 * bt).clamp(0.0, 1.0);
            let idx_ramp = ((1.0 - bright) * (RAMP.len() - 1) as f32) as u8;
            let idx = (y as u32 * cols + x as u32) as usize;
            if idx < grid.len() {
                grid[idx] = Some((idx_ramp.min(RAMP.len() as u8 - 1), bright));
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
        grid: &mut [Option<(u8, f32)>],
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
                // '@' is RAMP[0], the heavy marker.
                grid[idx] = Some((RAMP_INDEX[b'@' as usize], bright));
            }
        }
    }

    /// Perspective-project a 4D point to 3D along the w axis, with the
    /// observer at w = `dist`, the same way a 3D point projects to 2D.
    fn project4(p: [f32; 4], dist: f32) -> [f32; 3] {
        let s = dist / (dist - p[3]);
        [p[0] * s, p[1] * s, p[2] * s]
    }

    /// Rasterize one glyph at the given scale into a coverage mask. This is
    /// the expensive part (font outline tracing + AA rasterize), so it runs
    /// exactly once per (char, scale) and is cached for the life of the
    /// wallpaper; per-frame drawing only copies these bitmaps.
    fn rasterize_glyph(font: &Font<'static>, ch: char, scale: f32) -> Glyph {
        let glyph = font
            .glyph(ch)
            .scaled(Scale::uniform(scale))
            .positioned(point(0.0, 0.0));
        let Some(bb) = glyph.pixel_bounding_box() else {
            return Glyph {
                min_x: 0,
                min_y: 0,
                w: 0,
                h: 0,
                runs: Vec::new(),
                row_start: Vec::new(),
                data: Vec::new(),
            };
        };
        let w = (bb.max.x - bb.min.x).max(0) as u32;
        let h = (bb.max.y - bb.min.y).max(0) as u32;
        if w == 0 || h == 0 {
            return Glyph {
                min_x: bb.min.x,
                min_y: bb.min.y,
                w: 0,
                h: 0,
                runs: Vec::new(),
                row_start: Vec::new(),
                data: Vec::new(),
            };
        }
        let wu = w as usize;
        if w > u8::MAX as u32 || h > u8::MAX as u32 {
            // The run packing gives each field a byte. Unreachable at the
            // scales we use, but silently truncating would corrupt the mask.
            log::warn!(
                "glyph '{}' at scale {scale} is too large to cache; skipping",
                ch.escape_debug()
            );
            return Glyph {
                min_x: 0,
                min_y: 0,
                w: 0,
                h: 0,
                runs: Vec::new(),
                row_start: Vec::new(),
                data: Vec::new(),
            };
        }
        let area = wu * h as usize;
        let mut data = vec![0u8; area];
        glyph.draw(|x_rel, y_rel, v| {
            let xi = x_rel as usize;
            let yi = y_rel as usize;
            if xi < wu && yi < h as usize {
                data[yi * wu + xi] = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
        });

        // Every horizontal run of covered pixels, so the blit can skip the
        // empty margin around and between strokes without a per-pixel test.
        let mut runs = Vec::new();
        let mut row_start = Vec::with_capacity(h as usize + 1);
        for y in 0..h as usize {
            row_start.push(runs.len() as u32);
            let row = &data[y * wu..y * wu + wu];
            let mut x = 0;
            while x < wu {
                if row[x] == 0 {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < wu && row[x] != 0 {
                    x += 1;
                }
                runs.push(((y as u32) << 16) | ((start as u32) << 8) | (x as u32));
            }
        }
        row_start.push(runs.len() as u32);

        Glyph {
            min_x: bb.min.x,
            min_y: bb.min.y,
            w,
            h,
            runs,
            row_start,
            data,
        }
    }

    /// Pack a colour and brightness into the canvas's byte order
    /// (`Xrgb8888`, i.e. B in the low byte, R two bytes up) with the
    /// brightness already folded in. Returns 0 when the result would be
    /// invisible, so callers can skip the blit entirely.
    #[inline]
    fn pack(rgb: (f32, f32, f32), bright: f32) -> u32 {
        if bright <= 0.0 {
            return 0;
        }
        let b = (rgb.0 * bright).clamp(0.0, 255.0) as u32;
        let g = (rgb.1 * bright).clamp(0.0, 255.0) as u32;
        let r = (rgb.2 * bright).clamp(0.0, 255.0) as u32;
        (b << 16) | (g << 8) | r
    }

    /// Blit a cached glyph's coverage mask into the canvas with its top-left
    /// corner at `(px, py)`, tinted by the packed colour `col`.
    ///
    /// This is the hot loop, so it works in glyph-local space: the clip is
    /// resolved once into `[gx0, gx1) x [gy0, gy1)` inside the glyph's own
    /// bounding box, and the source and destination offsets then advance by a
    /// constant per row instead of being recomputed. Coverage is applied with
    /// two integer multiplies — the per-channel products fit in 16 bits each,
    /// so they never carry into the neighbouring channel, and the >>8 is a
    /// divide by 256 rather than 255, a difference of at most one level that
    /// is invisible on a glyph edge.
    #[inline]
    fn blit(
        glyph: &Glyph,
        canvas: &mut [u8],
        width: i32,
        height: i32,
        stride: usize,
        px: i32,
        py: i32,
        col: u32,
    ) {
        if col == 0 || glyph.w == 0 || glyph.h == 0 {
            return;
        }
        // Visible window, in glyph-local coordinates.
        let gx0 = if px < 0 { -px } else { 0 };
        let gx1 = (width - px).min(glyph.w as i32);
        if gx0 >= gx1 {
            return;
        }
        let gy0 = if py < 0 { -py } else { 0 };
        let gy1 = (height - py).min(glyph.h as i32);
        if gy0 >= gy1 {
            return;
        }

        // Blue sits in the low byte and red two bytes up, so one multiply can
        // attenuate both without the products interfering; green gets its own.
        let rb = ((col & 0xff) << 16) | ((col >> 16) & 0xff);
        let g = (col >> 8) & 0xff;
        let w = glyph.w as usize;
        let data = &glyph.data[..];
        let gx0u = gx0 as u32;
        let gx1u = gx1 as u32;
        // Vertical clip: two table lookups instead of a test per run.
        let lo = glyph.row_start[gy0 as usize] as usize;
        let hi = glyph.row_start[gy1 as usize] as usize;
        let mut cur_row = u32::MAX;
        let mut row_base = 0usize;

        for &run in &glyph.runs[lo..hi] {
            let row = run >> 16;
            // The destination row only moves when the row changes; written as a
            // select so the common "same row as last run" case stays
            // branch-free.
            let want = (py + row as i32) as usize * stride;
            row_base = if row != cur_row { want } else { row_base };
            cur_row = row;
            // Clip the run to the visible columns.
            let s = ((run >> 8) & 0xff).max(gx0u);
            let e = (run & 0xff).min(gx1u);
            if s < e {
                let n = (e - s) as usize;
                let cov = &data[row as usize * w + s as usize..][..n];
                let out = &mut canvas[row_base + (px + s as i32) as usize * 4..][..n * 4];
                for (out, &c) in out.chunks_exact_mut(4).zip(cov) {
                    let c = c as u32;
                    // Blue and red share one multiply: blue sits in the low
                    // byte and red in the third, so each product spans two
                    // bytes without ever carrying into the other field, and
                    // the scaled value is that product's high byte.
                    let p = rb * c;
                    let q = g * c;
                    out.copy_from_slice(&[(p >> 8) as u8, (q >> 8) as u8, (p >> 24) as u8, 0xff]);
                }
            }
        }
    }

    /// Blit a glyph into a character cell: the cell's pixel origin, plus the
    /// glyph's own bearing and the baseline shift that scales with the font
    /// size.
    #[inline]
    fn blit_cell(
        glyph: &Glyph,
        canvas: &mut [u8],
        width: i32,
        height: i32,
        stride: usize,
        cell_x: f32,
        cell_y: f32,
        ascent: f32,
        scale: f32,
        col: u32,
    ) {
        let px = cell_x as i32 + glyph.min_x;
        let py = (cell_y + ascent * (scale / TESSERACT_FONT_SIZE)) as i32 + glyph.min_y;
        Self::blit(glyph, canvas, width, height, stride, px, py, col);
    }

    /// Draw one rain wall. It is a solid, fully-filled column of symbols that
    /// slides down; the top edge (`wf.y`) and the glyphs below it are clipped
    /// against the screen bounds so the wall emerges from the top and exits the
    /// bottom smoothly. Brightness falls off from the leading (bottom) edge up,
    /// then is scaled by the wall's depth attenuation; the colour matches the
    /// tesseract.
    fn draw_waterfall(
        &self,
        canvas: &mut [u8],
        width: i32,
        height: i32,
        stride: usize,
        wf: &Waterfall,
    ) {
        let scale = TESSERACT_FONT_SIZE * wf.factor;
        let h = height as f32;
        // While the wall is still writing itself, the newest (bottom) slot
        // is whichever one was most recently revealed, not the final length.
        let bottom_i = (wf.grown.max(1) - 1) as i32;
        let wall_h = wf.grown as f32 * wf.pitch;
        // Wholly off the bottom of the screen: nothing to draw (and, at the
        // top, not even worth touching the glyph buffer).
        if wf.y > h || wf.y + wall_h < 0.0 {
            return;
        }
        let glyphs = &wf.glyphs;
        if glyphs.is_empty() {
            return;
        }

        // Vertical window of slots that can touch the screen at all. A glyph is
        // drawn at its slot's y plus the baseline shift, so the window has to
        // start one slot earlier than "the slot's own top edge is on screen" —
        // otherwise the topmost, still partly visible symbol of a wall sliding
        // in from above would be dropped.
        let first = (((-wf.y - wf.pitch) / wf.pitch).ceil().max(0.0)) as usize;
        let last = (((h - wf.y) / wf.pitch).ceil() as i32 + 1).clamp(0, wf.grown as i32) as usize;

        for i in first..last.min(wf.grown) {
            let y = wf.y + i as f32 * wf.pitch;
            // Skip glyphs entirely above or below the screen.
            if y + wf.pitch < 0.0 || y > h {
                continue;
            }
            let st = &wf.style[bottom_i as usize - i];

            // Brightness and colour come precomputed from the slot's distance
            // to the leading edge, with the flash glint folded in on top.
            let mut bright = st.bright;
            let mut color = st.rgb;
            if wf.flash_t[i] > 0.0 {
                let progress = 1.0 - (wf.flash_t[i] / FLASH_DURATION).clamp(0.0, 1.0);
                let pulse = (std::f32::consts::PI * progress).sin(); // 0 -> 1 -> 0
                bright = (bright + 0.55 * pulse).clamp(0.0, 1.0);
                color = (
                    RAIN_COLOR.0 + (220.0 - RAIN_COLOR.0) * pulse,
                    RAIN_COLOR.1 + (255.0 - RAIN_COLOR.1) * pulse,
                    RAIN_COLOR.2 + (230.0 - RAIN_COLOR.2) * pulse,
                );
            }

            // A glyph change (mutation or newly-grown symbol appearing)
            // crossfades from the previous glyph to the new one over the
            // morph window rather than popping instantly.
            if wf.morph_t[i] < 1.0 {
                let t = wf.morph_t[i].clamp(0.0, 1.0);
                let prev = wf.prev_cells[i];
                if prev != NO_GLYPH {
                    if let Some(g) = glyphs.get(prev as usize) {
                        Self::blit_cell(
                            g,
                            canvas,
                            width,
                            height,
                            stride,
                            wf.x,
                            y,
                            self.ascent,
                            scale,
                            Self::pack(color, bright * (1.0 - t)),
                        );
                    }
                }
                if let Some(g) = glyphs.get(wf.cells[i] as usize) {
                    Self::blit_cell(
                        g,
                        canvas,
                        width,
                        height,
                        stride,
                        wf.x,
                        y,
                        self.ascent,
                        scale,
                        Self::pack(color, bright * t),
                    );
                }
            } else if let Some(g) = glyphs.get(wf.cells[i] as usize) {
                Self::blit_cell(
                    g,
                    canvas,
                    width,
                    height,
                    stride,
                    wf.x,
                    y,
                    self.ascent,
                    scale,
                    Self::pack(color, bright),
                );
            }
        }
    }
}

impl RotatingTesseract {
    /// The frame-rate ceiling in force right now. On battery the ceiling drops
    /// to `power_save.fps_cap`; a battery cap of 0 means "don't cap on
    /// battery", leaving `fps_cap` (or the display's refresh rate) in charge.
    fn fps_cap_now(&self) -> u32 {
        if self.power_saving && self.power_save.fps_cap > 0 {
            self.power_save.fps_cap
        } else {
            self.fps_cap
        }
    }

    /// Whether the rain wall at `index` should be drawn this frame. While
    /// power saving only every `power_save.rain_divisor`th wall is: walls are
    /// handed screen columns round-robin, so stepping through the list thins
    /// the rain evenly over the whole screen instead of punching holes in it.
    /// The walls that are skipped keep falling and mutating, so full speed
    /// comes back instantly on the next frame after plugging in.
    #[inline]
    fn rain_wall_drawn(&self, index: usize) -> bool {
        let step = if self.power_saving {
            self.power_save.rain_divisor
        } else {
            1
        };
        step <= 1 || index.is_multiple_of(step as usize)
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

        let font_size = TESSERACT_FONT_SIZE;
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

                    // Only the ramp characters are needed, and only at the
                    // tesseract's own size.
                    self.ramp_glyphs = RAMP
                        .iter()
                        .map(|&b| Arc::new(Self::rasterize_glyph(&font, b as char, font_size)))
                        .collect();
                    self.grid = vec![None; (grid_rows * grid_cols) as usize];
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

                    log::info!("Loaded font from: {}", path);

                    // Try to load a dedicated waterfall font (matrix code).
                    // Falls back to the config path or the built-in default.
                    let waterfall_path: String = self
                        .waterfall_font_path
                        .clone()
                        .unwrap_or_else(|| WATERFALL_FONT_PATH.to_string());
                    let wf_font = match std::fs::read(&waterfall_path) {
                        Ok(wf_data) => match Font::try_from_vec(wf_data) {
                            Some(f) => {
                                log::info!("Loaded waterfall font from: {}", waterfall_path);
                                f
                            }
                            None => {
                                log::warn!(
                                    "Failed to parse waterfall font from: {}",
                                    waterfall_path
                                );
                                self.font.as_ref().unwrap().clone()
                            }
                        },
                        Err(_) => {
                            log::warn!("Waterfall font not found at: {}", waterfall_path);
                            self.font.as_ref().unwrap().clone()
                        }
                    };
                    self.waterfall_font = Some(wf_font);
                    self.rain_atlas.clear();

                    // Last, so the rain walls can bind their glyph slices from
                    // the waterfall font we just settled on.
                    self.spawn_waterfalls();

                    return;
                }
            }
        }

        log::error!("No font found. Tesseract will not render.");
    }

    fn tick_interval(&self) -> Option<Duration> {
        let cap = self.fps_cap_now();
        self.font.as_ref()?;
        if cap > 0 {
            return Some(Duration::from_secs_f64(1.0 / cap as f64));
        }
        // ~166 Hz pacing; the compositor/vsync caps it to the monitor rate
        // (e.g. 144 Hz). Any faster is wasted since commits don't display it.
        Some(Duration::from_micros(6000))
    }

    fn dbg_atlas_len(&self) -> usize {
        self.rain_atlas.values().map(|v| v.len()).sum()
    }

    fn set_power_saving(&mut self, on_battery: bool) {
        self.power_saving = on_battery;
    }

    fn render(&mut self, canvas: &mut [u8], width: u32, height: u32, stride: u32) -> bool {
        // Clear to black. The fourth byte of an `Xrgb8888` pixel is unused, so
        // the whole buffer can go in one memset-shaped fill.
        canvas.fill(0);

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
        let mut rot4 = [[0.0f32; 4]; 16];
        for (dst, v) in rot4.iter_mut().zip(VERTICES.iter()) {
            *dst = Self::rotate4(*v, axw, ayw, azw);
        }

        // 2. Weak perspective-project 4D -> 3D. The observer sits a good way
        //    back at w = 4.0, so the far half of the hypercube shrinks and the
        //    near half enlarges only gently — enough to show the inner/outer
        //    cells sliding apart without the whole shape lunging at the camera.
        let mut p3 = [[0.0f32; 3]; 16];
        for (dst, v) in p3.iter_mut().zip(rot4.iter()) {
            *dst = Self::project4(*v, 4.0);
        }

        // 3. Ordinary 3D rotation for extra motion.
        let mut pts3 = [[0.0f32; 3]; 16];
        for (dst, v) in pts3.iter_mut().zip(p3.iter()) {
            *dst = Self::rotate(*v, ax, ay, az);
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
        for (dst, v) in grid_pts.iter_mut().zip(pts3.iter()) {
            dst[0] = v[0] * kx + cols_c;
            dst[1] = -v[1] * ky + rows_c; // flip Y (screen is upside down)
        }

        // 5. Per-edge depth values.
        let mut depths = [0.0f32; 32];
        for (k, e) in EDGES.iter().enumerate() {
            depths[k] = (pts3[e[0]][2] + pts3[e[1]][2]) * 0.5;
        }
        let dmin = depths.iter().fold(f32::INFINITY, |m, &x| m.min(x));
        let dmax = depths.iter().fold(f32::NEG_INFINITY, |m, &x| m.max(x));
        let span = (dmax - dmin).max(1e-6);

        // The cell grid is kept between frames to avoid reallocating it, so it
        // has to be wiped here: every cell lit by the *previous* frame's edges
        // and vertices is still set, and without this the shape smears into a
        // solid blob and keeps drawing cells forever.
        self.grid.fill(None);

        // 6. Paint farthest -> nearest so nearer edges overlap the farther
        //    ones: a 32-byte index array sorted by depth, rather than building
        //    and sorting a vector of segment structs every frame.
        let mut order = [0u8; 32];
        for (i, o) in order.iter_mut().enumerate() {
            *o = i as u8;
        }
        order.sort_unstable_by(|&a, &b| {
            depths[a as usize]
                .partial_cmp(&depths[b as usize])
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let cols = self.grid_cols;
        for &k in order.iter() {
            let e = EDGES[k as usize];
            Self::draw_line(
                &mut self.grid,
                cols,
                grid_pts[e[0]],
                grid_pts[e[1]],
                pts3[e[0]][2],
                pts3[e[1]][2],
                dmin,
                span,
            );
        }

        // 6b. Stamp the 16 vertices last, unconditionally overwriting whatever
        // edge-cells piled up underneath them — vertices should always be the
        // crispest, brightest anchor points in the shape.
        for (i, p) in grid_pts.iter().enumerate() {
            Self::draw_vertex(
                &mut self.grid,
                cols,
                self.grid_rows,
                *p,
                pts3[i][2],
                dmin,
                span,
            );
        }

        // 7. Draw everything onto the canvas. Rain walls go down first and the
        //    tesseract is painted last, so it always sits above the rain and
        //    remains clearly visible no matter how many walls overlap it.
        let w = width as i32;
        let h = height as i32;
        let stride = stride as usize;

        // 7a. All rain walls (both nearer and farther than the tesseract),
        //     or a thinned-out subset of them while on battery.
        for (i, wf) in self.waterfalls.iter().enumerate() {
            if !self.rain_wall_drawn(i) {
                continue;
            }
            self.draw_waterfall(canvas, w, h, stride, wf);
        }

        // 7b. The tesseract's cells, drawn last so the tesseract is always on top.
        let ramp_glyphs = &self.ramp_glyphs;
        let cell_w = self.char_width;
        let cell_h = self.char_height;
        let ox = self.offset_x;
        let oy = self.offset_y;
        let ascent = self.ascent;
        for (idx, cell) in self.grid.iter().enumerate() {
            let Some((gi, bright)) = *cell else { continue };
            let Some(g) = ramp_glyphs.get(gi as usize) else {
                continue;
            };
            let col = idx as u32 % cols;
            let row = idx as u32 / cols;
            Self::blit_cell(
                g,
                canvas,
                w,
                h,
                stride,
                ox + col as f32 * cell_w,
                oy + row as f32 * cell_h,
                ascent,
                TESSERACT_FONT_SIZE,
                Self::pack(TESSERACT_COLOR, bright),
            );
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tesseract's cell grid is kept between frames (so it doesn't have to
    /// be reallocated every frame), which means `render` has to wipe it. If it
    /// doesn't, every cell the shape has *ever* visited stays lit: the
    /// wireframe smears into a solid blob and the lit-cell count ratchets up
    /// frame after frame until the whole grid is being blitted — which is both
    /// the "cube became a sphere" bug and a growing pile of work per frame.
    ///
    /// The check is phase-independent: measure the lit cells at some rotation,
    /// spin for several seconds, then come back to the *same* rotation. A grid
    /// that is wiped each frame lands on the same number of cells it started
    /// with; a stale one is stuck at the union of every phase it ever drew
    /// (~900 cells at 1080p, versus ~150-300 for a clean frame).
    #[test]
    fn tesseract_cell_grid_is_reset_every_frame() {
        let mut s = RotatingTesseract::new(None, None, 0, PowerSaveConfig::default());
        s.init(1920, 1080);
        if s.font.is_none() {
            // No system font in this environment; nothing to render.
            return;
        }
        // Tesseract only: this is about the cell grid, not the rain.
        s.waterfalls.clear();

        const W: u32 = 1920;
        const H: u32 = 1080;
        let stride = W * 4;
        let mut canvas = vec![0u8; (stride * H) as usize];
        let base = Instant::now();
        let lit = |s: &RotatingTesseract| s.grid.iter().filter(|c| c.is_some()).count();

        let at_phase_zero = |s: &mut RotatingTesseract, canvas: &mut [u8]| {
            s.start_time = base;
            s.last_frame = base - Duration::from_secs_f64(1.0);
            s.render(canvas, W, H, stride);
        };

        at_phase_zero(&mut s, &mut canvas);
        let first = lit(&s);
        assert!(first > 0, "tesseract drew nothing at all");

        // Five seconds of rotation, so the shape sweeps a wide arc of the grid.
        for f in 1..300u32 {
            s.start_time = base - Duration::from_secs_f64(f64::from(f) / 60.0);
            s.last_frame = base - Duration::from_secs_f64(1.0);
            s.render(&mut canvas, W, H, stride);
        }

        // Back where we started.
        at_phase_zero(&mut s, &mut canvas);
        let again = lit(&s);
        assert!(
            again <= first * 3 / 2 + 16,
            "lit cells went {first} -> {again} after returning to the same rotation: \
             the cell grid is not being cleared between frames"
        );
    }

    /// A `fps_cap` of 0 means "no cap" (the old ~166Hz pacing, which the
    /// compositor throttles to the display's refresh rate); any other value
    /// paces the redraw loop to exactly that many frames per second.
    #[test]
    fn tick_interval_honours_fps_cap() {
        let mut capped = RotatingTesseract::new(None, None, 60, PowerSaveConfig::default());
        capped.init(640, 480);
        if capped.font.is_none() {
            // No system font in this environment; nothing to pace.
            return;
        }
        assert_eq!(
            capped.tick_interval(),
            Some(Duration::from_secs_f64(1.0 / 60.0))
        );

        let mut uncapped = RotatingTesseract::new(None, None, 0, PowerSaveConfig::default());
        uncapped.init(640, 480);
        assert_eq!(uncapped.tick_interval(), Some(Duration::from_micros(6000)));
    }

    /// Going onto battery must drop to the battery frame rate, and plugging
    /// back in must restore the configured one — not leave the wallpaper
    /// stuck at 30fps forever.
    #[test]
    fn power_saving_drops_and_restores_the_frame_rate() {
        let ps = PowerSaveConfig {
            enabled: true,
            fps_cap: 30,
            rain_divisor: 4,
        };
        let mut s = RotatingTesseract::new(None, None, 60, ps);
        s.init(640, 480);
        if s.font.is_none() {
            return;
        }
        let on_mains = s.tick_interval();
        s.set_power_saving(true);
        let on_battery = s.tick_interval();
        s.set_power_saving(false);
        let restored = s.tick_interval();

        assert_eq!(on_mains, Some(Duration::from_secs_f64(1.0 / 60.0)));
        assert_eq!(on_battery, Some(Duration::from_secs_f64(1.0 / 30.0)));
        assert_eq!(restored, on_mains);
    }

    /// A battery frame-rate cap of 0 means "leave the normal cap alone".
    #[test]
    fn zero_battery_fps_cap_keeps_the_normal_cap() {
        let ps = PowerSaveConfig {
            enabled: true,
            fps_cap: 0,
            rain_divisor: 4,
        };
        let mut s = RotatingTesseract::new(None, None, 60, ps);
        s.init(640, 480);
        if s.font.is_none() {
            return;
        }
        s.set_power_saving(true);
        assert_eq!(s.tick_interval(), Some(Duration::from_secs_f64(1.0 / 60.0)));
    }

    /// The rain is thinned by keeping every Nth wall, so the walls that remain
    /// are spread right across the screen rather than clustered.
    #[test]
    fn power_saving_thins_the_rain_evenly() {
        let ps = PowerSaveConfig {
            enabled: true,
            fps_cap: 30,
            rain_divisor: 4,
        };
        let mut s = RotatingTesseract::new(None, None, 60, ps);

        // On mains: everything.
        s.set_power_saving(false);
        assert!(
            (0..64).all(|i| s.rain_wall_drawn(i)),
            "no thinning on mains"
        );

        // On battery: one in four, spread out.
        s.set_power_saving(true);
        let kept: Vec<usize> = (0..64).filter(|&i| s.rain_wall_drawn(i)).collect();
        assert_eq!(
            kept,
            vec![0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52, 56, 60]
        );
    }

    /// A divisor of 1 (or 0, which is nonsense config) must mean "no
    /// thinning" rather than "no rain at all".
    #[test]
    fn rain_divisor_of_one_keeps_everything() {
        for divisor in [0, 1] {
            let ps = PowerSaveConfig {
                enabled: true,
                fps_cap: 30,
                rain_divisor: divisor,
            };
            let mut s = RotatingTesseract::new(None, None, 60, ps);
            s.set_power_saving(true);
            assert!(
                (0..32).all(|i| s.rain_wall_drawn(i)),
                "divisor {divisor} dropped rain"
            );
        }
    }
}

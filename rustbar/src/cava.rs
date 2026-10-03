//! Cava audio-visualizer module — an oscilloscope-style **trace** of the live
//! spectrum, drawn inside the same bordered box every other module draws.
//!
//! Waybar links **libcava** in-process: it sets `ascii_range =
//! format-icons.len() - 1` (== 7), output method = RAW, reads `audio_raw.bars[]`
//! directly, and maps each value to a `format-icons` glyph (▁▂▃▄▅▆▇█). Rust has
//! no libcava bindings, so instead we spawn `cava` as a subprocess configured
//! for **raw ASCII output to stdout** (one frame per line, space-separated
//! values), read it on a background thread, and draw the latest frame.
//!
//! The generated cava config mirrors the waybar `cava` block for everything
//! that decides *what the signal is*:
//!   framerate=120, autosens=1, bars=14, lower_cutoff_freq=50,
//!   higher_cutoff_freq=10000, method=pipewire, source=auto, noise_reduction=77
//! (note: cava reads `noise_reduction` on a 0–100 scale and divides by 100
//! internally — `config.c` does `p->noise_reduction /= 100` — so the file value
//! `77` reproduces waybar's `0.77` exactly).
//!
//! IMPORTANT — `sensitivity` is intentionally OMITTED even though waybar's block
//! had `"sensitivity": 1`. Waybar links libcava in-process and sets `prm.sens`
//! to the raw value (1) directly — no `/100`. But the `cava` CLI parses the
//! config via `config.c`, which does `p->sens = sensitivity / 100`, so a config
//! value of `1` becomes `0.01` (near-silent) and the new-style `autosens` ramps
//! up far too slowly from there → everything stays at 0 even with music playing.
//! With `autosens = 1` cava manages sensitivity dynamically from the default
//! (100 → 1.0), which reproduces waybar's *intent* (autosens on) and actually
//! works.
//!
//! What deliberately **diverges** from waybar is the rendering. Waybar's
//! `ascii_range` of 7 exists only because it had 8 block glyphs to map onto; a
//! 14-point polyline has no such ladder to climb, so [`ASCII_MAX`] is opened up
//! and the curve stops being a 14-step staircase. Drawing the frame as one
//! connected stroke rather than 14 fat glyphs is also what makes it read as an
//! instrument: the box's 1px rules, the workspace slots and the clock are all
//! thin strokes on flat panels, and a row of solid blocks was the one element on
//! the bar that broke that language.
//!
//! Box padding (8px h-padding, 1px accent border) matches `#cava` in CSS; the
//! fill is the bar background like every other module.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use calloop::channel::Sender;

use crate::config::Colors;
use crate::theme::{draw_bordered_box, draw_line, fill_rect};

/// Number of bars (waybar `cava` `bars: 14`) — and so the number of vertices in
/// the trace, low frequencies on the left.
const BARS: usize = 14;
/// Max value cava emits: the `ascii_max_range` in [`CAVA_CONFIG`]. Waybar's 7
/// was dictated by its glyph table; here it only sets the vertical scale.
const ASCII_MAX: u8 = 100;
/// Horizontal distance between adjacent vertices, in pixels. Fixed rather than
/// derived from the box, so the trace's slope depends on the music and not on
/// the bar's height.
const PITCH: u32 = 6;
/// waybar `#cava` CSS `padding: 0 8px`.
const H_PAD: u32 = 8;
/// 1px accent border (waybar `#cava` `border: 1px solid rgb(191,255,217)`).
const BORDER: u32 = 1;

/// Envelope applied to each band before it is drawn: fast attack, slow release,
/// as a fraction of the remaining gap to the target. Music is full of transients
/// that would otherwise make the trace strobe.
const ATTACK: f32 = 0.55;
const RELEASE: f32 = 0.14;

/// How far a peak-hold tick falls per rendered frame, as a fraction of full
/// scale. Frames arrive at the configured 120/s, so this is a ~1.4s fall from
/// full — long enough to read as "this was the peak a moment ago".
const PEAK_FALL: f32 = 0.012;
/// Peak ticks are wider than the 1px trace so they read as markers on it rather
/// than as part of it.
const PEAK_W: u32 = 2;

/// Width of the plot area in px: the 13 gaps between 14 vertices at [`PITCH`],
/// plus the 1px the last vertex's stroke occupies.
const PLOT_W: u32 = (BARS as u32 - 1) * PITCH + 1;
/// Total box width, so [`Self::width`] and the render agree by construction.
const BOX_W: u32 = 2 * BORDER + 2 * H_PAD + PLOT_W;

/// Config written to a temp file and passed to `cava -p`. Every value mirrors the
/// waybar config.jsonc `cava` block; the `[output]` section forces the raw ASCII
/// stdout protocol we parse (one line per frame, space-separated 0..=7 values).
const CAVA_CONFIG: &str = "\
[general]\n\
framerate = 120\n\
autosens = 1\n\
bars = 14\n\
lower_cutoff_freq = 50\n\
higher_cutoff_freq = 10000\n\
sleep_timer = 5\n\
\n\
[input]\n\
method = pipewire\n\
source = auto\n\
\n\
[output]\n\
method = raw\n\
raw_target = /dev/stdout\n\
data_format = ascii\n\
ascii_max_range = 100\n\
bar_delimiter = 32\n\
frame_delimiter = 10\n\
\n\
[smoothing]\n\
noise_reduction = 77\n";

pub struct Cava {
    /// Latest frame (BARS values, 0..=ASCII_MAX), shared with the reader thread.
    bars: Arc<Mutex<Vec<u8>>>,
    /// cava subprocess — killed on drop so the reader thread's stdout EOFs and
    /// the child doesn't outlive the bar.
    child: Option<Child>,
    /// Temp config file path — removed on drop.
    config_path: PathBuf,
    /// Last-rendered box geometry (x_start, x_end), for hit-testing clicks.
    layout: (u32, u32),
    /// SIGSTOP/SIGCONT pause state — toggled by right-click (waybar
    /// `on-click-right: mode`).
    paused: bool,
    /// Per-band envelope follower, 0..=1. Kept here rather than in the reader
    /// thread because it is a *rendering* concern — the reader's job is to hand
    /// over the raw measurement, and a frame that arrives while the bar is
    /// behind should not be integrated twice.
    envelope: Vec<f32>,
    /// Per-band peak hold, 0..=1, decaying toward the live envelope each frame.
    peaks: Vec<f32>,
}

impl Cava {
    pub fn new() -> Self {
        Self {
            bars: Arc::new(Mutex::new(vec![0u8; BARS])),
            child: None,
            config_path: PathBuf::new(),
            layout: (0, 0),
            paused: false,
            envelope: vec![0.0; BARS],
            peaks: vec![0.0; BARS],
        }
    }

    /// Total width of the box this module draws. The bar lays the visualiser out at
    /// a fixed offset from the workspaces rather than from a measured width, so
    /// this exists for the tests that assert nothing escapes the box — the same
    /// job [`crate::workspaces::WorkspaceList::width`] does, but nobody outside
    /// them asks.
    #[cfg(test)]
    pub fn width(&self) -> u32 {
        BOX_W
    }

    /// Replace the latest frame as if the reader thread had delivered it. The
    /// rendering tests need a known signal, and the alternative — waiting on a
    /// live pipewire monitor — makes them depend on what is playing.
    #[cfg(test)]
    pub fn set_frame(&mut self, frame: &[u8]) {
        let mut bars = self.bars.lock().unwrap();
        bars.clear();
        bars.extend(frame.iter().copied().map(|v| v.min(ASCII_MAX)));
        bars.resize(BARS, 0);
    }

    /// Spawn the cava subprocess and the stdout reader thread. Each parsed frame
    /// triggers a bar redraw via `redraw_tx` (same calloop channel pattern as the
    /// tray). Fails gracefully (logs + renders an empty box) if `cava` is absent.
    pub fn spawn(&mut self, redraw_tx: Sender<()>) {
        // PID-suffixed temp file — unique per bar instance, cleaned up on drop
        // (no extra crate needed for a secure tempfile).
        let config_path =
            std::env::temp_dir().join(format!("rustbar-cava-{}.conf", std::process::id()));
        if let Err(err) = std::fs::write(&config_path, CAVA_CONFIG) {
            log::error!("cava: failed to write temp config {:?}: {err}", config_path);
            return;
        }
        self.config_path = config_path.clone();

        let mut child = match Command::new("cava")
            .arg("-p")
            .arg(&config_path)
            .stdout(Stdio::piped())
            // surface cava errors (e.g. pipewire capture failures) on the bar's
            // stderr rather than swallowing them.
            .stderr(Stdio::inherit())
            .spawn()
        {
            Ok(c) => c,
            Err(err) => {
                log::error!("cava: failed to spawn `cava` binary: {err}");
                let _ = std::fs::remove_file(&config_path);
                self.config_path = PathBuf::new();
                return;
            }
        };

        let stdout = child.stdout.take().expect("piped stdout");
        let bars_bg = self.bars.clone();
        thread::spawn(move || reader_loop(stdout, bars_bg, redraw_tx));

        self.child = Some(child);
    }

    /// Advance the envelope and peak-hold toward the latest frame. One step per
    /// render, so the smoothing rate is tied to how often the bar actually
    /// draws rather than to a wall clock — a dropped frame slows the decay
    /// instead of jumping it.
    fn advance(&mut self) {
        let frame = self.bars.lock().unwrap().clone();
        for (i, &raw) in frame.iter().enumerate() {
            let target = f32::from(raw) / f32::from(ASCII_MAX);
            let cur = self.envelope[i];
            let k = if target > cur { ATTACK } else { RELEASE };
            let next = cur + (target - cur) * k;
            self.envelope[i] = next;
            // A held peak ratchets up instantly and falls at its own rate; it is
            // not pulled toward the envelope, or it would just shadow it.
            self.peaks[i] = if next >= self.peaks[i] {
                next
            } else {
                (self.peaks[i] - PEAK_FALL).max(next)
            };
        }
    }

    /// Render the `#cava` box (background fill + 1px accent border, 8px
    /// h-padding) with the live spectrum drawn inside it as a single connected
    /// trace: a 1px accent stroke through one vertex per band, a soft gradient
    /// wash filling down to the baseline under it, and a peak-hold tick per band.
    pub fn render(
        &mut self,
        colors: &Colors,
        canvas: &mut [u8],
        x_offset: u32,
        height: u32,
        stride: u32,
    ) {
        self.advance();
        self.layout = (x_offset, x_offset + BOX_W);
        let surface_w = stride / 4;

        // Paused: the trace is still drawn (a frozen scope is more legible than
        // an empty box) but pulled most of the way back toward the background,
        // so the state is obvious without needing a label. `inactive` is the
        // palette's own "off" colour — the same one empty workspace slots use.
        let stroke = if self.paused {
            colors.accent.lerp(colors.background, 0.7)
        } else {
            colors.accent
        };

        draw_bordered_box(
            canvas,
            stride,
            x_offset,
            0,
            BOX_W,
            height,
            colors.background,
            colors.accent,
        );

        // Plot area: inside the border and the h-padding, and leaving the bottom
        // row clear for the baseline rule so the trace always has something to
        // sit on instead of merging into the border.
        let x0 = x_offset + BORDER + H_PAD;
        let top = BORDER;
        let baseline = height.saturating_sub(1 + BORDER);
        let plot_h = baseline.saturating_sub(top);
        // Fewer than two usable rows and there is no room for a curve *and* the
        // baseline rule it sits on.
        if plot_h < 2 {
            return;
        }

        // The baseline itself, dimmed well back from the accent so it reads as
        // a rule and not as part of the signal.
        fill_rect(
            canvas,
            stride,
            x0,
            baseline,
            PLOT_W,
            1,
            stroke.lerp(colors.background, 0.72),
            255,
        );

        let y_at = |v: f32| -> i32 {
            // The scale spans `plot_h - 1` rows, not `plot_h`: v=0 lands one row
            // clear of the baseline rule (so silence is a visible flat line, not
            // the rule itself) and v=1 lands exactly on the first interior row,
            // leaving the box's border unpainted.
            let span = (plot_h - 1) as f32;
            ((baseline - 1) as f32 - v.clamp(0.0, 1.0) * span).round() as i32
        };

        // Area wash under the trace, one column per pixel: each column is filled
        // from the trace down to the baseline with a colour that fades toward the
        // background as it descends. Walking the *curve* per column rather than
        // the bands keeps the fill continuous under a steep segment, which is
        // most of what sells it as a trace rather than 14 separate columns.
        for px in 0..PLOT_W {
            let t = px as f32 / PITCH as f32;
            let i = (t.floor() as usize).min(BARS - 2);
            let frac = t - i as f32;
            let v = self.envelope[i] + (self.envelope[i + 1] - self.envelope[i]) * frac;
            let y = y_at(v);
            let col = x0 + px;
            for row in y.max(top as i32)..baseline as i32 {
                // Brightest just under the curve, gone by the baseline.
                let depth = (row - y) as f32 / plot_h.max(1) as f32;
                fill_rect(
                    canvas,
                    stride,
                    col,
                    row as u32,
                    1,
                    1,
                    stroke.lerp(colors.background, 0.55 + 0.45 * depth.min(1.0)),
                    255,
                );
            }
        }

        // Peak-hold ticks, before the stroke: a tick sitting exactly on the live
        // value is redundant, and drawing it last would put a dimmer pixel in
        // the middle of the brightest line on the bar.
        let tick = stroke.lerp(colors.background, 0.25);
        for (i, &peak) in self.peaks.iter().enumerate() {
            let y = y_at(peak);
            if y < top as i32 {
                continue;
            }
            let x = x0 + i as u32 * PITCH;
            fill_rect(canvas, stride, x, y as u32, PEAK_W, 1, tick, 255);
        }

        // The trace itself: one 1px stroke vertex-to-vertex.
        for i in 1..BARS {
            let (xa, ya) = (x0 + (i as u32 - 1) * PITCH, y_at(self.envelope[i - 1]));
            let (xb, yb) = (x0 + i as u32 * PITCH, y_at(self.envelope[i]));
            draw_line(
                canvas,
                stride,
                (surface_w, height),
                xa as i32,
                ya,
                xb as i32,
                yb,
                stroke,
            );
        }
    }

    /// Whether the given bar-local x falls inside the last-rendered cava box.
    pub fn hit_test(&self, x: f64) -> bool {
        let x = x as u32;
        let (start, end) = self.layout;
        x >= start && x < end
    }

    /// Toggle pause/resume — the subprocess analogue of waybar's
    /// `on-click-right: mode` (which calls libcava `pauseResume`). SIGSTOP
    /// freezes cava on its last frame; SIGCONT resumes. Uses the `kill` util so
    /// we don't need a `libc`/`nix` dependency.
    pub fn toggle_pause(&mut self) {
        let Some(child) = self.child.as_ref() else {
            return;
        };
        let pid = child.id();
        self.paused = !self.paused;
        let sig = if self.paused { "STOP" } else { "CONT" };
        if let Err(err) = Command::new("kill")
            .args([format!("-{sig}"), pid.to_string()])
            .status()
        {
            log::error!("cava: failed to {sig} pid {pid}: {err}");
            self.paused = !self.paused; // roll back the tracked state
        }
    }
}

impl Drop for Cava {
    fn drop(&mut self) {
        // Kill cava so the reader thread's blocking read() returns EOF (Ok(0))
        // and the child doesn't outlive the bar, then reap it and clean up the
        // temp config.
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if !self.config_path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.config_path);
        }
    }
}

/// Reads cava's raw ASCII stdout on a dedicated thread. Each complete line
/// (frame, terminated by `frame_delimiter = '\n'`) is parsed into BARS values
/// (0..=ASCII_MAX, space-separated), stored as the latest frame, and a redraw
/// is signaled. Exits when stdout EOFs (cava exited / was killed).
fn reader_loop(
    mut stdout: std::process::ChildStdout,
    bars: Arc<Mutex<Vec<u8>>>,
    redraw_tx: Sender<()>,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        match stdout.read(&mut chunk) {
            Ok(0) => break, // EOF — cava exited or was killed
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                // Process every complete line now in the buffer.
                while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                    let line = String::from_utf8_lossy(&buf[..nl]).into_owned();
                    buf.drain(..=nl);
                    if let Some(frame) = parse_frame(&line) {
                        if frame.len() == BARS {
                            *bars.lock().unwrap() = frame;
                            let _ = redraw_tx.send(());
                        }
                    }
                }
            }
            Err(err) => {
                log::error!("cava: stdout read error: {err}");
                break;
            }
        }
    }
}

/// Parse one cava frame line (`"v0 v1 … v13 "`) into BARS values clamped to
/// ASCII_MAX. Returns None on any malformed token so the frame is skipped.
fn parse_frame(line: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(BARS);
    for tok in line.split_whitespace() {
        let v: u8 = tok.parse().ok()?;
        out.push(v.min(ASCII_MAX));
    }
    Some(out)
}

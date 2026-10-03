//! Cava audio-visualizer module — replicates the waybar `cava` module from
//! `~/.config/waybar/config.jsonc` (+ `#cava` styling in `style.css`).
//!
//! Waybar links **libcava** in-process: it sets `ascii_range =
//! format-icons.len() - 1` (== 7), output method = RAW, reads
//! `audio_raw.bars[]` (0..=7) directly, and maps each value to a
//! `format-icons` glyph (▁▂▃▄▅▆▇█). Rust has no libcava bindings, so instead we
//! spawn `cava` as a subprocess configured for **raw ASCII output to stdout**
//! (one frame per line, space-separated values 0..=7), read it on a background
//! thread, and render the latest frame with the same block characters.
//!
//! The generated cava config mirrors the waybar `cava` block exactly:
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
//! up far too slowly from there → bars stay at 0 even with music playing. With
//! `autosens = 1` cava manages sensitivity dynamically from the default (100 →
//! 1.0), which reproduces waybar's *intent* (autosens on) and actually works.
//! `format-icons` and the box padding (8px h-padding, 1px accent border) match
//! `#cava` in CSS, except the fill: waybar had `background: transparent` there
//! and this bar gives cava the same background as every other module.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use calloop::channel::Sender;
use rusttype::Scale;

use crate::theme::{self, accent, bg, draw_bordered_box, draw_text, text_width};

/// Number of bars (waybar `cava` `bars: 14`).
const BARS: usize = 14;
/// Max value cava emits — `format-icons.len() - 1` == 7 (waybar sets
/// `ascii_range` to this; here it's the cava config `ascii_max_range`).
const ASCII_MAX: u8 = 7;
/// waybar `cava` `format-icons`: the 8 Unicode block elements, lowest→highest.
///   ▁ U+2581 lower one eighth block … █ U+2588 full block.
const FORMAT_ICONS: [char; 8] = [
    '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2587}', '\u{2588}',
];
/// waybar `#cava` CSS `padding: 0 8px`.
const H_PAD: u32 = 8;
/// 1px accent border (waybar `#cava` `border: 1px solid rgb(191,255,217)`).
const BORDER: u32 = 1;
/// Font scale for the block glyphs — matches the bar's text scale (widgets /
/// workspaces use 0.5); the 8 block levels stay distinguishable at this size.
const FONT_SCALE: f32 = 0.5;

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
ascii_max_range = 7\n\
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
}

impl Cava {
    pub fn new() -> Self {
        Self {
            bars: Arc::new(Mutex::new(vec![0u8; BARS])),
            child: None,
            config_path: PathBuf::new(),
            layout: (0, 0),
            paused: false,
        }
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

    /// Render the `#cava` box (background fill + 1px accent border, 8px
    /// h-padding) containing the latest frame's block glyphs, accent-colored.
    pub fn render(&mut self, canvas: &mut [u8], x_offset: u32, height: u32, stride: u32) {
        let Some(font) = theme::font() else {
            return;
        };
        let scale = Scale::uniform(height as f32 * FONT_SCALE);
        let v_metrics = font.v_metrics(scale);
        let glyph_height = v_metrics.ascent - v_metrics.descent;

        // Map each bar value to its format-icons glyph (waybar does exactly
        // this: `getIcon(value, "", ascii_range + 1)`).
        let frame = self.bars.lock().unwrap().clone();
        let glyphs: String = frame
            .iter()
            .map(|&v| FORMAT_ICONS[(v as usize).min(FORMAT_ICONS.len() - 1)])
            .collect();

        let bar_width = stride / 4;
        let box_w = 2 * BORDER + 2 * H_PAD + text_width(font, scale, &glyphs).ceil() as u32;
        self.layout = (x_offset, x_offset + box_w);

        // Same filled-box primitive as the clock/workspaces/tray/widgets modules,
        // so cava matches the rest of the bar instead of being the one hole.
        draw_bordered_box(canvas, stride, x_offset, 0, box_w, height, bg(), accent());

        // Pen: 1px in from the border, 8px h-padding, vertically centered. The
        // block glyphs sit at the bottom of their cell, so bars grow upward from
        // the lower part of the centered cell — the correct visualizer look.
        draw_text(
            font,
            scale,
            canvas,
            stride,
            (bar_width, height),
            &glyphs,
            x_offset as f32 + BORDER as f32 + H_PAD as f32,
            (height as f32 - glyph_height) / 2.0 + v_metrics.ascent,
            accent(),
        );
    }

    /// Total pixel width this module occupies on the left of the bar, as laid out
    /// by the last render.
    pub fn width(&self) -> u32 {
        self.layout.1.saturating_sub(self.layout.0)
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

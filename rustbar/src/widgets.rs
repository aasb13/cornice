//! Right-side widget strip — replicates the waybar `modules-right` seamless
//! strip, using Nerd Font icons (JetBrainsMono Nerd Font Mono) so it matches
//! waybar visually instead of text tags. Read-only for now (no events).
//!
//! Layout: one seamless bordered box (black bg, 1px accent border, 14px
//! h-padding per segment) right-aligned ending at `right_edge`. The tray is
//! rendered separately, to the LEFT of this strip, by `tray_render`.

use std::collections::HashMap;
use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

use rusttype::{Font, Scale, point};

use crate::theme::{draw_bordered_box, ACCENT, BG};

const REFRESH_INTERVAL: Duration = Duration::from_millis(1000);
const BORDER: u32 = 1;
/// Horizontal padding inside each segment (waybar stats `padding: 0 14px`).
const H_PAD: u32 = 14;
/// Font scale for the widget strip — a middle ground: 0.6 (clock's size) was
/// wider than waybar, 0.43 (waybar's 13px-in-30px) was absurdly small. 0.5
/// (~16px at the default 32px bar) stays readable while keeping the strip
/// narrower than the clock.
const FONT_SCALE: f32 = 0.5;

// ── Nerd Font (Font Awesome / Material) icon codepoints, lifted from the
//    waybar config.jsonc + bandwidth scripts. Every codepoint was verified
//    against the glyph *name* in JetBrainsMonoNerdFontMono-Regular.ttf (an
//    earlier version had transposed digits → wrong icons like md-matrix).
const ICON_VOL_LOW: char = '\u{f026}';
const ICON_VOL_MID: char = '\u{f027}';
const ICON_VOL_HIGH: char = '\u{f028}';
const ICON_VOL_MUTE: char = '\u{f466}';
const ICON_CPU: char = '\u{f2db}';
const ICON_TEMP: char = '\u{f2c9}';
const ICON_MEM: char = '\u{f0c9}';
const ICON_BAT_CHARGE: char = '\u{f0084}';  // waybar format-charging
const ICON_BAT_PLUG: char = '\u{f17e2}';    // waybar format-plugged
// Notification — waybar custom/notification format-icons. `none`/`notification`
// use the bell (0xf0a2); dnd states use bell-off (0xf1f7); a notification present
// adds the red dot (0xf444). (Pango <span> markup can't be rendered here, so the
// dot is drawn inline rather than as a red superscript.)
const ICON_BELL: char = '\u{f0a2}';
const ICON_BELL_OFF: char = '\u{f1f7}';
const ICON_BELL_DOT: char = '\u{f444}';
// WiFi signal icons — Material `md-wifi_strength_*` (from bandwidth_wlan0.sh).
const ICON_WIFI_FULL: char = '\u{f0928}';
const ICON_WIFI_GOOD: char = '\u{f0925}';
const ICON_WIFI_MED: char = '\u{f0922}';
const ICON_WIFI_LOW: char = '\u{f091f}';
const ICON_WIFI_VWEAK: char = '\u{f092f}';
const ICON_WIFI_OFF: char = '\u{f092e}';
// Ethernet — Material `md-network` / `md-close_network` (bandwidth_enp3s0.sh).
const ICON_ETH_ON: char = '\u{f06f3}';
const ICON_ETH_OFF: char = '\u{f015b}';

/// Brightness icons indexed by percent (waybar `backlight` format-icons
/// array order: moon at 0%, then md-brightness_3..7).
const BRIGHTNESS_ICONS: [char; 7] = [
    '\u{e38d}',  // 0%   — weather-moon_new
    '\u{f00dc}', // ~14% — md-brightness_3
    '\u{f00db}', // ~28% — md-brightness_2
    '\u{f00de}', // ~42% — md-brightness_5
    '\u{f00dd}', // ~57% — md-brightness_4
    '\u{f00df}', // ~71% — md-brightness_6
    '\u{f00e0}', // ~85%+— md-brightness_7
];

/// Battery icons indexed by `capacity / 10` — waybar's exact format-icons
/// array order: [0%, 10%, 20%, …, 90%, 100%].
const BATTERY_ICONS: [char; 11] = [
    '\u{f0091}', // 0%   — md-battery_unknown
    '\u{f007a}', // 10%  — md-battery_10
    '\u{f007b}', // 20%  — md-battery_20
    '\u{f007c}', // 30%  — md-battery_30
    '\u{f007d}', // 40%  — md-battery_40
    '\u{f007e}', // 50%  — md-battery_50
    '\u{f007f}', // 60%  — md-battery_60
    '\u{f0080}', // 70%  — md-battery_70
    '\u{f0081}', // 80%  — md-battery_80
    '\u{f0082}', // 90%  — md-battery_90
    '\u{f0079}', // 100% — md-battery (full cap)
];

/// Identifies a widget-strip segment so the bar can route clicks/scrolls to
/// the right module. Only segments that mirror an interactive waybar module
/// get a dedicated kind; the rest collapse to `Other` (no action).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SegKind {
    Other,
    /// waybar `custom/notification` — click: `swaync-client -t`,
    /// right-click: `swaync-client -d`.
    Notification,
    /// waybar `pulseaudio` — click: `pavucontrol`; scroll: volume ±.
    Volume,
    /// waybar `backlight` — scroll: brightness ±.
    Brightness,
    /// The keyboard-layout indicator (waybar `hyprland/language`). Read-only,
    /// but it needs its own kind so a layout change can patch just this
    /// segment instead of forcing a full strip rebuild.
    Language,
}

pub struct WidgetStrip {
    font: Option<Font<'static>>,
    /// Cached (kind, label) per segment, rebuilt at most once per
    /// `REFRESH_INTERVAL`. The kind drives click/scroll dispatch.
    cache: Vec<(SegKind, String)>,
    /// (kind, x_start, x_end) per segment as drawn by the last `render` —
    /// used to hit-test pointer clicks/scrolls against the right module.
    layout: Vec<(SegKind, u32, u32)>,
    last_refresh: Option<Instant>,
    /// (idle, total) from the previous `/proc/stat` sample, for CPU delta.
    prev_cpu: Option<(u64, u64)>,
    /// iface -> (rx_bytes, tx_bytes) from the previous `/proc/net/dev` sample.
    prev_net: HashMap<String, (u64, u64)>,
    prev_time: Option<Instant>,
}

impl WidgetStrip {
    pub fn new() -> Self {
        Self {
            font: None,
            cache: Vec::new(),
            layout: Vec::new(),
            last_refresh: None,
            prev_cpu: None,
            prev_net: HashMap::new(),
            prev_time: None,
        }
    }

    pub fn init(&mut self) {
        let paths = [
            // Nerd Font — the *non-Mono* variant that waybar's CSS calls
            // "JetBrainsMono Nerd Font". The Mono variant shrinks every icon
            // to fit a single monospace cell, so icons render smaller than the
            // digits next to them; the regular variant draws icons at full
            // size matching the text (verified: same advance width, bigger
            // glyph ink), so icons match the font size without widening the bar.
            "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
            "/usr/share/fonts/TTF/JetBrainsMonoNerdFontMono-Regular.ttf",
            "/usr/share/fonts/TTF/DejaVuSansMNerdFontMono-Regular.ttf",
            "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
            "/usr/share/fonts/TTF/TerminusTTF.ttf",
        ];
        for path in &paths {
            if let Ok(data) = std::fs::read(path) {
                if let Some(font) = Font::try_from_vec(data) {
                    self.font = Some(font);
                    return;
                }
            }
        }
        log::error!("no font found for widgets");
    }

    /// Renders the seamless strip right-aligned to `right_edge`. Returns the
    /// total width consumed (0 if there's nothing to show).
    pub fn render(&mut self, canvas: &mut [u8], right_edge: u32, height: u32, stride: u32) -> u32 {
        self.maybe_refresh();
        let font = match &self.font {
            Some(f) => f,
            None => return 0,
        };
        if self.cache.is_empty() {
            return 0;
        }

        let scale = Scale::uniform(height as f32 * FONT_SCALE);
        let v_metrics = font.v_metrics(scale);
        let glyph_height = v_metrics.ascent - v_metrics.descent;
        let bar_width = stride / 4;

        // Measure each segment; seg width = 2*H_PAD + text advance.
        let segs: Vec<(SegKind, String, u32)> = self
            .cache
            .iter()
            .map(|(kind, label)| {
                let w: f32 = label
                    .chars()
                    .map(|c| font.glyph(c).scaled(scale).h_metrics().advance_width)
                    .sum();
                (*kind, label.clone(), 2 * H_PAD + w.ceil() as u32)
            })
            .collect();

        let content_w: u32 = segs.iter().map(|(_, _, w)| *w).sum();
        let box_w = 2 * BORDER + content_w;
        let box_x = right_edge.saturating_sub(box_w);

        // One seamless box for the whole strip (waybar stats: black bg, 1px
        // accent border; left cap on the first segment, right cap on the last).
        draw_bordered_box(canvas, stride, box_x, 0, box_w, height, BG, ACCENT);

        let mut x = box_x + BORDER;
        let y = (height as f32 - glyph_height) / 2.0 + v_metrics.ascent;
        // Record each segment's [x_start, x_end) so the bar can hit-test
        // pointer clicks/scrolls and dispatch them to the matching module.
        self.layout.clear();
        for (kind, label, w) in &segs {
            // Text centered within the segment by construction: seg_w =
            // text_w + 2*H_PAD, so pen_x = seg_x + H_PAD leaves equal padding.
            let pen_x = x as f32 + H_PAD as f32;
            draw_text(font, scale, canvas, stride, bar_width, height, label, pen_x, y);
            self.layout.push((*kind, x, x + w));
            x += w;
        }

        box_w
    }

    /// Which segment (if any) is under pointer x, in surface (bar) coordinates.
    /// Returns the segment kind so the bar can route clicks/scrolls; `None`
    /// when x is between segments or no render has happened yet.
    pub fn hit_test(&self, x: f64) -> Option<SegKind> {
        let x = x as i64;
        self.layout
            .iter()
            .find(|(_, start, end)| x >= *start as i64 && x < *end as i64)
            .map(|(kind, _, _)| *kind)
    }

    /// Re-read ONLY the volume segment and patch its cached label in place,
    /// leaving every other segment's value untouched. Used after a volume
    /// scroll so the new level shows immediately — without re-sampling
    /// CPU/temp/mem/… (which `force_refresh` would do, making the whole strip
    /// visibly jump on every scroll tick). The periodic 1s refresh keeps
    /// running on its own schedule, so the other segments still update normally.
    pub fn refresh_volume(&mut self) {
        if let Some(label) = volume_label() {
            for (kind, s) in &mut self.cache {
                if *kind == SegKind::Volume {
                    *s = label;
                    break;
                }
            }
        }
    }

    /// Re-read ONLY the brightness segment and patch its cached label in place
    /// — see `refresh_volume` for why this is targeted rather than a full
    /// strip refresh.
    pub fn refresh_brightness(&mut self) {
        if let Some(label) = brightness_label() {
            for (kind, s) in &mut self.cache {
                if *kind == SegKind::Brightness {
                    *s = label;
                    break;
                }
            }
        }
    }

    /// Re-read ONLY the notification segment and patch its cached label in
    /// place — see `refresh_volume` for why this is targeted rather than a
    /// full strip refresh. Used after a notification right-click (DND toggle)
    /// so the bell/bell-off icon flips immediately instead of lagging behind
    /// the periodic 1s refresh.
    pub fn refresh_notification(&mut self) {
        if let Some(label) = notification_label() {
            for (kind, s) in &mut self.cache {
                if *kind == SegKind::Notification {
                    *s = label;
                    break;
                }
            }
        }
    }

    /// Re-read ONLY the language segment and patch its cached label in place —
    /// see `refresh_volume` for why this is targeted rather than a full strip
    /// refresh. Called straight off Hyprland's `activelayout` event so the new
    /// layout shows on the very next frame rather than after the periodic 1s
    /// refresh (and its blocking `hyprctl` fork).
    pub fn refresh_language(&mut self) {
        let Some(label) = read_language() else { return };
        if let Some((_, s)) = self
            .cache
            .iter_mut()
            .find(|(kind, _)| *kind == SegKind::Language)
        {
            *s = label;
        } else {
            // No language segment cached yet (nothing has been rendered, or the
            // last read failed) — clear the throttle so the next render does a
            // full rebuild that includes the segment.
            self.last_refresh = None;
        }
    }

    fn maybe_refresh(&mut self) {
        let now = Instant::now();
        if let Some(t) = self.last_refresh {
            if now.duration_since(t) < REFRESH_INTERVAL {
                return;
            }
        }
        self.refresh(now);
        self.last_refresh = Some(now);
    }

    fn refresh(&mut self, now: Instant) {
        let dt = match self.prev_time {
            Some(t) => now.duration_since(t).as_secs_f64().max(0.001),
            None => 0.0,
        };

        let mut segs: Vec<(SegKind, String)> = Vec::new();

        // notification — mirrors waybar custom/notification: DND on → bell-off
        // (0xf1f7), else bell (0xf0a2); notifications present (count>0) add the
        // dot (0xf444). (Pango red-superscript markup can't be drawn here.)
        // Click → `swaync-client -t` (toggle panel), right-click → `-d` (DND).
        if let Some(label) = notification_label() {
            segs.push((SegKind::Notification, label));
        }
        // volume (waybar pulseaudio: `{volume}% {icon}`, muted icon).
        // Click → pavucontrol; scroll → wpctl set-volume ±.
        if let Some(label) = volume_label() {
            segs.push((SegKind::Volume, label));
        }
        // cpu (waybar: `{usage}% `)
        if let Some(cpu) = read_cpu(&mut self.prev_cpu) {
            segs.push((SegKind::Other, format!("{cpu}% {ICON_CPU}")));
        }
        // temperature (waybar: `{temperatureC} {icon}`)
        if let Some(t) = read_temp() {
            segs.push((SegKind::Other, format!("{t} {ICON_TEMP}")));
        }
        // memory (waybar: `{}% `)
        if let Some(r) = read_ram() {
            segs.push((SegKind::Other, format!("{r}% {ICON_MEM}")));
        }
        // wifi + ethernet (waybar custom/bandwidth_*: icon only, + rate)
        let cur_net = read_net_dev();
        if let Some(name) = pick_iface(true) {
            if let Some(label) = wifi_label(&name, &cur_net, &self.prev_net, dt) {
                segs.push((SegKind::Other, label));
            }
        }
        if let Some(name) = pick_iface(false) {
            if let Some(label) = eth_label(&name, &cur_net, &self.prev_net, dt) {
                segs.push((SegKind::Other, label));
            }
        }
        self.prev_net = cur_net;
        // brightness (waybar: `{percent}% {icon}`, icon picked by percent).
        // Scroll → brightnessctl set ±.
        if let Some(label) = brightness_label() {
            segs.push((SegKind::Brightness, label));
        }
        // language (waybar: `{shortDescription}`)
        if let Some(lang) = read_language() {
            segs.push((SegKind::Language, lang));
        }
        // battery — mirrors waybar's format precedence:
        //   Charging   → format-charging  → "{cap}% "  (bolt)
        //   Plugged    → format-plugged   → "{cap}% "  (plug)
        //   Full       → format-full       → "{cap}% {icon}" (array)
        //   Discharging→ format            → "{cap}% {icon}" (array)
        if let Some((cap, status)) = read_battery() {
            let icon = match status.as_str() {
                "Charging" => ICON_BAT_CHARGE,
                "Plugged" => ICON_BAT_PLUG,
                _ => battery_icon(cap), // Full + Discharging use the array icon
            };
            segs.push((SegKind::Other, format!("{cap}% {icon}")));
        }

        self.cache = segs;
        self.prev_time = Some(now);
    }
}

fn battery_icon(cap: u8) -> char {
    let idx = (cap / 10).min(10) as usize;
    BATTERY_ICONS[idx]
}

fn brightness_icon(pct: u8) -> char {
    // Mirror waybar's ALabel::getIcon exactly: divisor = max(1, 100/size),
    // idx = clamp(pct / divisor, 0, size-1). (For 7 icons: divisor=14.)
    let size = BRIGHTNESS_ICONS.len();
    let divisor = (100 / size).max(1);
    let idx = ((pct as usize) / divisor).min(size - 1);
    BRIGHTNESS_ICONS[idx]
}

// ── text draw: gamma-corrected, premultiplied src-over accent over the box ─
#[allow(clippy::too_many_arguments)]
fn draw_text(
    font: &Font<'static>,
    scale: Scale,
    canvas: &mut [u8],
    stride: u32,
    bar_width: u32,
    height: u32,
    text: &str,
    pen_x: f32,
    pen_y: f32,
) {
    let mut pen = point(pen_x, pen_y);
    for c in text.chars() {
        let glyph = font.glyph(c);
        let sg = glyph.scaled(scale);
        let adv = sg.h_metrics().advance_width;
        let pg = sg.positioned(pen);
        if let Some(bb) = pg.pixel_bounding_box() {
            pg.draw(|xr, yr, v| {
                let xi = bb.min.x + xr as i32;
                let yi = bb.min.y + yr as i32;
                if xi >= 0 && (xi as u32) < bar_width && yi >= 0 && (yi as u32) < height {
                    let off = yi as usize * stride as usize + xi as usize * 4;
                    if off + 3 < canvas.len() {
                        let gv = v.powf(1.0 / 2.2);
                        let src = [ACCENT.2 as f32, ACCENT.1 as f32, ACCENT.0 as f32]; // b, g, r
                        for c in 0..3 {
                            let s = src[c] * gv;
                            let d = canvas[off + c] as f32;
                            canvas[off + c] = (s + d * (1.0 - gv)).min(255.0) as u8;
                        }
                        let da = canvas[off + 3] as f32;
                        canvas[off + 3] = (255.0 * gv + da * (1.0 - gv)).min(255.0) as u8;
                    }
                }
            });
        }
        pen.x += adv;
    }
}

// ── system readers — each returns None to gracefully skip its segment ──

fn read_notif() -> Option<(u32, bool)> {
    // waybar's custom/notification uses `swaync-client -swb` (subscribe-waybar)
    // which is a LONG-RUNNING subscriber — it never exits, so Command::output()
    // would block forever and freeze the bar. Instead we use the non-blocking
    // queries: `-c -sw` for the count and `-D -sw` for the DND state, both of
    // which return immediately. (The `-sw` flag = "skip wait" so it doesn't
    // hang if swaync isn't running.)
    let count = {
        let out = Command::new("swaync-client").args(["-c", "-sw"]).output().ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse::<u32>().ok()?
    };
    let dnd = {
        let out = Command::new("swaync-client").args(["-D", "-sw"]).output().ok()?;
        String::from_utf8_lossy(&out.stdout).trim().eq_ignore_ascii_case("true")
    };
    Some((count, dnd))
}

/// Build the notification segment label (bell/bell-off + optional dot) from a
/// fresh `read_notif` sample. Pure relative to the segment list, so both the
/// periodic `refresh` and the targeted `refresh_notification` (after a DND
/// right-click) share it — keeping the icon logic in one place.
fn notification_label() -> Option<String> {
    let (count, dnd) = read_notif()?;
    let base = if dnd { ICON_BELL_OFF } else { ICON_BELL };
    Some(if count > 0 {
        format!("{base}{ICON_BELL_DOT}")
    } else {
        base.to_string()
    })
}

fn read_volume() -> Option<(u8, bool)> {
    let out = Command::new("wpctl").args(["get-volume", "@DEFAULT_AUDIO_SINK@"]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let muted = s.contains("MUTED");
    let pct = s
        .split("Volume:")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse::<f32>()
        .ok()?;
    Some(((pct * 100.0).round() as u8, muted))
}

fn read_cpu(prev: &mut Option<(u64, u64)>) -> Option<u8> {
    let stat = fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.is_empty() || fields[0] != "cpu" {
        return None;
    }
    let nums: Vec<u64> = fields[1..].iter().filter_map(|s| s.parse::<u64>().ok()).collect();
    if nums.len() < 4 {
        return None;
    }
    let idle = nums[3] + nums.get(4).copied().unwrap_or(0); // idle + iowait
    let total: u64 = nums.iter().sum();
    let usage = match prev {
        Some((p_idle, p_total)) => {
            let dt = total.saturating_sub(*p_total);
            let di = idle.saturating_sub(*p_idle);
            if dt == 0 { 0 } else { ((dt - di) * 100 / dt) as u8 }
        }
        None => 0,
    };
    *prev = Some((idle, total));
    Some(usage)
}

fn read_temp() -> Option<u32> {
    // Prefer the CPU package temp (coretemp), fall back to the ACPI zone.
    if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
        for e in entries.flatten() {
            let name = fs::read_to_string(e.path().join("name")).unwrap_or_default();
            if name.trim() == "coretemp" {
                if let Ok(t) = fs::read_to_string(e.path().join("temp1_input")) {
                    if let Ok(m) = t.trim().parse::<u64>() {
                        return Some((m / 1000) as u32);
                    }
                }
            }
        }
    }
    let t = fs::read_to_string("/sys/class/thermal/thermal_zone0/temp").ok()?;
    Some((t.trim().parse::<u64>().ok()? / 1000) as u32)
}

fn read_ram() -> Option<u8> {
    let s = fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = None;
    let mut avail = None;
    for line in s.lines() {
        if line.starts_with("MemTotal:") {
            total = line.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok());
        } else if line.starts_with("MemAvailable:") {
            avail = line.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok());
        }
    }
    let total = total?;
    let avail = avail?;
    if total == 0 {
        return None;
    }
    Some(((total - avail) * 100 / total) as u8)
}

fn read_brightness() -> Option<u8> {
    let dev = fs::read_dir("/sys/class/backlight").ok()?.flatten().next()?;
    let b = fs::read_to_string(dev.path().join("brightness")).ok()?.trim().parse::<u64>().ok()?;
    let m = fs::read_to_string(dev.path().join("max_brightness")).ok()?.trim().parse::<u64>().ok()?;
    if m == 0 {
        return None;
    }
    Some((b * 100 / m) as u8)
}

/// Format the volume segment label for a given (percent, muted) — pure so it
/// can be unit-tested independently of `wpctl`. Mirrors waybar pulseaudio
/// `{volume}% {icon}` with the level-thresholded icon (muted → mute icon).
fn fmt_volume(pct: u8, muted: bool) -> String {
    let icon = if muted {
        ICON_VOL_MUTE
    } else if pct < 34 {
        ICON_VOL_LOW
    } else if pct < 67 {
        ICON_VOL_MID
    } else {
        ICON_VOL_HIGH
    };
    format!("{pct}% {icon}")
}

/// Format the brightness segment label for a given percent — pure so it can
/// be unit-tested. Mirrors waybar backlight `{percent}% {icon}`.
fn fmt_brightness(pct: u8) -> String {
    format!("{pct}% {}", brightness_icon(pct))
}

/// Read the default sink and return its waybar-style label, or `None` if
/// `wpctl` isn't available / fails. Shared by the periodic `refresh` and the
/// targeted `refresh_volume` so the formatting never drifts between them.
fn volume_label() -> Option<String> {
    let (pct, muted) = read_volume()?;
    Some(fmt_volume(pct, muted))
}

/// Read the backlight and return its waybar-style label, or `None`. Shared by
/// the periodic `refresh` and the targeted `refresh_brightness`.
fn brightness_label() -> Option<String> {
    Some(fmt_brightness(read_brightness()?))
}

fn read_language() -> Option<String> {
    let out = Command::new("hyprctl").args(["devices", "-j"]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    Some(keymap_to_code(&parse_main_keymap(&s)?))
}

/// Select the `active_keymap` of the keyboard the user actually types on.
///
/// `hyprctl devices -j` lists *every* input device — including virtual ones
/// (`video-bus`, `power-button`, `sleep-button`, …) that are listed first and
/// never receive layout-toggle keypresses, so their `active_keymap` is frozen
/// on the first layout. A forward search for `"active_keymap"` therefore reads
/// the virtual device's layout and pins the widget to "us" regardless of the
/// real keyboard's current layout — the bug this fixes.
///
/// We instead pick the keyboard flagged `"main": true` (the real keyboard). In
/// the stable `hyprctl devices -j` field order each keyboard object emits
/// `active_keymap` immediately before `main`, so the main keyboard's keymap is
/// the last `"active_keymap"` occurrence preceding its `"main": true` marker.
/// If no device is flagged main we fall back to the last keyboard in the list.
fn parse_main_keymap(json: &str) -> Option<String> {
    const KEYMAP: &str = "\"active_keymap\": \"";
    const MAIN_TRUE: &str = "\"main\": true";
    // Prefer the keyboard flagged `"main": true`: its `active_keymap` is the
    // nearest `"active_keymap"` preceding the `"main": true` marker (Hyprland
    // emits `active_keymap` right before `main` in each keyboard object).
    if let Some(main_pos) = json.find(MAIN_TRUE)
        && let Some(kp) = json[..main_pos].rfind(KEYMAP)
    {
        let rest = &json[kp + KEYMAP.len()..];
        if let Some(end) = rest.find('"') {
            return Some(rest[..end].to_string());
        }
    }
    // Fallback: last keyboard listed.
    let kp = json.rfind(KEYMAP)?;
    let rest = &json[kp + KEYMAP.len()..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// XKB layout names whose code differs from their base language, keyed by the
/// *full* keymap name. These are the variants users actually see in Hyprland's
/// layout list, where the country/variant is the meaningful part
/// ("English (US)" is `us`, not `en`). Anything not listed falls through to
/// `LANGUAGE_CODES` on the base name.
const LAYOUT_VARIANT_CODES: &[(&str, &str)] = &[
    ("English (US)", "us"),
    ("English (UK)", "gb"),
    ("English (US, intl., with dead keys)", "us"),
    ("English (US, intl., with dead keys on 5-level compose)", "us"),
    ("English (intl., with dead keys)", "gb"),
    ("English (Russian)", "ru"),
    ("Portuguese (Brazil)", "br"),
    ("Spanish (Latin American)", "es"),
    ("French (AZERTY)", "fr"),
];

/// XKB layout long name → ISO 639-1 code. This is the table
/// `xkbcommon`'s `XkbLayoutInfo::short_description` is generated from, i.e.
/// exactly what waybar's `{shortDescription}` renders — "Russian" → `ru`,
/// not the full "russian" word.
const LANGUAGE_CODES: &[(&str, &str)] = &[
    ("Afrikaans", "af"),
    ("Albanian", "sq"),
    ("Amharic", "am"),
    ("Arabic", "ar"),
    ("Armenian", "hy"),
    ("Azerbaijani", "az"),
    ("Basque", "eu"),
    ("Belarusian", "be"),
    ("Bengali", "bn"),
    ("Bosnian", "bs"),
    ("Brazilian", "pt"),
    ("Bulgarian", "bg"),
    ("Burmese", "my"),
    ("Catalan", "ca"),
    ("Chinese", "zh"),
    ("Croatian", "hr"),
    ("Czech", "cs"),
    ("Danish", "da"),
    ("Dutch", "nl"),
    ("English", "en"),
    ("Esperanto", "eo"),
    ("Estonian", "et"),
    ("Faroese", "fo"),
    ("Finnish", "fi"),
    ("French", "fr"),
    ("Galician", "gl"),
    ("Georgian", "ka"),
    ("German", "de"),
    ("Greek", "el"),
    ("Hebrew", "he"),
    ("Hindi", "hi"),
    ("Hungarian", "hu"),
    ("Icelandic", "is"),
    ("Indonesian", "id"),
    ("Irish", "ga"),
    ("Italian", "it"),
    ("Japanese", "ja"),
    ("Kannada", "kn"),
    ("Kazakh", "kk"),
    ("Khmer", "km"),
    ("Korean", "ko"),
    ("Kurdish", "ku"),
    ("Lao", "lo"),
    ("Latin American", "es"),
    ("Latvian", "lv"),
    ("Lithuanian", "lt"),
    ("Macedonian", "mk"),
    ("Malay", "ms"),
    ("Malayalam", "ml"),
    ("Maltese", "mt"),
    ("Marathi", "mr"),
    ("Mongolian", "mn"),
    ("Nepali", "ne"),
    ("Norwegian", "no"),
    ("Persian", "fa"),
    ("Polish", "pl"),
    ("Portuguese", "pt"),
    ("Punjabi", "pa"),
    ("Romanian", "ro"),
    ("Russian", "ru"),
    ("Serbian", "sr"),
    ("Sinhala", "si"),
    ("Slovak", "sk"),
    ("Slovenian", "sl"),
    ("Spanish", "es"),
    ("Swahili", "sw"),
    ("Swedish", "sv"),
    ("Tamil", "ta"),
    ("Tatar", "tt"),
    ("Telugu", "te"),
    ("Thai", "th"),
    ("Turkish", "tr"),
    ("Ukrainian", "uk"),
    ("Urdu", "ur"),
    ("Uzbek", "uz"),
    ("Vietnamese", "vi"),
];

/// Shorten a full keymap name to a layout code, mirroring waybar's
/// `{shortDescription}` without linking xkbcommon: look the name up in
/// `LAYOUT_VARIANT_CODES` / `LANGUAGE_CODES` so it yields a real language code
/// ("Russian" → "ru", "German (Neo)" → "de"), falling back to the old
/// heuristic (parenthetical if present, else the first word) for names neither
/// table knows.
fn keymap_to_code(raw: &str) -> String {
    let raw = raw.trim();
    if let Some((_, code)) = LAYOUT_VARIANT_CODES.iter().find(|(name, _)| *name == raw) {
        return (*code).to_string();
    }
    let base = raw.split('(').next().unwrap_or(raw).trim();
    if let Some((_, code)) = LANGUAGE_CODES.iter().find(|(name, _)| *name == base) {
        return (*code).to_string();
    }
    if let Some(l) = raw.rfind('(') {
        let after = &raw[l + 1..];
        let close = after.find(')').unwrap_or(after.len());
        after[..close].to_lowercase()
    } else {
        raw.split_whitespace().next().unwrap_or(raw).to_lowercase()
    }
}

fn read_battery() -> Option<(u8, String)> {
    let bat = fs::read_dir("/sys/class/power_supply")
        .ok()?
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("BAT"))?;
    let base = bat.path();
    let cap = fs::read_to_string(base.join("capacity")).ok()?.trim().parse::<u8>().ok()?;
    let mut status = fs::read_to_string(base.join("status"))
        .ok()?
        .trim()
        .to_string();

    // Mirror waybar's battery.cpp preprocessing exactly:
    //   "Give `Plugged` higher priority over `Not charging`."
    //   if adapter online && status in {Discharging, Not charging}
    //   && adapter status != "Discharging"  →  status = "Plugged"
    if status == "Discharging" || status == "Not charging" {
        let adapter_online = fs::read_dir("/sys/class/power_supply")
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                // Mains adapter: AC*, ADP*, or anything with type "Mains"
                n.starts_with("AC") || n.starts_with("ADP")
            })
            .any(|e| {
                fs::read_to_string(e.path().join("online"))
                    .ok()
                    .and_then(|s| s.trim().parse::<u8>().ok())
                    == Some(1)
            });
        if adapter_online {
            status = "Plugged".to_string();
        }
    }
    // "If we've reached 100% just mark as full"
    if cap == 100 && status == "Charging" {
        status = "Full".to_string();
    }
    Some((cap, status))
}

fn read_net_dev() -> HashMap<String, (u64, u64)> {
    let mut map = HashMap::new();
    if let Ok(s) = fs::read_to_string("/proc/net/dev") {
        for line in s.lines().skip(2) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 {
                continue;
            }
            let name = fields[0].trim_end_matches(':');
            let rx = fields[1].parse::<u64>().unwrap_or(0);
            let tx = fields[9].parse::<u64>().unwrap_or(0);
            map.insert(name.to_string(), (rx, tx));
        }
    }
    map
}

fn operstate_up(iface: &str) -> bool {
    fs::read_to_string(format!("/sys/class/net/{iface}/operstate"))
        .map(|s| s.trim() == "up")
        .unwrap_or(false)
}

/// Pick a network interface: `wireless=true` -> wifi (prefer `wlan0`), else the
/// first ethernet (`en*`, not wireless).
fn pick_iface(wireless: bool) -> Option<String> {
    let entries: Vec<_> = fs::read_dir("/sys/class/net").ok()?.flatten().collect();
    let names: Vec<String> = entries
        .iter()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let preferred = if wireless { "wlan0" } else { "enp3s0" };
    if names.iter().any(|n| n == preferred) {
        return Some(preferred.to_string());
    }
    for e in &entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let is_wireless = e.path().join("wireless").is_dir();
        if wireless && is_wireless {
            return Some(name);
        }
        if !wireless && name.starts_with("en") && !is_wireless {
            return Some(name);
        }
    }
    None
}

/// Wifi label: just the signal-strength icon (waybar `custom/bandwidth_wlan0`
/// `text` is the icon only — the rate goes in the hover tooltip, not the bar).
fn wifi_label(iface: &str, _cur: &HashMap<String, (u64, u64)>, _prev: &HashMap<String, (u64, u64)>, _dt: f64) -> Option<String> {
    if !operstate_up(iface) {
        return Some(ICON_WIFI_OFF.to_string());
    }
    let signal = read_wifi_signal(iface).unwrap_or(0);
    let icon = if signal >= 80 {
        ICON_WIFI_FULL
    } else if signal >= 60 {
        ICON_WIFI_GOOD
    } else if signal >= 40 {
        ICON_WIFI_MED
    } else if signal >= 20 {
        ICON_WIFI_LOW
    } else {
        ICON_WIFI_VWEAK
    };
    Some(icon.to_string())
}

/// Ethernet label: just the connected/disconnected icon (waybar
/// `custom/bandwidth_enp3s0` `text` is the icon only — rate is in tooltip).
fn eth_label(iface: &str, _cur: &HashMap<String, (u64, u64)>, _prev: &HashMap<String, (u64, u64)>, _dt: f64) -> Option<String> {
    if !operstate_up(iface) {
        return Some(ICON_ETH_OFF.to_string());
    }
    Some(ICON_ETH_ON.to_string())
}

/// Wi-Fi signal strength 0..=100 via `/proc/net/wireless` (link quality).
fn read_wifi_signal(iface: &str) -> Option<u8> {
    let s = fs::read_to_string("/proc/net/wireless").ok()?;
    for line in s.lines().skip(2) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.is_empty() {
            continue;
        }
        let name = fields[0].trim_end_matches(':');
        if name == iface && fields.len() >= 3 {
            // link quality is "a/b" → percentage
            let lq = fields[2];
            if let Some((a, b)) = lq.split_once('/') {
                let a: u64 = a.parse().unwrap_or(0);
                let b: u64 = b.parse().unwrap_or(1);
                if b == 0 {
                    return Some(0);
                }
                return Some(((a * 100) / b).min(100) as u8);
            }
            // some kernels report a single float level; treat 70+ as full
            return Some(80);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The actual bug: `hyprctl devices -j` lists a virtual `video-bus` keyboard
    /// first (frozen on "English (US)", NOT main) while the real keyboard is
    /// flagged `main: true` and currently on Russian. A naive forward search
    /// reads `video-bus` and returns "us"; we must read the main keyboard.
    #[test]
    fn picks_main_keyboard_not_first() {
        let json = r#"{
  "mice": [],
  "keyboards": [
    {
      "address": "0x1",
      "name": "video-bus",
      "layout": "us,ru",
      "active_layout_index": 0,
      "active_keymap": "English (US)",
      "main": false
    },
    {
      "address": "0x2",
      "name": "compx-mchose-2.4g-wireless",
      "layout": "us,ru",
      "active_layout_index": 1,
      "active_keymap": "Russian",
      "main": true
    }
  ]
}"#;
        assert_eq!(parse_main_keymap(json).as_deref(), Some("Russian"));
        assert_eq!(keymap_to_code(&parse_main_keymap(json).unwrap()), "ru");
    }

    #[test]
    fn english_us_shortens_to_us() {
        let json = r#"{
  "keyboards": [
    { "name": "video-bus", "active_keymap": "English (US)", "main": false },
    { "name": "real-kb", "active_keymap": "English (US)", "main": true }
  ]
}"#;
        assert_eq!(parse_main_keymap(json).as_deref(), Some("English (US)"));
        assert_eq!(keymap_to_code(&parse_main_keymap(json).unwrap()), "us");
    }

    /// `"main": false` must never be mistaken for `"main": true`.
    #[test]
    fn ignores_main_false_before_main_true() {
        let json = r#"{
  "keyboards": [
    { "name": "a", "active_keymap": "English (US)", "main": false },
    { "name": "b", "active_keymap": "English (US)", "main": false },
    { "name": "c", "active_keymap": "Russian", "main": true }
  ]
}"#;
        assert_eq!(parse_main_keymap(json).as_deref(), Some("Russian"));
    }

    /// No `main: true` present — fall back to the last keyboard listed.
    #[test]
    fn falls_back_to_last_when_no_main() {
        let json = r#"{
  "keyboards": [
    { "name": "video-bus", "active_keymap": "English (US)", "main": false },
    { "name": "real-kb", "active_keymap": "Russian", "main": false }
  ]
}"#;
        assert_eq!(parse_main_keymap(json).as_deref(), Some("Russian"));
    }

    #[test]
    fn keymap_to_code_uses_language_tables_then_falls_back() {
        // Known language names collapse to their ISO 639-1 code.
        assert_eq!(keymap_to_code("Russian"), "ru");
        assert_eq!(keymap_to_code("Ukrainian"), "uk");
        assert_eq!(keymap_to_code("French"), "fr");
        // Variants prefer the variant table, then the base language.
        assert_eq!(keymap_to_code("English (US)"), "us");
        assert_eq!(keymap_to_code("English (US, intl., with dead keys)"), "us");
        assert_eq!(keymap_to_code("German (Neo)"), "de");
        assert_eq!(keymap_to_code("French (AZERTY)"), "fr");
        // Unknown names keep the old heuristic: parenthetical, else first word.
        assert_eq!(keymap_to_code("Colemak"), "colemak");
        assert_eq!(keymap_to_code("Dvorak (US)"), "us");
    }

    #[test]
    fn empty_json_returns_none() {
        assert_eq!(parse_main_keymap(""), None);
        assert_eq!(parse_main_keymap(r#"{"keyboards": []}"#), None);
    }

    /// `fmt_volume` mirrors waybar pulseaudio `{volume}% {icon}`: muted → mute
    /// icon regardless of level; otherwise level-thresholded low/mid/high.
    #[test]
    fn fmt_volume_picks_icon_by_level_and_mute() {
        // muted always uses the mute icon, even at high levels
        assert_eq!(fmt_volume(0, true), format!("0% {ICON_VOL_MUTE}"));
        assert_eq!(fmt_volume(80, true), format!("80% {ICON_VOL_MUTE}"));
        // unmuted: thresholds are 34 / 67 (waybar's default <34/<67/else)
        assert_eq!(fmt_volume(0, false), format!("0% {ICON_VOL_LOW}"));
        assert_eq!(fmt_volume(33, false), format!("33% {ICON_VOL_LOW}"));
        assert_eq!(fmt_volume(34, false), format!("34% {ICON_VOL_MID}"));
        assert_eq!(fmt_volume(66, false), format!("66% {ICON_VOL_MID}"));
        assert_eq!(fmt_volume(67, false), format!("67% {ICON_VOL_HIGH}"));
        assert_eq!(fmt_volume(100, false), format!("100% {ICON_VOL_HIGH}"));
    }

    /// `fmt_brightness` mirrors waybar backlight `{percent}% {icon}`, delegating
    /// the icon to `brightness_icon` (which has its own divisor logic).
    #[test]
    fn fmt_brightness_delegates_to_brightness_icon() {
        assert_eq!(fmt_brightness(0), format!("0% {}", brightness_icon(0)));
        assert_eq!(fmt_brightness(50), format!("50% {}", brightness_icon(50)));
        assert_eq!(fmt_brightness(100), format!("100% {}", brightness_icon(100)));
    }
}

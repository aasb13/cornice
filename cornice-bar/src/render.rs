//! The backend-independent bar: painting and input decisions.
//!
//! Both the Wayland and the X11 backend own a surface, a pixel buffer and a
//! native event stream. What gets painted into the buffer, and what a click or
//! scroll at a given x means, is identical — so it lives here. A backend turns
//! its native events into a button/x/step and gets back a redraw flag and, for
//! a right-click on a tray icon, a request to open a menu using whatever popup
//! mechanism that display server provides.

use cornice_config::Reload;

use crate::cava::Cava;
use crate::clock::Clock;
use crate::config::Config;
use crate::theme::SIDE_MARGIN;
use crate::tray::MenuItem;
use crate::tray_render::TrayRenderer;
use crate::widgets::{SegKind, WidgetStrip};
use crate::workspaces::WorkspaceList;

/// Volume scroll step in percent. Matches waybar `pulseaudio`'s default
/// `scroll-step` (the user's config has no override); volume is capped at 100%
/// via `wpctl set-volume -l 1.0 …` (waybar `max-volume: 100`).
const VOL_SCROLL_STEP: u8 = 1;
/// Brightness scroll step in percent. Matches waybar `backlight`'s default.
const BRIGHT_SCROLL_STEP: u8 = 1;

/// Launch a command detached so it never blocks the bar — mirrors waybar's
/// `on-click`/`on-scroll` execution. A short-lived reaper thread waits on the
/// child so we don't accumulate zombies.
pub fn spawn(cmd: &str, args: &[&str]) {
    use std::process::Stdio;
    match std::process::Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(err) => log::error!("failed to spawn {cmd}: {err}"),
    }
}

/// Run a command to completion so a follow-up re-read sees the applied change.
/// These are the bar's own `set`-style actions; `.status()` reaps the child, so
/// rapid scroll can't accumulate zombies.
pub fn run_sync(cmd: &str, args: &[&str]) {
    use std::process::Stdio;
    if let Err(err) = std::process::Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        log::error!("failed to run {cmd}: {err}");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    Default,
    Pointer,
}

pub struct Hover {
    pub redraw: bool,
    pub cursor: Cursor,
}

/// A right-click on a tray icon. Wayland creates an `xdg_popup` grabbed on the
/// seat's serial; X11 creates an override-redirect menu window. The data is the
/// same either way.
pub struct MenuRequest {
    pub owner_id: String,
    pub menu_path: String,
    pub items: Vec<MenuItem>,
    pub x: f64,
    pub serial: u32,
}

pub struct Bar {
    pub clock: Box<Clock>,
    pub workspaces: Box<WorkspaceList>,
    pub widgets: Box<WidgetStrip>,
    pub cava: Box<Cava>,
    pub tray: Option<TrayRenderer>,
    pub reload: Reload<Config>,
}

impl Bar {
    pub fn new(
        clock: Box<Clock>,
        workspaces: Box<WorkspaceList>,
        reload: Reload<Config>,
        tray: Option<TrayRenderer>,
    ) -> Self {
        Self {
            clock,
            workspaces,
            widgets: Box::new(WidgetStrip::new()),
            cava: Box::new(Cava::new()),
            tray,
            reload,
        }
    }

    /// Re-read the config if the file changed on disk. A broken file is logged
    /// inside `Reload` and the last good config stays in force.
    pub fn check_reload(&mut self) {
        if self.reload.poll().is_some() {
            log::info!("config reloaded from disk");
        }
    }

    /// Re-read unconditionally, used by `SIGHUP`.
    pub fn force_reload(&mut self) {
        if self.reload.force().is_some() {
            log::info!("config reloaded from SIGHUP");
        }
    }

    /// Paint one frame into a `B, G, R, A` canvas.
    pub fn render(&mut self, canvas: &mut [u8], width: u32, height: u32, stride: u32) {
        for pixel in canvas.as_chunks_mut::<4>().0 {
            pixel.fill(0);
        }

        // One palette and one visualiser mode for the whole frame, read once so
        // a reload takes effect everywhere at the next redraw rather than each
        // module holding a copy that has to be refreshed separately.
        let colors = &self.reload.config().colors;
        let mode = self.reload.config().cava.mode;

        self.clock.render(
            colors,
            canvas,
            SIDE_MARGIN,
            width - SIDE_MARGIN,
            height,
            stride,
        );
        self.workspaces
            .render(colors, canvas, SIDE_MARGIN, height, stride);
        // waybar `modules-left = [hyprland/workspaces, cava]`: cava sits just
        // after workspaces with the waybar 6px gap between them.
        let cava_x = SIDE_MARGIN + self.workspaces.width() + 6;
        self.cava
            .render(colors, mode, canvas, cava_x, height, stride);
        // The widget strip is rightmost; the tray is to its LEFT with the
        // waybar 14px gap between them.
        let widgets_w = self
            .widgets
            .render(colors, canvas, width - SIDE_MARGIN, height, stride);
        let tray_right = width - SIDE_MARGIN - widgets_w - if widgets_w > 0 { 14 } else { 0 };
        if let Some(tray) = &mut self.tray {
            tray.render(colors, canvas, tray_right, height, stride);
        }
    }

    /// Pointer motion at bar-local `x`. While a menu is open the bar does not
    /// change its own hover state: the click belongs to the menu.
    pub fn motion(&mut self, x: f64, menu_open: bool) -> Hover {
        let redraw = if menu_open {
            false
        } else {
            self.workspaces.update_hover(x)
        };
        let widget_hover = matches!(
            self.widgets.hit_test(x),
            Some(SegKind::Volume) | Some(SegKind::Notification) | Some(SegKind::Brightness)
        );
        let clicking = self.workspaces.hit_test(x).is_some()
            || self.cava.hit_test(x)
            || widget_hover
            || self.tray.as_ref().is_some_and(|t| t.hit_test(x).is_some());
        Hover {
            redraw,
            cursor: if clicking {
                Cursor::Pointer
            } else {
                Cursor::Default
            },
        }
    }

    /// Pointer left the bar.
    pub fn leave(&mut self) -> bool {
        self.workspaces.clear_hover()
    }

    /// A press on the bar. Returns `(needs_redraw, menu_to_open)`; an open menu
    /// consumes the press, which only dismisses it.
    pub fn press(
        &mut self,
        button: PointerButton,
        x: f64,
        serial: u32,
        menu_open: bool,
    ) -> (bool, Option<MenuRequest>) {
        if menu_open {
            return (true, None);
        }
        let mut redraw = false;
        let mut menu = None;
        match button {
            PointerButton::Left => {
                if let Some(id) = self.workspaces.hit_test(x) {
                    self.workspaces.switch_to(id);
                    redraw = true;
                } else if let Some(item_id) = self
                    .tray
                    .as_ref()
                    .and_then(|t| t.hit_test(x).map(str::to_string))
                {
                    if let Some(tray) = &self.tray {
                        tray.activate(&item_id);
                    }
                } else if let Some(seg) = self.widgets.hit_test(x) {
                    match seg {
                        // waybar pulseaudio `on-click: pavucontrol`
                        SegKind::Volume => spawn("pavucontrol", &[]),
                        // waybar custom/notification `on-click: swaync -t`
                        SegKind::Notification => spawn("swaync-client", &["-t"]),
                        SegKind::Brightness | SegKind::Language | SegKind::Other => {}
                    }
                }
            }
            PointerButton::Right => {
                if self.cava.hit_test(x) {
                    // waybar `cava` `on-click-right: mode` → pause/resume.
                    self.cava.toggle_pause();
                    redraw = true;
                } else if self.widgets.hit_test(x) == Some(SegKind::Notification) {
                    // waybar `on-click-right: swaync -d` → toggle Do-Not-Disturb.
                    run_sync("swaync-client", &["-d"]);
                    self.widgets.refresh_notification();
                    redraw = true;
                } else if let Some(tray) = &self.tray
                    && let Some(item_id) = tray.hit_test(x).map(str::to_string)
                {
                    let (menu_path, items) = tray.menu_for(&item_id);
                    if let Some(menu_path) = menu_path
                        && !items.is_empty()
                    {
                        menu = Some(MenuRequest {
                            owner_id: item_id,
                            menu_path,
                            items,
                            x,
                            serial,
                        });
                    }
                }
            }
        }
        (redraw, menu)
    }

    /// A whole number of scroll notches at bar-local `x`. Touchpad
    /// accumulation, which is protocol-specific, belongs to the backend.
    pub fn axis(&mut self, steps: i32, x: f64) -> bool {
        if steps == 0 {
            return false;
        }
        let n = steps.unsigned_abs();
        // Wayland's vertical axis is a motion vector in surface coords: wheel-up
        // is negative. X11 button 4 is wheel-up, which backends map to -1.
        let up = steps < 0;
        match self.widgets.hit_test(x) {
            Some(SegKind::Volume) => {
                let arg = if up {
                    format!("{VOL_SCROLL_STEP}%+")
                } else {
                    format!("{VOL_SCROLL_STEP}%-")
                };
                for _ in 0..n {
                    run_sync(
                        "wpctl",
                        &[
                            "set-volume",
                            "-l",
                            "1.0",
                            "@DEFAULT_AUDIO_SINK@",
                            arg.as_str(),
                        ],
                    );
                }
                self.widgets.refresh_volume();
                true
            }
            Some(SegKind::Brightness) => {
                let arg = if up {
                    format!("+{BRIGHT_SCROLL_STEP}%")
                } else {
                    format!("{BRIGHT_SCROLL_STEP}%-")
                };
                for _ in 0..n {
                    run_sync("brightnessctl", &["set", arg.as_str()]);
                }
                self.widgets.refresh_brightness();
                true
            }
            _ => false,
        }
    }
}

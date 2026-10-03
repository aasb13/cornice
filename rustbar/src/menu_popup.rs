//! Tray context menu, rendered as an **xdg_popup** (not a layer surface).
//!
//! Using an xdg_popup with a pointer grab is the only Wayland mechanism that
//! grabs clicks across the *entire desktop*: once grabbed, the compositor sends
//! `popup_done` when the user clicks anywhere outside the popup (or presses
//! Escape), so the menu dismisses like a real popup menu. A plain layer-shell
//! surface only receives pointer events while the pointer is over it, so it
//! can't do click-outside-to-close on other windows.

use rusttype::Scale;
use smithay_client_toolkit::{
    compositor::{CompositorState, Surface, SurfaceData},
    shell::{
        wlr_layer::LayerSurface,
        xdg::{
            XdgPositioner, XdgShell,
            popup::{Popup, PopupData},
        },
    },
    shm::{Shm, slot::SlotPool},
};
use wayland_client::{
    Dispatch, QueueHandle,
    protocol::{wl_seat::WlSeat, wl_shm, wl_surface::WlSurface},
};
use wayland_protocols::xdg::shell::client::{
    xdg_popup::XdgPopup,
    xdg_positioner::{Anchor, Gravity},
    xdg_surface::XdgSurface,
};

use crate::config::Colors;
use crate::theme::{self, draw_text, text_width};
use crate::tray::MenuItem;

const ROW_HEIGHT: u32 = 26;
/// Font size for menu labels.
const FONT_SIZE: f32 = 18.0;
/// Horizontal padding on each side of the longest label.
const H_PAD: u32 = 16;
/// Used only when no font could be loaded (so text can't be measured).
const FALLBACK_MENU_WIDTH: u32 = 180;

pub struct MenuPopup {
    popup: Popup,
    pool: SlotPool,
    width: u32,
    height: u32,
    /// which tray icon this menu belongs to, so clicks route back correctly
    pub owner_id: String,
    pub menu_path: String,
    pub items: Vec<MenuItem>,
    hovered: Option<usize>,
}

impl MenuPopup {
    /// Creates an xdg_popup grabbed on `seat` with `serial` (the serial of the
    /// triggering right-click — Wayland requires the grab serial to match the
    /// input event). The popup is parented to the bar's layer surface so it
    /// can be positioned relative to it.
    ///
    /// `click_x` is the absolute x of the right-click, measured from the
    /// bar's left edge (i.e. in the parent surface's coordinate space, which
    /// is what the positioner uses). The menu is placed with its right edge at
    /// `click_x` and its top at the tray icon's bottom edge (`bar_height * 3/4`),
    /// dropping down just below the icon.
    ///
    /// `D` is whoever will receive the popup's protocol events — in practice
    /// `wayland_backend::App`, which implements `PopupHandler`. It is a parameter
    /// rather than a concrete type so this module doesn't have to name that one:
    /// `wayland_backend` imports `MenuPopup`, and naming `App` back here made the
    /// two modules import each other.
    #[allow(clippy::too_many_arguments)]
    pub fn create<D>(
        qh: &QueueHandle<D>,
        compositor_state: &CompositorState,
        xdg_shell: &XdgShell,
        shm: &Shm,
        parent_layer: &LayerSurface,
        seat: &WlSeat,
        serial: u32,
        owner_id: String,
        menu_path: String,
        items: Vec<MenuItem>,
        click_x: i32,
        bar_height: u32,
    ) -> anyhow::Result<Self>
    where
        D: Dispatch<WlSurface, SurfaceData>
            + Dispatch<XdgSurface, PopupData>
            + Dispatch<XdgPopup, PopupData>
            + 'static,
    {
        let height = (items.len() as u32 * ROW_HEIGHT).max(ROW_HEIGHT);

        // The popup is sized to its labels up front, because the SHM buffer is
        // allocated once and xdg_popup has to be told the size before the first
        // commit.
        let width = match theme::font() {
            Some(font) => {
                let scale = Scale::uniform(FONT_SIZE);
                let longest = items
                    .iter()
                    .map(|item| text_width(font, scale, &item.label))
                    .max_by(|a, b| a.total_cmp(b))
                    .unwrap_or(0.0);
                (longest.ceil() as u32).saturating_add(2 * H_PAD)
            }
            None => FALLBACK_MENU_WIDTH,
        };

        // Position the popup relative to the bar (parent) surface. The positioner's
        // coordinates are measured from the parent's top-left, so we use the
        // absolute click x (from the bar's left edge), NOT the distance from
        // the right edge. The anchor rect is a 1px-wide column at click_x
        // spanning the bar height; its anchor point is the bottom-right
        // (bar's bottom edge at the click column). Gravity BottomRight means
        // the menu is placed *toward* the bottom-right of that anchor point,
        // i.e. its top-left corner sits at the anchor and it grows downward
        // (below the bar). The offset then pulls the menu left by the menu
        // width so its right edge lands at click_x, and up to the tray icon's
        // bottom edge (bar_height * 3/4).
        let positioner = XdgPositioner::new(xdg_shell)?;
        positioner.set_anchor_rect(click_x, 0, 1, bar_height as i32);
        positioner.set_anchor(Anchor::BottomRight);
        positioner.set_gravity(Gravity::BottomRight);
        positioner.set_offset(-(width as i32), -(bar_height as i32 / 4));
        positioner.set_size(width as i32, height as i32);

        let surface = Surface::new(compositor_state, qh)?;
        let popup = Popup::from_surface(None, &positioner, qh, surface, xdg_shell)?;
        parent_layer.get_popup(popup.xdg_popup());
        popup.xdg_popup().grab(seat, serial);
        popup.wl_surface().commit();

        let pool = SlotPool::new((width * height * 4) as usize, shm)?;

        Ok(Self {
            popup,
            pool,
            width,
            height,
            owner_id,
            menu_path,
            items,
            hovered: None,
        })
    }

    /// Pointer hover in popup-local y coordinate — sets the hovered row.
    pub fn set_hover(&mut self, y: f64) {
        self.hovered = if y < 0.0 {
            None
        } else {
            let row = (y as u32) / ROW_HEIGHT;
            if (row as usize) < self.items.len() {
                Some(row as usize)
            } else {
                None
            }
        };
    }

    /// Pointer click in popup-local y coordinate — returns the menu item id, if any.
    pub fn hit_test(&self, y: f64) -> Option<i32> {
        if y < 0.0 {
            return None;
        }
        let row = (y as u32) / ROW_HEIGHT;
        self.items.get(row as usize).map(|i| i.id)
    }

    pub fn wl_surface(&self) -> &wayland_client::protocol::wl_surface::WlSurface {
        self.popup.wl_surface()
    }

    /// Paint the menu contents into its own surface's buffer.
    pub fn draw(&mut self, colors: Colors) {
        let (buffer, canvas) = match self.pool.create_buffer(
            self.width as i32,
            self.height as i32,
            self.width as i32 * 4,
            wl_shm::Format::Argb8888,
        ) {
            Ok(v) => v,
            Err(err) => {
                log::error!("menu popup buffer alloc failed: {err}");
                return;
            }
        };
        let stride = self.width * 4;

        for (row, item) in self.items.iter().enumerate() {
            let y0 = row as u32 * ROW_HEIGHT;
            let row_bg = if Some(row) == self.hovered {
                colors.background_hover
            } else {
                colors.background
            }
            .to_bgra(255);
            for y in y0..y0 + ROW_HEIGHT {
                for x in 0..self.width {
                    let off = (y * stride + x * 4) as usize;
                    if off + 3 < canvas.len() {
                        canvas[off..off + 4].copy_from_slice(&row_bg);
                    }
                }
            }

            if let Some(font) = theme::font() {
                let scale = Scale::uniform(FONT_SIZE);
                let v_metrics = font.v_metrics(scale);
                let pen_y =
                    y0 as f32 + (ROW_HEIGHT as f32 + v_metrics.ascent + v_metrics.descent) / 2.0;
                let pen_x = (self.width as f32 - text_width(font, scale, &item.label)) / 2.0;
                // Bounds are the popup's own surface, not the bar's.
                draw_text(
                    font,
                    scale,
                    canvas,
                    stride,
                    (self.width, self.height),
                    &item.label,
                    pen_x,
                    pen_y,
                    colors.accent,
                );
            }
        }

        self.popup
            .wl_surface()
            .damage_buffer(0, 0, self.width as i32, self.height as i32);
        if let Err(err) = buffer.attach_to(self.popup.wl_surface()) {
            log::error!("menu popup attach failed: {err}");
            return;
        }
        self.popup.wl_surface().commit();
    }
}

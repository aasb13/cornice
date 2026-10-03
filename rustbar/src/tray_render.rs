use crate::theme::{accent, bg, draw_bordered_box};
use crate::tray::{MenuItem, TrayCommand, TrayIcons};

pub struct TrayRenderer {
    icons: TrayIcons,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<TrayCommand>,
    /// (icon id, x_start, x_end) from the last render, for hit-testing clicks.
    layout: Vec<(String, u32, u32)>,
}

impl TrayRenderer {
    pub fn new(icons: TrayIcons, cmd_tx: tokio::sync::mpsc::UnboundedSender<TrayCommand>) -> Self {
        Self {
            icons,
            cmd_tx,
            layout: Vec::new(),
        }
    }

    /// Renders the tray as a waybar `#tray` box (black bg, 1px accent border,
    /// 14px h-padding) right-aligned ending at `right_edge`. Returns total
    /// width consumed (0 if there are no icons).
    pub fn render(&mut self, canvas: &mut [u8], right_edge: u32, height: u32, stride: u32) -> u32 {
        self.layout.clear();
        let icons = self.icons.lock().unwrap().clone();
        // Size icons to the bar's font scale (matches the widget strip's
        // 0.5×height), so tray icons are the same visual size as the bar text
        // instead of the old `height - 8` (~24px) that made them too big.
        let icon_size = (height as f32 * 0.5).round() as u32;
        let icon_size = icon_size.max(1);
        let pad = 6u32;
        let h_pad = 14u32; // waybar `#tray { padding: 0 14px }`
        let border = 1u32;
        let n = icons.len() as u32;
        if n == 0 {
            return 0;
        }
        let y_off = (height - icon_size) / 2;

        let slot = icon_size + pad;
        let content_w = n * slot;
        let box_w = 2 * border + 2 * h_pad + content_w;
        let box_x = right_edge.saturating_sub(box_w);

        draw_bordered_box(canvas, stride, box_x, 0, box_w, height, bg(), accent());

        // Icons left-to-right inside the box, each centered in its slot.
        let mut x = box_x + border + h_pad;
        for icon in icons.iter() {
            blit_scaled(
                canvas,
                stride,
                x + pad / 2,
                y_off,
                icon_size,
                icon_size,
                &icon.pixels,
                icon.width,
                icon.height,
            );
            self.layout.push((icon.id.clone(), x, x + slot));
            x += slot;
        }

        box_w
    }

    pub fn hit_test(&self, x: f64) -> Option<&str> {
        let x = x as u32;
        self.layout
            .iter()
            .find(|(_, start, end)| x >= *start && x < *end)
            .map(|(id, _, _)| id.as_str())
    }

    pub fn activate(&self, id: &str) {
        if let Err(err) = self.cmd_tx.send(TrayCommand::Activate(id.to_string())) {
            log::error!("failed to send tray activate command: {err}");
        }
    }

    pub fn menu_for(&self, id: &str) -> (Option<String>, Vec<MenuItem>) {
        self.icons
            .lock()
            .unwrap()
            .iter()
            .find(|i| i.id == id)
            .map(|i| (i.menu_path.clone(), i.menu.clone()))
            .unwrap_or((None, Vec::new()))
    }

    pub fn click_menu_item(&self, address: &str, menu_path: &str, item_id: i32) {
        let cmd = TrayCommand::ClickMenuItem {
            address: address.to_string(),
            menu_path: menu_path.to_string(),
            item_id,
        };
        if let Err(err) = self.cmd_tx.send(cmd) {
            log::error!("failed to send menu click command: {err}");
        }
    }
}

/// Nearest-neighbor scale + blit of a premultiplied BGRA source into canvas
/// at (dst_x, dst_y), sized dst_w x dst_h.
#[allow(clippy::too_many_arguments)]
fn blit_scaled(
    canvas: &mut [u8],
    stride: u32,
    dst_x: u32,
    dst_y: u32,
    dst_w: u32,
    dst_h: u32,
    src: &[u8],
    src_w: u32,
    src_h: u32,
) {
    if src_w == 0 || src_h == 0 {
        return;
    }
    for y in 0..dst_h {
        let sy = y * src_h / dst_h;
        for x in 0..dst_w {
            let sx = x * src_w / dst_w;
            let src_off = (sy * src_w + sx) as usize * 4;
            if src_off + 3 >= src.len() {
                continue;
            }
            let dst_off = ((dst_y + y) * stride + (dst_x + x) * 4) as usize;
            if dst_off + 3 >= canvas.len() {
                continue;
            }
            // simple alpha-over onto whatever's already there
            let sa = src[src_off + 3] as u32;
            if sa == 0 {
                continue;
            }
            for c in 0..4 {
                let s = src[src_off + c] as u32;
                let d = canvas[dst_off + c] as u32;
                canvas[dst_off + c] = (s + d * (255 - sa) / 255) as u8;
            }
        }
    }
}

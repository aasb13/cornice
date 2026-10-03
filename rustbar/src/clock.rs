use rusttype::Scale;

use crate::theme::{self, accent, bg, draw_bordered_box, draw_text, text_width};

pub struct Clock;

impl Clock {
    pub fn new() -> Self {
        Clock
    }

    pub fn render(&mut self, canvas: &mut [u8], left: u32, right: u32, height: u32, stride: u32) {
        let Some(font) = theme::font() else {
            return;
        };

        let text = chrono::Local::now().format("%H:%M:%S - %A, %d").to_string();
        let scale = Scale::uniform(height as f32 * 0.6);
        let v_metrics = font.v_metrics(scale);
        let glyph_height = v_metrics.ascent - v_metrics.descent;

        // ── waybar `#clock` box: black bg, 1px accent border, 14px h-padding ──
        //    `#clock { padding: 0 14px; background: #000; border: 1px solid accent }`
        const BORDER: u32 = 1;
        const H_PAD: u32 = 14;
        let box_w = 2 * BORDER + 2 * H_PAD + text_width(font, scale, &text).ceil() as u32;
        let usable_w = right.saturating_sub(left);
        let box_x = left + usable_w.saturating_sub(box_w) / 2;

        // waybar `#clock` box: black bg + 1px accent border.
        draw_bordered_box(canvas, stride, box_x, 0, box_w, height, bg(), accent());

        // text: 14px in from the box's left border, vertically centered
        let x = box_x as f32 + BORDER as f32 + H_PAD as f32;
        let y = (height as f32 - glyph_height) / 2.0 + v_metrics.ascent;
        draw_text(
            font,
            scale,
            canvas,
            stride,
            (stride / 4, height),
            &text,
            x,
            y,
            accent(),
        );
    }
}

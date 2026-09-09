use rusttype::{Font, Scale, point};

use crate::theme::{draw_bordered_box, ACCENT, BG};

pub struct Clock {
    font: Option<Font<'static>>,
}

impl Clock {
    pub fn new() -> Self {
        Self { font: None }
    }

    pub fn init(&mut self) {
        let paths = [
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
        log::error!("no font found");
    }

    pub fn render(&mut self, canvas: &mut [u8], left: u32, right: u32, height: u32, stride: u32) {
        let font = match &self.font {
            Some(f) => f,
            None => return,
        };

        let text = chrono::Local::now().format("%H:%M:%S - %A, %d").to_string();
        let scale = Scale::uniform(height as f32 * 0.6);
        let v_metrics = font.v_metrics(scale);
        let glyph_height = v_metrics.ascent - v_metrics.descent;

        let total_width: f32 = text
            .chars()
            .map(|c| font.glyph(c).scaled(scale).h_metrics().advance_width)
            .sum();

        // ── waybar `#clock` box: black bg, 1px accent border, 14px h-padding ──
        //    `#clock { padding: 0 14px; background: #000; border: 1px solid accent }`
        const BORDER: u32 = 1;
        const H_PAD: u32 = 14;
        let box_w = 2 * BORDER + 2 * H_PAD + total_width.ceil() as u32;
        let usable_w = right.saturating_sub(left);
        let box_x = left + usable_w.saturating_sub(box_w) / 2;

        // waybar `#clock` box: black bg + 1px accent border.
        draw_bordered_box(canvas, stride, box_x, 0, box_w, height, BG, ACCENT);

        // text: 14px in from the box's left border, vertically centered
        let x = box_x as f32 + BORDER as f32 + H_PAD as f32;
        let y = (height as f32 - glyph_height) / 2.0 + v_metrics.ascent;
        let mut pen = point(x, y);
        let bar_width = stride / 4;

        for c in text.chars() {
            let glyph = font.glyph(c);
            let scaled_glyph = glyph.scaled(scale);
            let advance_width = scaled_glyph.h_metrics().advance_width;
            let positioned_glyph = scaled_glyph.positioned(pen);

            if let Some(bb) = positioned_glyph.pixel_bounding_box() {
                positioned_glyph.draw(|x_rel, y_rel, v| {
                    let xi = bb.min.x + x_rel as i32;
                    let yi = bb.min.y + y_rel as i32;
                    if xi >= 0 && xi < bar_width as i32 && yi >= 0 && yi < height as i32 {
                        let off = yi as usize * stride as usize + xi as usize * 4;
                        if off + 3 < canvas.len() {
                            // gamma-correct coverage, then premultiplied
                            // src-over so accent text composites correctly over
                            // the black box instead of poking holes in it.
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
            pen.x += advance_width;
        }
    }
}

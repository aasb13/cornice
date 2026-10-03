use rusttype::Scale;

use crate::config::Colors;
use crate::theme::{self, draw_bordered_box, draw_text, fill_rect, text_width};

const NUM_SLOTS: i32 = 10;
/// Button width — waybar `#workspaces button { min-width: 22px }`.
const BOX_WIDTH: u32 = 22;
/// 1px accent border around the workspaces container
/// (waybar `#workspaces { border: 1px solid accent }`).
const BORDER: u32 = 1;

/// Which workspace is active, and which ones exist.
///
/// A snapshot rather than a live query: reading it from Hyprland is a blocking
/// socket round-trip (see [`crate::hypr`]), and painting happens on every frame
/// — including the ~120/second frames the audio visualiser drives. [`Self`]
/// holds the latest one so `render` costs no IPC at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceState {
    /// The active workspace's id, or -1 when Hyprland hasn't said.
    pub active: i32,
    /// The ids of every workspace that currently exists.
    pub existing: Vec<i32>,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        WorkspaceState {
            active: -1,
            existing: Vec::new(),
        }
    }
}

pub struct WorkspaceList {
    /// (workspace id, x_start, x_end) for the boxes drawn in the last render —
    /// used for hit-testing pointer clicks/hover.
    layout: Vec<(i32, u32, u32)>,
    /// Currently hovered slot (1..=NUM_SLOTS), for the `:hover` text color.
    hovered: Option<i32>,
    /// Latest snapshot from the Hyprland event thread.
    state: WorkspaceState,
}

impl WorkspaceList {
    pub fn new() -> Self {
        Self {
            layout: Vec::new(),
            hovered: None,
            state: WorkspaceState::default(),
        }
    }

    /// Adopt a snapshot from the Hyprland event thread.
    pub fn set_state(&mut self, state: WorkspaceState) {
        self.state = state;
    }

    /// Total pixel width this module occupies on the left of the bar.
    pub fn width(&self) -> u32 {
        2 * BORDER + NUM_SLOTS as u32 * BOX_WIDTH
    }

    pub fn render(
        &mut self,
        colors: &Colors,
        canvas: &mut [u8],
        x_offset: u32,
        height: u32,
        stride: u32,
    ) {
        self.layout.clear();
        let Some(font) = theme::font() else {
            return;
        };

        let active_id = self.state.active;

        // ── Container: opaque black background with a 1px accent border ──
        //    (waybar `#workspaces { background:#000; border:1px solid accent }`).
        let container_w = 2 * BORDER + NUM_SLOTS as u32 * BOX_WIDTH;
        draw_bordered_box(
            canvas,
            stride,
            x_offset,
            0,
            container_w,
            height,
            colors.background,
            colors.accent,
        );

        let bar_width = stride / 4;
        let scale = Scale::uniform(height as f32 * 0.5);
        let v_metrics = font.v_metrics(scale);
        let glyph_height = v_metrics.ascent - v_metrics.descent;

        for slot in 1..=NUM_SLOTS {
            // Button rect sits inside the 1px border, offset by the side margin.
            let bx = x_offset + BORDER + (slot - 1) as u32 * BOX_WIDTH;
            let by = BORDER;
            let bw = BOX_WIDTH;
            let bh = height.saturating_sub(2 * BORDER);
            self.layout.push((slot, bx, bx + bw));

            let is_active = slot == active_id;

            // active → accent background (waybar `.active { background: accent }`)
            if is_active {
                fill_rect(canvas, stride, bx, by, bw, bh, colors.accent, 255);
            }

            // Text color, matching waybar's button/empty/active/hover rules:
            //   active            → `on_accent` (readable over the accent fill)
            //   exists, not active → accent
            //   empty + hover      → accent (`:hover { color: accent }`)
            //   empty              → `inactive` (`.empty { color: #222 }`)
            let text_color = if is_active {
                colors.on_accent
            } else if self.state.existing.contains(&slot) || self.hovered == Some(slot) {
                colors.accent
            } else {
                colors.inactive
            };

            let text = slot.to_string();
            let x = bx as f32 + (bw as f32 - text_width(font, scale, &text)) / 2.0;
            let y = by as f32 + (bh as f32 - glyph_height) / 2.0 + v_metrics.ascent;
            draw_text(
                font,
                scale,
                canvas,
                stride,
                (bar_width, height),
                &text,
                x,
                y,
                text_color,
            );
        }
    }

    /// Which workspace slot (if any) the given bar-local x coordinate falls
    /// inside. Used both for cursor-icon feedback and click dispatch.
    pub fn hit_test(&self, x: f64) -> Option<i32> {
        let x = x as u32;
        self.layout
            .iter()
            .find(|(_, start, end)| x >= *start && x < *end)
            .map(|(id, _, _)| *id)
    }

    /// Update the hovered slot from a pointer x coordinate. Returns whether
    /// the hover state changed so the caller redraws only when needed.
    pub fn update_hover(&mut self, x: f64) -> bool {
        let new = self.hit_test(x);
        let changed = new != self.hovered;
        self.hovered = new;
        changed
    }

    /// Clear the hovered slot (pointer left the bar). Returns whether the
    /// hover state changed.
    pub fn clear_hover(&mut self) -> bool {
        let changed = self.hovered.is_some();
        self.hovered = None;
        changed
    }

    /// Ask Hyprland to switch to the given workspace via `hyprctl` directly
    /// rather than hyprland-rs's own socket dispatch — hyprland-rs still
    /// sends the pre-Lua-config dispatch string format, which current
    /// Hyprland's Lua-wrapped command parser rejects. `hyprctl` is kept in
    /// lockstep with Hyprland itself, so it's the more reliable path for
    /// dispatch specifically — the crate's *reads* are unaffected, which is why
    /// the workspace snapshot still comes from it (on the Hyprland event thread,
    /// where a blocking round-trip is free; see [`crate::hypr`]).
    pub fn switch_to(&self, id: i32) {
        let expr = format!("hl.dsp.focus({{ workspace = \"{id}\" }})");
        let result = std::process::Command::new("hyprctl")
            .args(["dispatch", &expr])
            .output();
        match result {
            Ok(out) if !out.status.success() => {
                log::error!(
                    "hyprctl dispatch failed (status {:?}): stdout={:?} stderr={:?}",
                    out.status.code(),
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            Err(err) => log::error!("failed to run hyprctl: {err}"),
            _ => {}
        }
    }
}

//! The extension point.
//!
//! `rustwall` itself only knows how to talk to Wayland: claim a monitor,
//! open a `wlr-layer-shell` background surface, hand you a pixel buffer.
//! It has no idea what an image, a video, or an audio visualizer is — and
//! it doesn't need to. All of that lives in a `FrameSource` implementation
//! compiled directly into this binary. Nothing external, no IPC, no socket:
//! you write a struct, implement this trait, and select it in `main.rs`.

use std::time::Duration;

/// Something that can render pixels into the wallpaper.
pub trait FrameSource {
    /// Called exactly once, right after the real pixel dimensions of the
    /// wallpaper surface are known (i.e. after the first Wayland `configure`
    /// event). Use this to load an image, start a decoder, connect to
    /// PulseAudio for cava-style visualization, etc.
    fn init(&mut self, width: u32, height: u32) {
        let _ = (width, height);
    }

    /// Render one frame.
    ///
    /// `canvas` is exactly `height * stride` bytes: `height` rows of
    /// `stride` bytes each (`stride == width * 4` — rustwall doesn't add
    /// row padding), 4 bytes per pixel, `XRGB8888` (i.e. each pixel is the
    /// four bytes `B, G, R, X` in memory, native/little-endian).
    ///
    /// Return `true` if you actually drew something new (rustwall will
    /// damage the surface and commit it to the compositor). Return `false`
    /// to skip this tick with no commit — cheap for e.g. an audio
    /// visualizer that has nothing new to show yet.
    fn render(&mut self, canvas: &mut [u8], width: u32, height: u32, stride: u32) -> bool;

    /// How often `render` should be called after the first draw.
    ///
    /// - `None` (the default): static content. `render` is called once,
    ///   right after `init`, and never again — zero CPU use afterwards.
    /// - `Some(duration)`: `render` is called repeatedly, roughly every
    ///   `duration` (video, cava, a clock, anything that changes on its
    ///   own).
    fn tick_interval(&self) -> Option<Duration> {
        None
    }
}

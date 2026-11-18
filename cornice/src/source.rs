//! The extension point.
//!
//! `cornice` owns the display plumbing: claim a monitor, open a background
//! surface (a Wayland layer or an X11 window), hand you a pixel buffer.
//! It has no idea what an image, a video, or an audio visualizer is — and
//! it doesn't need to. All of that lives in a `FrameSource` implementation
//! compiled directly into this binary. Nothing external, no IPC, no socket:
//! you write a struct, implement this trait, and select it in `main.rs`.

use std::time::Duration;

/// Something that can render pixels into the wallpaper.
pub trait FrameSource {
    /// Called exactly once, right after the real pixel dimensions of the
    /// wallpaper surface are known (after the first Wayland `configure` event,
    /// or once the X11 window has been sized). Use this to load an image,
    /// start a decoder, connect to PulseAudio for cava-style visualization,
    /// etc.
    fn init(&mut self, width: u32, height: u32) {
        let _ = (width, height);
    }

    /// Render one frame.
    ///
    /// `canvas` is exactly `height * stride` bytes: `height` rows of
    /// `stride` bytes each (`stride == width * 4` — cornice doesn't add
    /// row padding), 4 bytes per pixel, `XRGB8888` (i.e. each pixel is the
    /// four bytes `B, G, R, X` in memory, native/little-endian).
    ///
    /// Return `true` if you actually drew something new (cornice will publish
    /// the frame to the display server). Return `false` to skip this tick with
    /// no commit — cheap for e.g. an audio visualizer that has nothing new to
    /// show yet.
    fn render(&mut self, canvas: &mut [u8], width: u32, height: u32, stride: u32) -> bool;

    /// How often `render` should be called after the first draw.
    ///
    /// - `None` (the default): static content. `render` is called once,
    ///   right after `init`, and never again — zero CPU use afterwards.
    /// - `Some(duration)`: `render` is called repeatedly, roughly every
    ///   `duration` (video, cava, a clock, anything that changes on its
    ///   own).
    ///
    /// cornice re-reads this after every frame, so a source can change the
    /// answer at runtime — that's how a frame-rate cap follows the display
    /// refresh rate, or drops when the machine goes onto battery.
    fn tick_interval(&self) -> Option<Duration> {
        None
    }

    /// Called when the power budget changes — the machine's battery state, the
    /// `[power_save]` settings, or both — so an animated source can spend less
    /// effort.
    ///
    /// `state.throttling` is true only while a battery exists *and* is
    /// discharging *and* power saving is enabled: a desktop never gets this, and
    /// a plugged-in laptop gets `false` again.
    ///
    /// Implementations must return to full speed when handed `throttling: false`
    /// — that is a temporary state, not a mode to latch. The rest of the struct
    /// says how hard to scale back while it is true, and is ignored otherwise, so
    /// a source that draws nothing animated can leave this alone. The default
    /// does nothing.
    fn set_power_saving(&mut self, _state: crate::config::PowerState) {}

    /// Apply a freshly-loaded config, live, without restarting the process.
    ///
    /// Called after the config file changes on disk (or on `SIGHUP`), so a colour
    /// tweak shows up immediately. The default does nothing, which is correct
    /// for a source with no configurable state.
    ///
    /// `width`/`height` are the surface's current dimensions, so a source that
    /// has to rebuild caches (rasterized glyphs, geometry) can do it without
    /// waiting for the next `init`. Implementations must leave the animation
    /// running where they can — a source that restarts its clock on every
    /// reload would visibly jump.
    fn reconfigure(&mut self, _config: &crate::config::Config, _width: u32, _height: u32) {}
}

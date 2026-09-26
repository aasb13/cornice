mod config;
mod power;
mod source;
mod sources;
mod wayland_backend;

use anyhow::Result;
use clap::Parser;

/// rustwall — a minimal Wayland wallpaper platform.
///
/// This binary does all the Wayland/layer-shell plumbing: claiming a
/// monitor, opening a `wlr-layer-shell` background surface, managing the
/// pixel buffer. It renders whatever `source::FrameSource` you give it.
///
/// Tuning comes from the config file at `~/.config/rustwall/config.toml`
/// (created on first run with all options commented out), e.g. overriding
/// the font paths used by the cube3d source.
#[derive(Parser, Debug)]
#[command(name = "rustwall", about = "A minimal Wayland wallpaper platform")]
struct Args {
    /// Which output (monitor) to attach to. Defaults to the last one the
    /// compositor reports.
    #[arg(long)]
    monitor: Option<String>,

    /// Enable debug logging.
    #[arg(short, long)]
    debug: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let default_level = if args.debug { "debug" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_level))
        .init();

    let config = config::load()?;

    let wp = &config.wallpaper;

    let active_source: Box<dyn source::FrameSource> =
        Box::new(sources::cube3d::RotatingTesseract::new(
            wp.font_path.clone(),
            wp.waterfall_font_path.clone(),
            wp.fps_cap,
            wp.power_save.clone(),
        ));

    wayland_backend::run(args.monitor, active_source, wp.power_save.clone())
}

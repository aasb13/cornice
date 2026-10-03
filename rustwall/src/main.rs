mod config;
mod power;
mod source;
mod sources;
mod wayland_backend;

use anyhow::{Result, bail};
use clap::Parser;
use std::path::PathBuf;

/// rustwall — a minimal Wayland wallpaper platform.
///
/// This binary does all the Wayland/layer-shell plumbing: claiming a
/// monitor, opening a `wlr-layer-shell` background surface, managing the
/// pixel buffer. It renders whatever `source::FrameSource` you give it.
///
/// Everything tunable lives in the config file at `~/.config/rustwall/config.toml`
/// (created on first run as a fully commented template of every option), or
/// wherever `--config` points.
#[derive(Parser, Debug)]
#[command(name = "rustwall", about = "A minimal Wayland wallpaper platform")]
struct Args {
    /// Which output (monitor) to attach to. Defaults to the last one the
    /// compositor reports.
    #[arg(long)]
    monitor: Option<String>,

    /// Config file to use instead of `~/.config/rustwall/config.toml`.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Print the effective config, fully resolved, and exit without drawing.
    #[arg(long)]
    print_config: bool,

    /// Print the commented config template that a missing config file is
    /// created from, and exit.
    #[arg(long)]
    print_default_config: bool,

    /// Enable debug logging.
    #[arg(short, long)]
    debug: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.print_default_config {
        print!("{}", config::default_config_text());
        return Ok(());
    }

    let default_level = if args.debug { "debug" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_level))
        .init();

    let wp = config::Config::load(args.config.as_deref())?;

    if args.print_config {
        print!("{}", toml::to_string_pretty(&wp)?);
        return Ok(());
    }

    let active_source: Box<dyn source::FrameSource> = match wp.source.as_str() {
        "cube3d" => Box::new(sources::cube3d::RotatingTesseract::new(
            wp.cube3d.clone(),
            wp.fps_cap,
            wp.power_save.clone(),
        )),
        other => bail!("unknown wallpaper source {other:?}"),
    };

    wayland_backend::run(args.monitor, active_source, wp.power_save)
}

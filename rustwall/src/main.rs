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
/// wherever `--config` points. Changes apply without restarting: the file is
/// watched automatically (see `[reload]`), or `rustwall --reload` tells the
/// running instance to re-read now.
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

    /// Ask any running rustwall to re-read its config, then exit. Works
    /// whether or not `[reload] watch` is enabled.
    #[arg(long)]
    reload: bool,

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

    // Before the logger and before touching the config, because this invocation
    // only signals another process and exits — it has no wallpaper to draw.
    if args.reload {
        let n = rwconfig::signal_siblings()?;
        if n == 0 {
            bail!("no running rustwall instance found");
        }
        println!("asked {n} running rustwall instance(s) to reload");
        return Ok(());
    }

    let default_level = if args.debug { "debug" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_level))
        .init();

    // Before anything spawns a thread: every thread must block SIGHUP for the
    // event loop's signalfd to receive `--reload` rather than the kernel
    // delivering it to a worker and killing the process.
    rwconfig::block_early()?;

    // Opens the config, creating a commented template on first run. A config
    // that doesn't parse is fatal here — there is no previous one to fall back
    // to — whereas a bad edit later is logged and ignored.
    let reload = rwconfig::Reload::<config::Config>::open(
        "rustwall",
        config::CONFIG_HEADER,
        args.config.as_deref(),
    )?;
    let wp = reload.config();

    if args.print_config {
        print!("{}", toml::to_string_pretty(wp)?);
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

    wayland_backend::run(args.monitor, active_source, reload)
}

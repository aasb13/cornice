mod cava;
mod clock;
mod config;
mod hypr;
mod menu_popup;
mod theme;
mod tray;
mod tray_render;
mod wayland_backend;
mod widgets;
mod workspaces;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    /// Which output (monitor) to attach to. Defaults to the leftmost one, which
    /// is the same screen `rustwall` picks.
    #[arg(long)]
    monitor: Option<String>,
    #[arg(long, default_value_t = 32)]
    height: u32,

    /// Config file to use instead of `~/.config/rustbar/config.toml`.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Print the effective config and exit, without drawing.
    #[arg(long)]
    print_config: bool,

    /// Print the commented config template a missing config file is created
    /// from, and exit.
    #[arg(long)]
    print_default_config: bool,

    /// Ask any running rustbar to re-read its config, then exit. Works whether
    /// or not `[reload] watch` is enabled.
    #[arg(long)]
    reload: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.print_default_config {
        print!("{}", config::default_config_text());
        return Ok(());
    }

    // Before the logger and before touching the config: this invocation only
    // signals another process and exits, so it has no bar to draw.
    if args.reload {
        let n = rwconfig::signal_siblings()?;
        if n == 0 {
            anyhow::bail!("no running rustbar instance found");
        }
        println!("asked {n} running rustbar instance(s) to reload");
        return Ok(());
    }

    env_logger::init();

    // Before anything spawns a thread: every thread must block SIGHUP for the
    // event loop's signalfd to receive `--reload` rather than the kernel
    // delivering it to a worker (the cava reader, the tray client) and killing
    // the process.
    rwconfig::block_early()?;

    // Opens the config, creating a commented template on first run. A config
    // that doesn't parse is fatal here — there is no previous one to fall back
    // to — whereas a bad edit later is logged and ignored.
    let reload = rwconfig::Reload::<config::Config>::open(
        "rustbar",
        config::CONFIG_HEADER,
        args.config.as_deref(),
    )?;

    if args.print_config {
        print!("{}", toml::to_string_pretty(reload.config())?);
        return Ok(());
    }

    wayland_backend::run(
        args.monitor,
        args.height,
        Box::new(clock::Clock::new()),
        reload,
    )
}

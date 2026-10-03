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
}

fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

    if args.print_default_config {
        print!("{}", config::default_config_text());
        return Ok(());
    }

    let config = config::Config::load(args.config.as_deref())?;

    if args.print_config {
        print!("{}", toml::to_string_pretty(&config)?);
        return Ok(());
    }

    theme::init(&config);
    wayland_backend::run(args.monitor, args.height, Box::new(clock::Clock::new()))
}

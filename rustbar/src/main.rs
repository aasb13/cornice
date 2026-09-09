mod wayland_backend;
mod clock;
mod cava;
mod hypr;
mod workspaces;
mod tray;
mod tray_render;
mod widgets;
mod menu_popup;
mod theme;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    monitor: Option<String>,
    #[arg(long, default_value_t = 32)]
    height: u32,
}

fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();
    wayland_backend::run(args.monitor, args.height, Box::new(clock::Clock::new()))
}

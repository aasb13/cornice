//! The X11 half of the wallpaper.
//!
//! X11 has no `wlr-layer-shell`: the closest equivalent of a background layer
//! is an override-redirect window lowered to the bottom of the stack and sized
//! to one RandR monitor. It is drawn with `PutImage` into a plain SHM-style byte
//! buffer, which is exactly the `XRGB8888` layout `FrameSource` already writes.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use calloop::EventLoop;
use calloop::channel::{self, Sender};
use calloop::timer::{TimeoutAction, Timer};
use log::{error, info};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{
    ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, Gcontext,
    ImageFormat, Pixmap, StackMode, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;

use crate::config::Config;
use crate::display::{leftmost, names};
use crate::source::FrameSource;
use crate::wallpaper::Wallpaper;
use cornice_config::Reload;

/// A monitor as X11 describes it: an optional RandR name, its top-left corner
/// and its size in pixels.
type Monitor = (Option<String>, (i32, i32), u32, u32);

/// What the blocking event-reader thread forwards to the event loop.
enum XEvent {
    Expose,
    Destroy,
}

/// Spawn the one thread that blocks on the X socket. Everything else —
/// `PutImage`, config polls, timers — stays on the calloop thread, matching the
/// rule the Wayland backend follows: blocking waits never run on the loop.
fn spawn_event_reader(conn: Arc<RustConnection>, window: Window, tx: Sender<XEvent>) {
    std::thread::spawn(move || {
        loop {
            match conn.wait_for_event() {
                Ok(Event::Expose(event)) if event.window == window => {
                    if tx.send(XEvent::Expose).is_err() {
                        break;
                    }
                }
                Ok(Event::DestroyNotify(event)) if event.window == window => {
                    let _ = tx.send(XEvent::Destroy);
                    break;
                }
                Ok(Event::Error(err)) => error!("x11 error: {err:?}"),
                Ok(_) => {}
                Err(err) => {
                    error!("x11 event reader stopped: {err}");
                    let _ = tx.send(XEvent::Destroy);
                    break;
                }
            }
        }
    });
}

fn monitor_name(conn: &RustConnection, atom: u32) -> Option<String> {
    let bytes = conn.get_atom_name(atom).ok()?.reply().ok()?.name;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// RandR monitors give us names and geometry, which is what `--monitor` and the
/// leftmost-output rule need. A server without RandR 1.5 still gets one usable
/// output covering the whole screen.
fn monitors(conn: &RustConnection, screen_num: usize) -> Vec<Monitor> {
    let screen = &conn.setup().roots[screen_num];
    let reply = conn
        .randr_get_monitors(screen.root, true)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .filter(|reply| !reply.monitors.is_empty());
    match reply {
        Some(reply) => reply
            .monitors
            .iter()
            .map(|m| {
                (
                    monitor_name(conn, m.name),
                    (m.x as i32, m.y as i32),
                    m.width as u32,
                    m.height as u32,
                )
            })
            .collect(),
        None => {
            info!("RandR get-monitors unavailable; using the whole screen");
            vec![(
                Some(format!("screen{screen_num}")),
                (0, 0),
                screen.width_in_pixels as u32,
                screen.height_in_pixels as u32,
            )]
        }
    }
}

/// Pick `monitor`, or the leftmost one when no name was given.
fn choose(monitor: Option<&str>, found: &[Monitor]) -> Result<usize> {
    let described: Vec<_> = found
        .iter()
        .map(|(name, (x, y), _, _)| (name.clone(), (*x, *y)))
        .collect();
    match monitor {
        Some(name) => described
            .iter()
            .position(|(n, _)| n.as_deref() == Some(name))
            .ok_or_else(|| anyhow!("monitor '{name}' not found among {:?}", names(&described))),
        None => {
            let i = leftmost(&described).context("no X11 outputs found")?;
            info!(
                "no monitor specified, using {:?}",
                described[i].0.as_deref()
            );
            Ok(i)
        }
    }
}

/// Upload one frame in request-sized chunks. A 4K frame is larger than the
/// default maximum X request, so a single `PutImage` would kill the connection;
/// splitting by rows keeps each request within the server's limit.
fn upload(
    conn: &RustConnection,
    window: Window,
    gc: Gcontext,
    width: u32,
    height: u32,
    depth: u8,
    pixels: &[u8],
) -> Result<()> {
    let row = width as usize * 4;
    let limit = conn.maximum_request_bytes();
    let rows_per_request = limit.saturating_sub(64) / row.max(1);
    let rows_per_request = rows_per_request.clamp(1, height as usize);
    let mut y = 0usize;
    while y < height as usize {
        let rows = rows_per_request.min(height as usize - y);
        conn.put_image(
            ImageFormat::Z_PIXMAP,
            window,
            gc,
            width as u16,
            rows as u16,
            0,
            y as i16,
            0,
            depth,
            &pixels[y * row..(y + rows) * row],
        )
        .context("PutImage failed")?;
        y += rows;
    }
    conn.flush().context("failed to flush X11 connection")?;
    Ok(())
}

/// Claim `monitor` (or the leftmost one) and render `source` into a background
/// window on it. Blocks until the window goes away.
pub fn run(
    monitor: Option<String>,
    source: Box<dyn FrameSource>,
    reload: Reload<Config>,
) -> Result<()> {
    let (connection, screen_num) =
        RustConnection::connect(None).context("failed to connect to X display")?;
    let conn = Arc::new(connection);
    let screen = &conn.setup().roots[screen_num];

    let found = monitors(&conn, screen_num);
    let chosen = choose(monitor.as_deref(), &found)?;
    let (name, (mx, my), width, height) = found[chosen].clone();
    info!("targeting output {name:?} ({width}x{height})");

    let window = conn
        .generate_id()
        .context("failed to allocate X window id")?;
    conn.create_window(
        screen.root_depth,
        window,
        screen.root,
        mx as i16,
        my as i16,
        width as u16,
        height as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .override_redirect(1u32)
            .background_pixmap(Pixmap::from(0u32))
            .event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY),
    )
    .context("failed to create X window")?;
    let gc = conn.generate_id().context("failed to allocate X gc id")?;
    conn.create_gc(gc, window, &CreateGCAux::new())
        .context("failed to create X graphics context")?;
    conn.map_window(window).context("failed to map X window")?;
    // Override-redirect windows are not managed, so the stacking we ask for is
    // the stacking we get: below every other window, i.e. a wallpaper.
    conn.configure_window(
        window,
        &ConfigureWindowAux::new().stack_mode(StackMode::BELOW),
    )
    .context("failed to lower X window")?;
    conn.flush().context("failed to flush X11 connection")?;

    let mut app = App {
        conn: Arc::clone(&conn),
        window,
        gc,
        depth: screen.root_depth,
        width,
        height,
        pixels: vec![0; (width as usize * 4).saturating_mul(height as usize)],
        wallpaper: Wallpaper::new(source, reload),
        exit: false,
    };
    app.wallpaper.set_dimensions(width, height);
    app.wallpaper.init(width, height);
    app.draw()?;

    let (event_tx, event_rx) = channel::channel::<XEvent>();
    spawn_event_reader(Arc::clone(&conn), window, event_tx);

    let mut event_loop = EventLoop::try_new().context("failed to create event loop")?;
    let handle = event_loop.handle();
    handle
        .insert_source(event_rx, move |event, _, app: &mut App| match event {
            channel::Event::Msg(XEvent::Expose) => {
                if let Err(err) = app.draw() {
                    error!("failed to redraw wallpaper: {err}");
                }
            }
            channel::Event::Msg(XEvent::Destroy) | channel::Event::Closed => {
                app.exit = true;
            }
        })
        .map_err(|e| anyhow!("failed to insert X11 event source: {e}"))?;

    let initial = app.wallpaper.interval();
    if let Some(interval) = initial {
        handle
            .insert_source(
                Timer::from_duration(interval),
                move |_deadline, _, app: &mut App| {
                    if let Err(err) = app.draw() {
                        error!("failed to draw wallpaper: {err}");
                    }
                    let next = app.wallpaper.tick_interval().unwrap_or(interval);
                    TimeoutAction::ToDuration(next)
                },
            )
            .map_err(|e| anyhow!("failed to insert render timer: {e}"))?;
    }

    #[cfg(unix)]
    handle
        .insert_source(
            calloop::signals::Signals::new(&[calloop::signals::Signal::SIGHUP])
                .context("failed to create SIGHUP source")?,
            |_, _, app: &mut App| {
                app.wallpaper.force_reload();
                if let Err(err) = app.draw() {
                    error!("failed to redraw wallpaper: {err}");
                }
            },
        )
        .map_err(|e| anyhow!("failed to insert SIGHUP source: {e}"))?;

    while !app.exit {
        event_loop
            .dispatch(None, &mut app)
            .context("event loop failed")?;
    }
    Ok(())
}

struct App {
    conn: Arc<RustConnection>,
    window: Window,
    gc: Gcontext,
    depth: u8,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    wallpaper: Wallpaper,
    exit: bool,
}

impl App {
    fn draw(&mut self) -> Result<()> {
        let (width, height) = (self.width, self.height);
        if width == 0 || height == 0 {
            return Ok(());
        }
        let stride = width * 4;
        let needed = stride as usize * height as usize;
        if self.pixels.len() != needed {
            self.pixels.resize(needed, 0);
        }
        if self
            .wallpaper
            .render(&mut self.pixels, width, height, stride)
        {
            upload(
                &self.conn,
                self.window,
                self.gc,
                width,
                height,
                self.depth,
                &self.pixels,
            )?;
        }
        Ok(())
    }
}

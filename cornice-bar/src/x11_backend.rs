//! The X11 half of the status bar.
//!
//! X11 has no layer shell, xdg popups or compositor seat. The equivalents used
//! here are an override-redirect window stacked above the desktop with an EWMH
//! dock strut, an override-redirect menu window with a pointer grab for
//! click-outside-to-close, and core-pointer button/scroll events. Everything
//! drawn into those windows — and every click decision — is shared with the
//! Wayland backend through [`crate::render`].

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
    Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageData, ClientMessageEvent,
    ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, Cursor as XCursor,
    EventMask, Gcontext, GrabMode, ImageFormat, Pixmap, PropMode, StackMode, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::clock::Clock;
use crate::config::Config;
use crate::display::{leftmost, names};
use crate::menu_popup::{menu_hit_test, menu_size, render_menu};
use crate::render::{Bar, MenuRequest, PointerButton};
use crate::theme::TOP_MARGIN;
use crate::tray;
use crate::tray_render::TrayRenderer;
use crate::workspaces::{WorkspaceList, WorkspaceState};
use cornice_config::Reload;

type Monitor = (Option<String>, (i32, i32), u32, u32);

/// Events the reader thread forwards. Each one carries the window it belongs
/// to, because the bar, the menu and the root all share the same X socket.
enum XEvent {
    Expose(Window),
    ButtonPress {
        window: Window,
        button: u8,
        x: i16,
        y: i16,
    },
    Motion {
        window: Window,
        x: i16,
        y: i16,
    },
    Leave(Window),
    /// Root properties changed: re-read EWMH desktop state.
    DesktopChanged,
    Destroy(Window),
}

struct Atoms {
    current_desktop: Atom,
    number_of_desktops: Atom,
    window_type: Atom,
    window_type_dock: Atom,
    strut_partial: Atom,
}

fn intern(conn: &RustConnection, name: &str) -> Result<Atom> {
    let atom = conn
        .intern_atom(false, name.as_bytes())
        .with_context(|| format!("failed to intern atom {name}"))?
        .reply()
        .with_context(|| format!("failed to intern atom {name}"))?
        .atom;
    Ok(atom)
}

fn atoms(conn: &RustConnection) -> Result<Atoms> {
    Ok(Atoms {
        current_desktop: intern(conn, "_NET_CURRENT_DESKTOP")?,
        number_of_desktops: intern(conn, "_NET_NUMBER_OF_DESKTOPS")?,
        window_type: intern(conn, "_NET_WM_WINDOW_TYPE")?,
        window_type_dock: intern(conn, "_NET_WM_WINDOW_TYPE_DOCK")?,
        strut_partial: intern(conn, "_NET_WM_STRUT_PARTIAL")?,
    })
}

/// The blocking X event reader. Only this thread waits on the socket; the bar
/// itself stays on the calloop thread like the Wayland backend.
fn spawn_event_reader(conn: Arc<RustConnection>, root: Window, tx: Sender<XEvent>) {
    std::thread::spawn(move || {
        loop {
            let event = match conn.wait_for_event() {
                Ok(event) => event,
                Err(err) => {
                    error!("x11 event reader stopped: {err}");
                    let _ = tx.send(XEvent::Destroy(root));
                    break;
                }
            };
            let forwarded = match event {
                Event::Expose(e) => Some(XEvent::Expose(e.window)),
                Event::ButtonPress(e) => Some(XEvent::ButtonPress {
                    window: e.event,
                    button: e.detail,
                    x: e.event_x,
                    y: e.event_y,
                }),
                Event::MotionNotify(e) => Some(XEvent::Motion {
                    window: e.event,
                    x: e.event_x,
                    y: e.event_y,
                }),
                Event::LeaveNotify(e) => Some(XEvent::Leave(e.event)),
                Event::PropertyNotify(e) if e.window == root => Some(XEvent::DesktopChanged),
                Event::DestroyNotify(e) => Some(XEvent::Destroy(e.window)),
                Event::Error(err) => {
                    error!("x11 error: {err:?}");
                    None
                }
                _ => None,
            };
            if let Some(event) = forwarded
                && tx.send(event).is_err()
            {
                break;
            }
        }
    });
}

fn monitor_name(conn: &RustConnection, atom: u32) -> Option<String> {
    let bytes = conn.get_atom_name(atom).ok()?.reply().ok()?.name;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

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
        None => vec![(
            Some(format!("screen{screen_num}")),
            (0, 0),
            screen.width_in_pixels as u32,
            screen.height_in_pixels as u32,
        )],
    }
}

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

fn read_cardinal(conn: &RustConnection, window: Window, atom: Atom) -> Option<u32> {
    let reply = conn
        .get_property(false, window, atom, AtomEnum::CARDINAL, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    reply.value32()?.next()
}

/// Translate EWMH's zero-based desktop numbers into the one-based slots the
/// workspace widget draws.
fn desktop_state(conn: &RustConnection, root: Window, atoms: &Atoms) -> WorkspaceState {
    let current = read_cardinal(conn, root, atoms.current_desktop).unwrap_or(0);
    let count = read_cardinal(conn, root, atoms.number_of_desktops).unwrap_or(1);
    desktop_state_from(current, count)
}

fn desktop_state_from(current: u32, count: u32) -> WorkspaceState {
    // The widget has a fixed set of slots; a WM with more desktops than that
    // can only ever show the first ten.
    WorkspaceState {
        active: (current + 1).min(10) as i32,
        existing: (1..=count.min(10) as i32).collect(),
    }
}

/// Upload one frame in request-sized chunks, as in the wallpaper backend.
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
    let per = (limit.saturating_sub(64) / row.max(1)).clamp(1, height as usize);
    let mut y = 0usize;
    while y < height as usize {
        let rows = per.min(height as usize - y);
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

/// A tray context menu on X11: a small override-redirect window plus a pointer
/// grab so a click anywhere outside it dismisses the menu.
struct XMenu {
    window: Window,
    gc: Gcontext,
    width: u32,
    height: u32,
    request: MenuRequest,
    hovered: Option<usize>,
}

/// Claim `monitor` (or the leftmost one) and draw the bar into an
/// override-redirect window on it. Blocks until the window is destroyed.
#[allow(clippy::too_many_arguments)]
pub fn run(
    monitor: Option<String>,
    bar_height: u32,
    clock: Box<Clock>,
    reload: Reload<Config>,
) -> Result<()> {
    let (connection, screen_num) =
        RustConnection::connect(None).context("failed to connect to X display")?;
    let conn = Arc::new(connection);
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;
    let atoms = atoms(&conn)?;

    let found = monitors(&conn, screen_num);
    let chosen = choose(monitor.as_deref(), &found)?;
    let (name, (mx, my), width, height) = found[chosen].clone();
    info!("targeting output {name:?} ({width}x{height})");

    let depth = screen.root_depth;
    let visual = screen.root_visual;

    let bar_y = my + TOP_MARGIN;
    let window = conn
        .generate_id()
        .context("failed to allocate X window id")?;
    conn.create_window(
        depth,
        window,
        root,
        mx as i16,
        bar_y as i16,
        width as u16,
        bar_height as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &CreateWindowAux::new()
            .override_redirect(1u32)
            .background_pixmap(Pixmap::from(0u32))
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::STRUCTURE_NOTIFY
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::ENTER_WINDOW
                    | EventMask::LEAVE_WINDOW,
            ),
    )
    .context("failed to create bar window")?;
    let gc = conn.generate_id().context("failed to allocate X gc id")?;
    conn.create_gc(gc, window, &CreateGCAux::new())
        .context("failed to create graphics context")?;
    conn.map_window(window)
        .context("failed to map bar window")?;
    conn.configure_window(
        window,
        &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
    )
    .context("failed to raise bar window")?;

    // A dock window type and a partial strut are the closest X11 has to the
    // exclusive zone a Wayland layer surface gets. WMs that honour EWMH will
    // keep tiled windows below the bar.
    conn.change_property32(
        PropMode::REPLACE,
        window,
        atoms.window_type,
        AtomEnum::ATOM,
        &[atoms.window_type_dock],
    )?;
    let top = (bar_height as i32 + my).max(0) as u32;
    conn.change_property32(
        PropMode::REPLACE,
        window,
        atoms.strut_partial,
        AtomEnum::CARDINAL,
        &[
            0,
            0,
            top,
            0,
            0,
            0,
            0,
            0,
            mx.max(0) as u32,
            (mx + width as i32).max(0) as u32,
            0,
            0,
        ],
    )?;

    // Property changes on the root tell us when the WM switches desktops.
    conn.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )
    .context("failed to watch root window properties")?;

    let switch_conn = Arc::clone(&conn);
    let switch_root = root;
    let switch_atom = atoms.current_desktop;
    let switcher = Box::new(move |slot: i32| {
        let desktop = (slot - 1).max(0) as u32;
        if let Err(err) = switch_conn.send_event(
            false,
            switch_root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            ClientMessageEvent {
                response_type: 33,
                sequence: 0,
                format: 32,
                window: switch_root,
                type_: switch_atom,
                data: ClientMessageData::from([desktop, 0, 0, 0, 0]),
            },
        ) {
            error!("failed to send EWMH desktop switch: {err}");
        }
        let _ = switch_conn.flush();
    });

    let (tray_tx, tray_rx) = channel::channel::<()>();
    let (tray_icons, tray_cmd_tx) = tray::spawn(tray_tx);
    let mut bar = Bar::new(
        clock,
        Box::new(WorkspaceList::with_switcher(switcher)),
        reload,
        Some(TrayRenderer::new(tray_icons, tray_cmd_tx)),
    );
    bar.workspaces.set_state(desktop_state(&conn, root, &atoms));
    let (cava_tx, cava_rx) = channel::channel::<()>();
    bar.cava.spawn(cava_tx);

    conn.flush().context("failed to flush X11 connection")?;

    let mut app = App {
        conn: Arc::clone(&conn),
        root,
        atoms,
        bar_window: window,
        bar_gc: gc,
        visual,
        depth,
        bar_x: mx,
        bar_y,
        width,
        height: bar_height,
        pixels: vec![0; (width as usize * 4).saturating_mul(bar_height as usize)],
        bar,
        menu: None,
        exit: false,
    };
    app.draw_bar()?;

    let (event_tx, event_rx) = channel::channel::<XEvent>();
    spawn_event_reader(Arc::clone(&conn), root, event_tx);

    let mut event_loop = EventLoop::try_new().context("failed to create event loop")?;
    let handle = event_loop.handle();
    handle
        .insert_source(event_rx, move |event, _, app: &mut App| {
            if let calloop::channel::Event::Msg(event) = event
                && let Err(err) = app.handle_event(event)
            {
                error!("x11 bar event failed: {err}");
            }
        })
        .map_err(|e| anyhow!("failed to insert X11 event source: {e}"))?;

    // Clock refresh: one second, aligned like the Wayland bar.
    let into_second = chrono::Local::now().timestamp_subsec_millis() as u64;
    let first = std::time::Duration::from_millis(1000 - into_second.min(999));
    handle
        .insert_source(
            Timer::from_duration(first),
            move |_deadline, _, app: &mut App| {
                if let Err(err) = app.draw_bar() {
                    error!("failed to draw bar: {err}");
                }
                TimeoutAction::ToDuration(std::time::Duration::from_secs(1))
            },
        )
        .map_err(|e| anyhow!("failed to insert clock timer: {e}"))?;

    let reload_interval = app.bar.reload.interval();
    handle
        .insert_source(
            Timer::from_duration(reload_interval),
            move |_deadline, _, app: &mut App| {
                app.bar.check_reload();
                if let Err(err) = app.draw_bar() {
                    error!("failed to draw bar: {err}");
                }
                TimeoutAction::ToDuration(app.bar.reload.interval())
            },
        )
        .map_err(|e| anyhow!("failed to insert reload timer: {e}"))?;

    handle
        .insert_source(tray_rx, move |event, _, app: &mut App| {
            if let calloop::channel::Event::Msg(()) = event
                && let Err(err) = app.draw_bar()
            {
                error!("failed to redraw tray: {err}");
            }
        })
        .map_err(|e| anyhow!("failed to insert tray source: {e}"))?;
    handle
        .insert_source(cava_rx, move |event, _, app: &mut App| {
            if let calloop::channel::Event::Msg(()) = event
                && let Err(err) = app.draw_bar()
            {
                error!("failed to redraw cava: {err}");
            }
        })
        .map_err(|e| anyhow!("failed to insert cava source: {e}"))?;

    #[cfg(unix)]
    handle
        .insert_source(
            calloop::signals::Signals::new(&[calloop::signals::Signal::SIGHUP])
                .context("failed to create SIGHUP source")?,
            |_, _, app: &mut App| {
                app.bar.force_reload();
                if let Err(err) = app.draw_bar() {
                    error!("failed to redraw bar: {err}");
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
    root: Window,
    atoms: Atoms,
    bar_window: Window,
    bar_gc: Gcontext,
    visual: u32,
    depth: u8,
    bar_x: i32,
    bar_y: i32,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    bar: Bar,
    menu: Option<XMenu>,
    exit: bool,
}

impl App {
    fn draw_bar(&mut self) -> Result<()> {
        let (width, height) = (self.width, self.height);
        if width == 0 || height == 0 {
            return Ok(());
        }
        let stride = width * 4;
        let needed = stride as usize * height as usize;
        if self.pixels.len() != needed {
            self.pixels.resize(needed, 0);
        }
        self.bar.render(&mut self.pixels, width, height, stride);
        upload(
            &self.conn,
            self.bar_window,
            self.bar_gc,
            width,
            height,
            self.depth,
            &self.pixels,
        )
    }

    fn close_menu(&mut self) {
        if self.menu.take().is_some() {
            let _ = self.conn.ungrab_pointer(0u32);
            let _ = self.conn.flush();
        }
    }

    fn open_menu(&mut self, request: MenuRequest) {
        self.close_menu();
        let (menu_w, menu_h) = menu_size(&request.items);
        // Match the Wayland popup: right edge at the click, top just below the
        // tray icon, dropping down from the bar.
        let right = self.bar_x + request.x as i32;
        let top = self.bar_y + self.height as i32 - self.height as i32 / 4;
        let x = right - menu_w as i32;
        let y = top;
        let window = match self.conn.generate_id() {
            Ok(id) => id,
            Err(err) => {
                error!("failed to allocate menu window id: {err}");
                return;
            }
        };
        let gc = match self.conn.generate_id() {
            Ok(id) => id,
            Err(err) => {
                error!("failed to allocate menu gc id: {err}");
                return;
            }
        };
        let root = self.root;
        if let Err(err) = self
            .conn
            .create_window(
                self.depth,
                window,
                root,
                x as i16,
                y as i16,
                menu_w as u16,
                menu_h as u16,
                0,
                WindowClass::INPUT_OUTPUT,
                self.bar_visual(),
                &CreateWindowAux::new()
                    .override_redirect(1u32)
                    .background_pixmap(Pixmap::from(0u32))
                    .event_mask(
                        EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::POINTER_MOTION,
                    ),
            )
            .and_then(|_| self.conn.create_gc(gc, window, &CreateGCAux::new()))
            .and_then(|_| self.conn.map_window(window))
            .and_then(|_| {
                self.conn.configure_window(
                    window,
                    &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                )
            })
        {
            error!("failed to create menu window: {err}");
            return;
        }
        // Grab the pointer so clicks outside the menu still reach us: the grab
        // routes every pointer event to the menu window with menu-local coords.
        if let Err(err) = self.conn.grab_pointer(
            false,
            window,
            EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
            Window::from(0u32),
            XCursor::from(0u32),
            0u32,
        ) {
            error!("failed to grab pointer for menu: {err}");
            let _ = self.conn.destroy_window(window);
            let _ = self.conn.flush();
            return;
        }
        let _ = self.conn.flush();
        self.menu = Some(XMenu {
            window,
            gc,
            width: menu_w,
            height: menu_h,
            request,
            hovered: None,
        });
        if let Err(err) = self.draw_menu() {
            error!("failed to paint menu: {err}");
        }
    }

    /// The bar and the menu share the bar window's visual, so their pixel
    /// layouts are identical.
    fn bar_visual(&self) -> u32 {
        self.visual
    }

    fn draw_menu(&mut self) -> Result<()> {
        let Some(menu) = &mut self.menu else {
            return Ok(());
        };
        let stride = menu.width * 4;
        let mut pixels = vec![0u8; stride as usize * menu.height as usize];
        let colors = self.bar.reload.config().colors;
        render_menu(
            &mut pixels,
            stride,
            menu.width,
            menu.height,
            colors,
            &menu.request.items,
            menu.hovered,
        );
        upload(
            &self.conn,
            menu.window,
            menu.gc,
            menu.width,
            menu.height,
            self.depth,
            &pixels,
        )
    }

    fn handle_event(&mut self, event: XEvent) -> Result<()> {
        match event {
            XEvent::Expose(window) => {
                if window == self.bar_window {
                    self.draw_bar()?;
                } else if self.menu.as_ref().is_some_and(|m| m.window == window) {
                    self.draw_menu()?;
                }
            }
            XEvent::ButtonPress {
                window,
                button,
                x,
                y,
            } => {
                if let Some(menu) = &self.menu
                    && window == menu.window
                {
                    // Grabbed pointer events can fall outside the menu entirely.
                    if x < 0 || y < 0 || x as u32 >= menu.width || y as u32 >= menu.height {
                        self.close_menu();
                    } else if button == 1 {
                        let id = menu_hit_test(&menu.request.items, f64::from(y));
                        let request = &menu.request;
                        if let Some(id) = id
                            && let Some(tray) = &self.bar.tray
                        {
                            tray.click_menu_item(&request.owner_id, &request.menu_path, id);
                        }
                        self.close_menu();
                    }
                    return Ok(());
                }
                if window != self.bar_window {
                    return Ok(());
                }
                let x = f64::from(x);
                match button {
                    4 => {
                        if self.bar.axis(-1, x) {
                            self.draw_bar()?;
                        }
                    }
                    5 => {
                        if self.bar.axis(1, x) {
                            self.draw_bar()?;
                        }
                    }
                    1 | 3 => {
                        let which = if button == 1 {
                            PointerButton::Left
                        } else {
                            PointerButton::Right
                        };
                        let menu_open = self.menu.is_some();
                        if menu_open {
                            self.close_menu();
                            self.draw_bar()?;
                            return Ok(());
                        }
                        let (redraw, request) = self.bar.press(which, x, 0, false);
                        if let Some(request) = request {
                            self.open_menu(request);
                        }
                        if redraw {
                            self.draw_bar()?;
                        }
                    }
                    _ => {}
                }
            }
            XEvent::Motion { window, x, y } => {
                let menu_hover_changed = if let Some(menu) = &mut self.menu
                    && window == menu.window
                {
                    let hovered = if y < 0 || x < 0 || x as u32 >= menu.width {
                        None
                    } else {
                        let row = y as u32 / crate::menu_popup::MENU_ROW_HEIGHT;
                        ((row as usize) < menu.request.items.len()).then_some(row as usize)
                    };
                    if menu.hovered != hovered {
                        menu.hovered = hovered;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if menu_hover_changed {
                    self.draw_menu()?;
                    return Ok(());
                }
                if window == self.bar_window {
                    let hover = self.bar.motion(f64::from(x), self.menu.is_some());
                    if hover.redraw {
                        self.draw_bar()?;
                    }
                }
            }
            XEvent::Leave(window) => {
                if window == self.bar_window && self.bar.leave() {
                    self.draw_bar()?;
                }
            }
            XEvent::DesktopChanged => {
                let state = desktop_state(&self.conn, self.root, &self.atoms);
                self.bar.workspaces.set_state(state);
                self.draw_bar()?;
            }
            XEvent::Destroy(window) => {
                if window == self.bar_window {
                    self.exit = true;
                } else if self.menu.as_ref().is_some_and(|m| m.window == window) {
                    self.close_menu();
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ewmh_desktops_map_to_one_based_slots() {
        let state = desktop_state_from(0, 4);
        assert_eq!(state.active, 1);
        assert_eq!(state.existing, vec![1, 2, 3, 4]);
        let state = desktop_state_from(2, 4);
        assert_eq!(state.active, 3);
    }

    #[test]
    fn a_wm_reporting_no_desktops_still_gets_one_slot() {
        let state = desktop_state_from(0, 0);
        assert_eq!(state.active, 1);
        assert!(state.existing.is_empty());
    }

    #[test]
    fn more_than_ten_desktops_are_clipped_to_the_widget() {
        let state = desktop_state_from(11, 20);
        assert_eq!(state.active, 10);
        assert_eq!(state.existing.len(), 10);
    }
}

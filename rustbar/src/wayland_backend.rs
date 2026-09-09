use anyhow::{Context, Result, anyhow, bail};
use calloop::EventLoop;
use calloop::timer::{TimeoutAction, Timer};
use calloop_wayland_source::WaylandSource;
use crate::theme::SIDE_MARGIN;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        pointer::{CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemedPointer},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
        xdg::{XdgShell, popup::{Popup, PopupHandler, PopupConfigure}},
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
};

use crate::clock::Clock;
use crate::cava::Cava;
use crate::hypr::{self, HyprEvent};
use crate::menu_popup::MenuPopup;
use crate::tray::{self};
use crate::tray_render::TrayRenderer;
use crate::widgets::{SegKind, WidgetStrip};
use crate::workspaces::WorkspaceList;

/// Volume scroll step in percent. Matches waybar `pulseaudio`'s default
/// `scroll-step` (the user's config has no override); volume is capped at 100%
/// via `wpctl set-volume -l 1.0 …` (waybar `max-volume: 100`). Adjust to taste.
const VOL_SCROLL_STEP: u8 = 1;
/// Brightness scroll step in percent. Matches waybar `backlight`'s default
/// `scroll-step` (the user's config has no override). Adjust to taste.
const BRIGHT_SCROLL_STEP: u8 = 1;

/// Launch an external command detached so it never blocks the bar — mirrors
/// waybar's `on-click`/`on-scroll` execution. A short-lived reaper thread
/// waits on the child so we don't accumulate zombies (matters for scroll-repeat
/// firing `wpctl`/`brightnessctl` many times per second).
fn spawn(cmd: &str, args: &[&str]) {
    use std::process::Stdio;
    match std::process::Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(err) => log::error!("failed to spawn {cmd}: {err}"),
    }
}

/// Run a command to completion before returning, so a follow-up re-read sees
/// the applied change. Used for the bar's own `set`-style actions (volume,
/// brightness, DND toggle) which are re-read immediately after — unlike
/// `spawn`, which is fire-and-forget for GUI launchers. These commands are
/// fast (a few ms of dbus/sysfs round-trips), matching waybar's synchronous
/// on-scroll/on-click behavior; `.status()` also reaps the child, so no
/// reaper thread is needed and rapid scroll can't accumulate zombies.
fn run_sync(cmd: &str, args: &[&str]) {
    use std::process::Stdio;
    if let Err(err) = std::process::Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        log::error!("failed to run {cmd}: {err}");
    }
}

pub fn run(monitor: Option<String>, bar_height: u32, mut clock: Box<Clock>) -> Result<()> {
    let conn = Connection::connect_to_env().context("failed to connect to Wayland display")?;
    let (globals, mut event_queue) = registry_queue_init::<App>(&conn)
        .context("failed to enumerate Wayland globals")?;
    let qh = event_queue.handle();

    let compositor_state =
        CompositorState::bind(&globals, &qh).context("wl_compositor not available")?;
    let layer_shell =
        LayerShell::bind(&globals, &qh).context("wlr-layer-shell not available")?;
    let xdg_shell =
        XdgShell::bind(&globals, &qh).context("xdg_wm_base not available")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm not available")?;
    // SeatState::new (below, in App) already binds wl_seat via registry handling —
    // new_capability fires once the compositor advertises Pointer.

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        compositor_state,
        layer_shell,
        xdg_shell,
        shm,
        layer: None,
        pool: None,
        width: 0,
        height: bar_height,
        configured: false,
        exit: false,
        pointer: None,
        themed_pointer: None,
        seat: None,
        clock: {
            clock.init();
            clock
        },
        workspaces: {
            let mut w = WorkspaceList::new();
            w.init();
            Box::new(w)
        },
        widgets: {
            let mut w = WidgetStrip::new();
            w.init();
            Box::new(w)
        },
        cava: {
            let mut c = Cava::new();
            c.init();
            Box::new(c)
        },
        tray: None,
        menu: None,
        scroll_accum: 0.0,
    };

    event_queue.roundtrip(&mut app).context("initial roundtrip failed")?;
    event_queue.roundtrip(&mut app).context("output info roundtrip failed")?;

    let outputs: Vec<_> = app.output_state.outputs().collect();
    if outputs.is_empty() {
        bail!("no Wayland outputs found");
    }

    let target = match &monitor {
        Some(name) => outputs
            .iter()
            .find(|o| {
                app.output_state
                    .info(o)
                    .and_then(|info| info.name)
                    .as_deref()
                    == Some(name.as_str())
            })
            .cloned()
            .ok_or_else(|| anyhow!("monitor '{name}' not found"))?,
        None => outputs.first().cloned().expect("checked non-empty above"),
    };

    let surface = app.compositor_state.create_surface(&qh);
    let layer = app.layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Top,
        Some("rustbar"),
        Some(&target),
    );
    layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
    // waybar uses margin-top: 10 — a gap between the bar and the screen top.
    layer.set_margin(crate::theme::TOP_MARGIN, 0, 0, 0);
    // Hyprland/wlroots adds the surface margin to the exclusive zone when
    // computing the reserved area, so we set the zone to just the bar height
    // (NOT height + margin) — otherwise the margin gets double-counted and a
    // gap appears below the bar.
    layer.set_exclusive_zone(bar_height as i32);
    layer.set_size(0, bar_height);
    layer.commit();

    app.pool = Some(SlotPool::new(4096 * bar_height as usize * 4, &app.shm)
        .context("failed to create shm pool")?);
    app.layer = Some(layer);

    let mut event_loop: EventLoop<App> = EventLoop::try_new().context("failed to create event loop")?;
    let loop_handle = event_loop.handle();

    while !app.configured {
        event_queue.blocking_dispatch(&mut app)?;
        if app.exit {
            return Ok(());
        }
    }

    WaylandSource::new(conn, event_queue)
        .insert(loop_handle.clone())
        .map_err(|e| anyhow!("failed to insert Wayland source: {e}"))?;

    let qh_for_timer = qh.clone();
    let millis_into_second = chrono::Local::now().timestamp_subsec_millis() as u64;
    let until_next_second = std::time::Duration::from_millis(1000 - millis_into_second.min(999));
    loop_handle
        .insert_source(
            Timer::from_duration(until_next_second),
            move |_deadline, _, app: &mut App| {
                app.draw(&qh_for_timer);
                TimeoutAction::ToDuration(std::time::Duration::from_secs(1))
            },
        )
        .map_err(|e| anyhow!("failed to insert render timer: {e}"))?;

    let (hypr_tx, hypr_rx) = calloop::channel::channel::<HyprEvent>();
    hypr::spawn(hypr_tx);
    let qh_for_hypr = qh.clone();
    loop_handle
        .insert_source(hypr_rx, move |event, _, app: &mut App| {
            if let calloop::channel::Event::Msg(HyprEvent::WorkspaceChanged) = event {
                app.draw(&qh_for_hypr);
            }
        })
        .map_err(|e| anyhow!("failed to insert hyprland event source: {e}"))?;

    let (tray_tx, tray_rx) = calloop::channel::channel::<()>();
    let (tray_icons, tray_cmd_tx) = tray::spawn(tray_tx);
    app.tray = Some(crate::tray_render::TrayRenderer::new(tray_icons, tray_cmd_tx));
    let qh_for_tray = qh.clone();
    loop_handle
        .insert_source(tray_rx, move |event, _, app: &mut App| {
            if let calloop::channel::Event::Msg(()) = event {
                app.draw(&qh_for_tray);
            }
        })
        .map_err(|e| anyhow!("failed to insert tray event source: {e}"))?;

    // cava visualizer — a frame arrives ~120×/sec; each one triggers a redraw.
    let (cava_tx, cava_rx) = calloop::channel::channel::<()>();
    let qh_for_cava = qh.clone();
    loop_handle
        .insert_source(cava_rx, move |event, _, app: &mut App| {
            if let calloop::channel::Event::Msg(()) = event {
                app.draw(&qh_for_cava);
            }
        })
        .map_err(|e| anyhow!("failed to insert cava event source: {e}"))?;
    app.cava.spawn(cava_tx);

    loop {
        event_loop.dispatch(None, &mut app)?;
        if app.exit {
            break;
        }
    }

    Ok(())
}

pub(crate) struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    compositor_state: CompositorState,
    layer_shell: LayerShell,
    xdg_shell: XdgShell,
    shm: Shm,
    layer: Option<LayerSurface>,
    pool: Option<SlotPool>,
    width: u32,
    height: u32,
    configured: bool,
    exit: bool,
    pointer: Option<wl_pointer::WlPointer>,
    themed_pointer: Option<ThemedPointer>,
    /// The wl_seat — kept so the tray menu popup can grab it on right-click.
    seat: Option<wl_seat::WlSeat>,
    clock: Box<Clock>,
    workspaces: Box<WorkspaceList>,
    widgets: Box<WidgetStrip>,
    cava: Box<Cava>,
    tray: Option<TrayRenderer>,
    menu: Option<MenuPopup>,
    /// Leftover sub-step touchpad scroll distance (in the same units as
    /// `Axis::vertical.absolute`, ~15px per logical step) carried over
    /// between events. Touchpads emit many small continuous `Axis` events
    /// per physical scroll (a few px each) instead of one discrete
    /// notch-per-event like a wheel, so truncating `absolute / 15` on every
    /// single event rounds almost all of them down to 0 steps and drops the
    /// remainder — nothing happens unless one event's delta alone exceeds
    /// 15px, i.e. you have to flick the touchpad hard. Accumulating here and
    /// consuming whole steps out of the running total fixes that.
    scroll_accum: f64,
}

impl App {
    fn draw(&mut self, _qh: &QueueHandle<Self>) {
        let (width, height) = (self.width, self.height);
        if width == 0 || height == 0 {
            return;
        }
        let stride = width as i32 * 4;
        let layer = self.layer.as_ref().expect("draw called before layer surface exists");
        let pool = self.pool.as_mut().expect("draw called before pool exists");

        let (buffer, canvas) = match pool.create_buffer(width as i32, height as i32, stride, wl_shm::Format::Argb8888)
        {
            Ok(v) => v,
            Err(err) => {
                log::error!("failed to allocate shm buffer: {err}");
                return;
            }
        };

        for pixel in canvas.chunks_exact_mut(4) {
            pixel[0] = 0;
            pixel[1] = 0;
            pixel[2] = 0;
            pixel[3] = 0; // fully transparent — argb8888 is premultiplied, so 0 alpha means 0 everywhere
        }

        self.clock.render(canvas, SIDE_MARGIN, width - SIDE_MARGIN, height, stride as u32);
        self.workspaces.render(canvas, SIDE_MARGIN, height, stride as u32);
        // cava: waybar `modules-left = [hyprland/workspaces, cava]` — sits right
        // after workspaces with the waybar 6px gap (`#workspaces { margin: 0
        // 6px 0 0 }` right margin) between them.
        let cava_x = SIDE_MARGIN + self.workspaces.width() + 6;
        self.cava.render(canvas, cava_x, height, stride as u32);
        // Right side: the seamless widget strip is rightmost (battery right
        // cap at the inset edge); the tray is rendered to its LEFT with the
        // waybar 14px gap between them.
        let widgets_w = self.widgets.render(canvas, width - SIDE_MARGIN, height, stride as u32);
        let tray_right = width - SIDE_MARGIN - widgets_w - if widgets_w > 0 { 14 } else { 0 };
        if let Some(tray) = &mut self.tray {
            tray.render(canvas, tray_right, height, stride as u32);
        }

        layer.wl_surface().damage_buffer(0, 0, width as i32, height as i32);
        if let Err(err) = buffer.attach_to(layer.wl_surface()) {
            log::error!("failed to attach buffer: {err}");
            return;
        }
        layer.commit();
    }

    fn open_menu(
        &mut self,
        qh: &QueueHandle<Self>,
        owner_id: String,
        menu_path: String,
        items: Vec<crate::tray::MenuItem>,
        click_x: f64,
        serial: u32,
    ) {
        self.close_menu();
        // The positioner wants the absolute click x measured from the bar's
        // left edge (the parent surface's coordinate origin), not the distance
        // from the right edge.
        let click_x = click_x as i32;
        let Some(parent_layer) = self.layer.as_ref() else { return; };
        let Some(seat) = self.seat.as_ref() else { return; };
        let seat = seat.clone();
        match crate::menu_popup::MenuPopup::create(
            qh,
            &self.compositor_state,
            &self.xdg_shell,
            &self.shm,
            parent_layer,
            &seat,
            serial,
            owner_id,
            menu_path,
            items,
            click_x,
            self.height,
        ) {
            Ok(popup) => {
                // The popup's first paint happens on the initial configure
                // event from PopupHandler::configure; painting earlier can
                // trip a protocol error on strict compositors.
                self.menu = Some(popup);
            }
            Err(err) => log::error!("failed to open tray menu: {err}"),
        }
    }

    fn close_menu(&mut self) {
        self.menu = None;
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        // Both the bar and the tray menu popup are layer surfaces routed through
        // this handler. We must NOT treat the menu's surface going away as a
        // reason to tear down the whole bar — only the bar's own surface does.
        let surface = layer.wl_surface();
        if self.layer.as_ref().map(|l| l.wl_surface()) == Some(surface) {
            self.exit = true;
        } else if self.menu.as_ref().map(|m| m.wl_surface()) == Some(surface) {
            self.menu = None;
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let surface = layer.wl_surface();
        if self.layer.as_ref().map(|l| l.wl_surface()) == Some(surface) {
            // bar surface — accept the negotiated geometry and repaint.
            let (w, h) = configure.new_size;
            if w > 0 {
                self.width = w;
            }
            if h > 0 {
                self.height = h;
            }
            self.configured = true;
            self.draw(qh);
        } else if self.menu.as_ref().map(|m| m.wl_surface()) == Some(surface) {
            // tray menu popup — now an xdg_popup, not a layer surface, so this
            // branch shouldn't fire for it; left as a no-op safety net.
            let _ = self.menu.as_mut().map(|m| m.draw());
        }
    }
}

impl PopupHandler for App {
    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _popup: &Popup,
        _configure: PopupConfigure,
    ) {
        // The xdg_popup has been configured; paint it now (its size was set in
        // MenuPopup::create). We match by the popup's wl_surface, but since
        // only one menu is open at a time we can just repaint it directly.
        if let Some(menu) = &mut self.menu {
            menu.draw();
        }
    }

    fn done(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _popup: &Popup) {
        // The compositor dismissed the popup — the user clicked outside it
        // (anywhere on the desktop) or pressed Escape. Close our menu.
        self.close_menu();
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            let cursor_surface = self.compositor_state.create_surface(qh);
            let themed_pointer = self
                .seat_state
                .get_pointer_with_theme(
                    qh,
                    &seat,
                    &self.shm.wl_shm().clone(),
                    cursor_surface,
                    smithay_client_toolkit::seat::pointer::ThemeSpec::System,
                )
                .expect("failed to bind themed pointer");
            self.pointer = Some(themed_pointer.pointer().clone());
            self.themed_pointer = Some(themed_pointer);
            self.seat = Some(seat);
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            self.pointer = None;
            self.themed_pointer = None;
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let on_bar = self.layer.as_ref().map_or(false, |l| l.wl_surface() == &event.surface);
            let on_menu = self.menu.as_ref().map_or(false, |m| m.wl_surface() == &event.surface);

            if on_menu {
                match event.kind {
                    PointerEventKind::Motion { .. } => {
                        let (_x, y) = event.position;
                        if let Some(menu) = &mut self.menu {
                            menu.set_hover(y);
                            menu.draw();
                        }
                        if let Some(tp) = &self.themed_pointer {
                            let _ = tp.set_cursor(conn, CursorIcon::Pointer);
                        }
                    }
                    PointerEventKind::Press { button, .. } if button == 0x110 => {
                        let (_x, y) = event.position;
                        if let Some(menu) = &self.menu {
                            if let Some(item_id) = menu.hit_test(y) {
                                if let Some(tray) = &self.tray {
                                    tray.click_menu_item(&menu.owner_id, &menu.menu_path, item_id);
                                }
                            }
                        }
                        self.close_menu();
                    }
                    PointerEventKind::Leave { .. } => {
                        self.close_menu();
                    }
                    _ => {}
                }
                continue;
            }

            if !on_bar {
                continue;
            }

            match event.kind {
                PointerEventKind::Enter { .. } => {
                    if let Some(tp) = &self.themed_pointer {
                        let _ = tp.set_cursor(conn, CursorIcon::Default);
                    }
                }
                PointerEventKind::Motion { .. } => {
                    let (x, _y) = event.position;
                    // While the tray menu is open it captures the pointer: bar
                    // motion doesn't change workspace hover (the click is
                    // supposed to go to the menu, not the bar underneath).
                    let hover_changed = if self.menu.is_none() {
                        self.workspaces.update_hover(x)
                    } else {
                        false
                    };
                    // Show the pointer hand over anything clickable/scrollable:
                    // workspaces, cava, tray icons, and the actionable widget
                    // segments (volume/notification/brightness).
                    let widget_hover = matches!(
                        self.widgets.hit_test(x),
                        Some(SegKind::Volume) | Some(SegKind::Notification) | Some(SegKind::Brightness)
                    );
                    let hovering = self.workspaces.hit_test(x).is_some()
                        || self.cava.hit_test(x)
                        || widget_hover
                        || self.tray.as_ref().map_or(false, |t| t.hit_test(x).is_some());
                    if let Some(tp) = &self.themed_pointer {
                        let icon = if hovering { CursorIcon::Pointer } else { CursorIcon::Default };
                        let _ = tp.set_cursor(conn, icon);
                    }
                    if hover_changed {
                        self.draw(qh);
                    }
                }
                PointerEventKind::Press { button, serial, .. } => {
                    let (x, _y) = event.position;
                    // While the tray menu is open it "takes" the click: any
                    // press on the bar just closes the menu and consumes the
                    // event — you can't click workspaces/tray icons (or open
                    // another menu) until the current menu is dismissed.
                    if self.menu.is_some() {
                        self.close_menu();
                        continue;
                    }
                    if button == 0x110 {
                        // BTN_LEFT
                        if let Some(id) = self.workspaces.hit_test(x) {
                            self.close_menu();
                            self.workspaces.switch_to(id);
                            self.draw(qh);
                        } else if let Some(item_id) =
                            self.tray.as_ref().and_then(|t| t.hit_test(x).map(|s| s.to_string()))
                        {
                            self.close_menu();
                            if let Some(tray) = &self.tray {
                                tray.activate(&item_id);
                            }
                        } else if let Some(seg) = self.widgets.hit_test(x) {
                            self.close_menu();
                            match seg {
                                // waybar pulseaudio `on-click: pavucontrol`
                                SegKind::Volume => spawn("pavucontrol", &[]),
                                // waybar custom/notification `on-click: swaync -t`
                                SegKind::Notification => spawn("swaync-client", &["-t"]),
                                SegKind::Brightness | SegKind::Other => {}
                            }
                        } else {
                            self.close_menu();
                        }
                    } else if button == 0x111 {
                        // BTN_RIGHT — toggle the tray icon's context menu.
                        // First click opens it (grabbed on the press serial);
                        // a second RMB (while it's open) just closes it.
                        if self.menu.is_some() {
                            self.close_menu();
                        } else if self.cava.hit_test(x) {
                            // waybar `cava` `on-click-right: mode` → pause/resume.
                            self.cava.toggle_pause();
                        } else if self.widgets.hit_test(x) == Some(SegKind::Notification) {
                            // waybar custom/notification `on-click-right: swaync -d`
                            // → toggle Do-Not-Disturb. Run synchronously so the
                            // bell icon flips immediately (the periodic 1s refresh
                            // would otherwise lag by up to a second).
                            run_sync("swaync-client", &["-d"]);
                            self.widgets.refresh_notification();
                            self.draw(qh);
                        } else if let Some(tray) = &self.tray {
                            if let Some(item_id) = tray.hit_test(x).map(|s| s.to_string()) {
                                let (menu_path, menu_items) = tray.menu_for(&item_id);
                                if let Some(menu_path) = menu_path {
                                    if !menu_items.is_empty() {
                                        self.open_menu(qh, item_id, menu_path, menu_items, x, serial);
                                    }
                                }
                            }
                        }
                    }
                }
                PointerEventKind::Axis { vertical, .. } => {
                    let (x, _y) = event.position;
                    if self.menu.is_some() {
                        continue;
                    }
                    // Vertical scroll only (matches a normal mouse wheel; we
                    // ignore horizontal pans so they don't nudge volume).
                    // One logical step per 120 (high-res wheel); fall back to
                    // the deprecated `discrete` field, then to the continuous
                    // `absolute` magnitude for touchpads (~15px ≈ one notch).
                    let steps: i32 = if vertical.value120 != 0 {
                        // High-res mouse wheel: one whole logical step per
                        // event already, nothing to accumulate.
                        vertical.value120 / 120
                    } else if vertical.discrete != 0 {
                        // Legacy wheel: same, one discrete notch per event.
                        vertical.discrete
                    } else {
                        // Touchpad (continuous `absolute`, no discrete
                        // notches): a single physical scroll fires many
                        // events of a few px each. Add this event's delta to
                        // the running total and only emit whole ~15px steps,
                        // keeping the fractional remainder for the next
                        // event instead of dropping it. Without this,
                        // truncating per-event rounds almost every touchpad
                        // event down to 0 steps, so light scrolling does
                        // nothing and only a hard flick (>15px in one event)
                        // registers.
                        self.scroll_accum += vertical.absolute;
                        let s = (self.scroll_accum / 15.0) as i32;
                        self.scroll_accum -= s as f64 * 15.0;
                        s
                    };
                    if steps == 0 {
                        continue;
                    }
                    let n = steps.unsigned_abs();
                    // Wayland vertical-axis sign convention: the value is a
                    // motion vector in surface coords (positive = down). A
                    // traditional mouse wheel rolled UP produces a NEGATIVE
                    // value (content moves up), so wheel-up → increase means
                    // `up = steps < 0`. (This is why `steps > 0` was inverted.)
                    let up = steps < 0;
                    match self.widgets.hit_test(x) {
                        Some(SegKind::Volume) => {
                            // waybar pulseaudio default scroll: volume ±step,
                            // capped at 100% (`max-volume: 100`) via `-l 1.0`.
                            let arg = if up {
                                format!("{VOL_SCROLL_STEP}%+")
                            } else {
                                format!("{VOL_SCROLL_STEP}%-")
                            };
                            for _ in 0..n {
                                run_sync(
                                    "wpctl",
                                    &["set-volume", "-l", "1.0", "@DEFAULT_AUDIO_SINK@", arg.as_str()],
                                );
                            }
                            // The set ran to completion above, so this re-read
                            // sees the new level (a detached `spawn` would race
                            // the read and show the old value until the next
                            // periodic refresh). Patch only the volume segment so
                            // the rest of the strip (CPU/temp/mem/…) keeps its
                            // cached values instead of re-sampling on every
                            // scroll tick.
                            self.widgets.refresh_volume();
                            self.draw(qh);
                        }
                        Some(SegKind::Brightness) => {
                            // waybar backlight default scroll: brightness ±step.
                            let arg = if up {
                                format!("+{BRIGHT_SCROLL_STEP}%")
                            } else {
                                format!("{BRIGHT_SCROLL_STEP}%-")
                            };
                            for _ in 0..n {
                                run_sync("brightnessctl", &["set", arg.as_str()]);
                            }
                            // `run_sync` waited for the set to apply, so this
                            // re-read reflects the new brightness immediately (a
                            // detached spawn would race it).
                            self.widgets.refresh_brightness();
                            self.draw(qh);
                        }
                        _ => {}
                    }
                }
                PointerEventKind::Leave { .. } => {
                    if self.workspaces.clear_hover() {
                        self.draw(qh);
                    }
                }
                _ => {}
            }
        }
    }
}


impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_layer!(App);
delegate_seat!(App);
delegate_pointer!(App);
delegate_registry!(App);
smithay_client_toolkit::delegate_xdg_popup!(App);

// Manual dispatch for xdg_wm_base (ping/pong) and the decoration manager —
// we use only xdg_popup, not toplevel windows, so we avoid
// `delegate_xdg_shell!` which would pull in WindowHandler/toplevel-decoration
// requirements. Both are simple no-ops for our use.
use smithay_client_toolkit::globals::GlobalData;
use wayland_protocols::xdg::{
    decoration::zv1::client::zxdg_decoration_manager_v1::{
        self, ZxdgDecorationManagerV1,
    },
    shell::client::xdg_wm_base::{self, XdgWmBase},
};
impl Dispatch<XdgWmBase, GlobalData> for App {
    fn event(
        state: &mut Self,
        _proxy: &XdgWmBase,
        event: xdg_wm_base::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            _proxy.pong(serial);
        }
        let _ = state;
    }
}
impl Dispatch<ZxdgDecorationManagerV1, GlobalData> for App {
    fn event(
        _state: &mut Self,
        _proxy: &ZxdgDecorationManagerV1,
        _event: zxdg_decoration_manager_v1::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

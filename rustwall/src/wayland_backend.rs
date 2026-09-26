//! All the Wayland plumbing lives here. This is the part ported from the
//! original `vecpaper` C code (monitor enumeration, `wlr-layer-shell`
//! background surface) plus the bits that were missing before: an actual
//! `wl_shm` pixel buffer, and a render loop that calls into a
//! `crate::source::FrameSource`.
//!
//! Nothing in this file knows or cares what's being rendered — that's the
//! whole point.

use anyhow::{Context, Result, anyhow, bail};
use calloop::EventLoop;
use calloop::timer::{TimeoutAction, Timer};
use calloop_wayland_source::WaylandSource;
use log::{debug, info};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, Layer, LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
        },
    },
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
};
use wayland_client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
};

use crate::config::PowerSaveConfig;
use crate::power::PowerMonitor;
use crate::source::FrameSource;

/// Claim `monitor` (or, if `None`, the last output the compositor reports —
/// matching the original vecpaper's fallback behavior) and start rendering
/// `source` into a `wlr-layer-shell` background surface on it. Blocks
/// forever (or until the layer surface is closed by the compositor).
pub fn run(
    monitor: Option<String>,
    source: Box<dyn FrameSource>,
    power_save: PowerSaveConfig,
) -> Result<()> {
    let conn = Connection::connect_to_env().context("failed to connect to Wayland display")?;
    let (globals, mut event_queue) =
        registry_queue_init::<App>(&conn).context("failed to enumerate Wayland globals")?;
    let qh = event_queue.handle();

    let compositor_state =
        CompositorState::bind(&globals, &qh).context("wl_compositor not available")?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .context("wlr-layer-shell not available (unsupported compositor?)")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm not available")?;

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        layer: None,
        pool: None,
        width: 0,
        height: 0,
        configured: false,
        exit: false,
        source,
        interval: None,
        buffer: None,
        buf_w: 0,
        buf_h: 0,
        power: PowerMonitor::new(),
        power_save,
        on_battery: false,
    };

    // Two roundtrips, same as the original C main(): the first lets the
    // registry globals arrive, the second lets each wl_output's
    // geometry/mode/name/description/done events arrive so we actually know
    // what monitors exist before picking one.
    event_queue
        .roundtrip(&mut app)
        .context("initial roundtrip failed")?;
    event_queue
        .roundtrip(&mut app)
        .context("output info roundtrip failed")?;

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
        None => {
            info!("no monitor specified, picking the last one reported");
            outputs.last().cloned().expect("checked non-empty above")
        }
    };

    let info = app
        .output_state
        .info(&target)
        .ok_or_else(|| anyhow!("no output info available for chosen monitor"))?;
    let (width, height) = info
        .modes
        .iter()
        .find(|m| m.current)
        .map(|m| (m.dimensions.0 as u32, m.dimensions.1 as u32))
        .ok_or_else(|| anyhow!("chosen monitor reported no current mode"))?;
    debug!("targeting output {:?} ({}x{})", info.name, width, height);

    let surface = compositor_state.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Background,
        Some("rustwall"),
        Some(&target),
    );
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_exclusive_zone(-1);
    layer.set_size(width, height);
    // Passthrough: the wallpaper should never intercept clicks.
    let empty_region = Region::new(&compositor_state).context("failed to create input region")?;
    layer
        .wl_surface()
        .set_input_region(Some(empty_region.wl_region()));
    drop(empty_region);
    // Initial commit with no buffer attached: this is required before the
    // compositor will send us a configure event. See WaylandSurface::commit.
    layer.commit();

    app.pool = Some(
        SlotPool::new((width * height * 4) as usize, &app.shm)
            .context("failed to create shm pool")?,
    );
    app.width = width;
    app.height = height;
    app.layer = Some(layer);

    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().context("failed to create event loop")?;
    let loop_handle = event_loop.handle();

    // Wait for the first configure event (which triggers the first draw and,
    // if the source wants to animate, schedules its timer) before handing
    // the queue over to calloop.
    while !app.configured {
        event_queue.blocking_dispatch(&mut app)?;
        if app.exit {
            return Ok(());
        }
    }

    WaylandSource::new(conn, event_queue)
        .insert(loop_handle.clone())
        .map_err(|e| anyhow!("failed to insert Wayland source into event loop: {e}"))?;

    if let Some(interval) = app.interval {
        let qh_for_timer = qh.clone();
        loop_handle
            .insert_source(
                Timer::from_duration(interval),
                move |_deadline, _, app: &mut App| {
                    app.draw(&qh_for_timer);
                    // Re-read the interval every tick rather than capturing it, so
                    // a source that changes its mind (frame-rate cap following the
                    // power state, say) is honoured from the very next frame.
                    let next = app.source.tick_interval().unwrap_or(interval);
                    TimeoutAction::ToDuration(next)
                },
            )
            .map_err(|e| anyhow!("failed to insert render timer: {e}"))?;
    }

    // No separate timer for the battery: `PowerMonitor::poll` is now a single
    // atomic load, so the render path can just check it as it goes. An earlier
    // version polled sysfs from a timer on *this* thread and a slow ACPI read
    // froze the wallpaper for 50-80ms every few seconds.

    loop {
        event_loop.dispatch(None, &mut app)?;
        if app.exit {
            break;
        }
    }

    Ok(())
}

struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    layer: Option<LayerSurface>,
    pool: Option<SlotPool>,
    width: u32,
    height: u32,
    configured: bool,
    exit: bool,
    source: Box<dyn FrameSource>,
    /// Set once, right after `init`, from `source.tick_interval()`, and then
    /// only used as the render timer's initial delay and fallback.
    interval: Option<std::time::Duration>,
    /// Battery state, refreshed by a background thread (see [`PowerMonitor`]).
    power: PowerMonitor,
    /// What to do while on battery, from the config.
    power_save: PowerSaveConfig,
    /// The state the source was last told about, so we only wake it on a
    /// change.
    on_battery: bool,
    /// The single SHM buffer we draw into, reused frame after frame. Replacing
    /// this (only on resize) drops the old one and lets the slot pool reclaim
    /// the memory. Keeping one buffer — instead of allocating a new one every
    /// tick — is what stops the `SlotPool` from growing without bound.
    buffer: Option<Buffer>,
    buf_w: u32,
    buf_h: u32,
}

impl App {
    /// Notice a change of power source and tell the source about it.
    /// Unplugging costs a throttled wallpaper; plugging back in must always
    /// restore full speed. Called once per frame, and it costs one atomic load.
    fn check_power(&mut self) {
        let on_battery = self.power_save.enabled && self.power.poll();
        if on_battery == self.on_battery {
            return;
        }
        self.on_battery = on_battery;
        log::info!(
            "power source changed: {}",
            if on_battery {
                "on battery, throttling"
            } else {
                "on mains, full speed"
            }
        );
        self.source.set_power_saving(on_battery);
    }

    fn draw(&mut self, _qh: &QueueHandle<Self>) {
        // Cheap enough to do every frame (one atomic load), and it means a
        // battery change takes effect as soon as the background thread sees it.
        self.check_power();
        let layer = match &self.layer {
            Some(l) => l,
            None => return,
        };
        let width = self.width;
        let height = self.height;
        if width == 0 || height == 0 {
            return;
        }
        let stride = width as i32 * 4;

        let pool = match self.pool.as_mut() {
            Some(p) => p,
            None => return,
        };

        // Reuse the existing SHM buffer when the surface dimensions haven't changed.
        // pool.canvas() returns None while the compositor hasn't released the
        // previous frame, which naturally throttles drawing to the compositor's
        // refresh rate and keeps the pool at a single buffer's worth of memory
        // instead of growing without bound (the original "memory leak"). We only
        // allocate a new buffer on the first frame or after a resize.
        let need_new = self.buffer.is_none() || self.buf_w != width || self.buf_h != height;

        let canvas = if need_new {
            let (buffer, canvas) = match pool.create_buffer(
                width as i32,
                height as i32,
                stride,
                wl_shm::Format::Xrgb8888,
            ) {
                Ok(v) => v,
                Err(err) => {
                    log::error!("failed to allocate shm buffer: {err}");
                    return;
                }
            };
            self.buffer = Some(buffer);
            self.buf_w = width;
            self.buf_h = height;
            canvas
        } else {
            let buffer = self.buffer.as_ref().unwrap();
            match pool.canvas(buffer) {
                Some(c) => c,
                None => {
                    return; // compositor still showing the previous frame
                }
            }
        };

        let updated = self.source.render(canvas, width, height, stride as u32);
        if !updated {
            return;
        }

        layer
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        if let Err(err) = self.buffer.as_ref().unwrap().attach_to(layer.wl_surface()) {
            log::error!("failed to attach buffer: {err}");
            return;
        }
        layer.commit();
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
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
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (w, h) = configure.new_size;
        if w > 0 && h > 0 {
            self.width = w;
            self.height = h;
        }
        if !self.configured {
            self.configured = true;
            self.source.init(self.width, self.height);
            // The first battery reading happens before the first frame, so a
            // wallpaper that boots on battery starts throttled.
            self.on_battery = self.power_save.enabled && self.power.poll();
            self.source.set_power_saving(self.on_battery);
            if self.on_battery {
                log::info!("running on battery: starting throttled");
            }
            self.interval = self.source.tick_interval();
            self.draw(qh);
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
    registry_handlers![OutputState];
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_layer!(App);
delegate_registry!(App);

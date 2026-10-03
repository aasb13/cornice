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

use crate::config::{Config, PowerSaveConfig, PowerState};
use crate::power::PowerMonitor;
use crate::source::FrameSource;
use rwconfig::Reload;

/// Index of the output to use when `monitor` is `None`.
///
/// Deterministic on purpose. `OutputState::outputs` is in registry-advertisement
/// order — whatever order the compositor enumerated its globals in — which is not
/// specified and can change across a hotplug or a compositor restart. This
/// previously picked `outputs.last()`, while `rustbar` picked `outputs.first()`,
/// so on a two-monitor machine the wallpaper and the bar could silently end up on
/// different screens. Ordering by geometry (then y, then name) picks the same
/// screen on every run and the same screen for both programs. Leftmost is the
/// conventional "main monitor"; it is a heuristic, not a primary-output query,
/// which core Wayland does not have.
///
/// If the compositor withholds geometry — `OutputInfo::location` is documented as
/// possibly always `(0, 0)` — every output ties and the name breaks it, which is
/// still deterministic.
fn leftmost(outputs: &[(Option<String>, (i32, i32))]) -> Option<usize> {
    outputs
        .iter()
        .enumerate()
        // `is_none` sorts false first, so a named output beats an unnamed one at
        // the same position: it is the one `--monitor` can refer to.
        .min_by_key(|(_, (name, (x, y)))| (*x, *y, name.is_none(), name.clone()))
        .map(|(i, _)| i)
}

/// Every output name, for an error message that says what *was* available.
fn names(outputs: &[(Option<String>, (i32, i32))]) -> Vec<&str> {
    outputs
        .iter()
        .map(|(n, _)| n.as_deref().unwrap_or("(unnamed)"))
        .collect()
}

/// Claim `monitor` (or, if `None`, the leftmost output — see [`leftmost`]) and
/// start rendering `source` into a `wlr-layer-shell` background surface on it.
/// Blocks forever (or until the layer surface is closed by the compositor).
pub fn run(
    monitor: Option<String>,
    source: Box<dyn FrameSource>,
    reload: Reload<Config>,
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
        battery: PowerMonitor::new(),
        power: PowerState::default(),
        reload,
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

    // (name, topleft) for each output, for ordering and for matching --monitor.
    let described: Vec<(Option<String>, (i32, i32))> = outputs
        .iter()
        .map(|o| match app.output_state.info(o) {
            Some(info) => (info.name, info.location),
            // No info yet: treat it as unnamed at the origin rather than dropping
            // it, so a compositor that hasn't described an output doesn't make it
            // invisible.
            None => (None, (0, 0)),
        })
        .collect();

    let chosen = match &monitor {
        Some(name) => described
            .iter()
            .position(|(n, _)| n.as_deref() == Some(name.as_str()))
            .ok_or_else(|| anyhow!("monitor '{name}' not found among {:?}", names(&described)))?,
        None => {
            let i = leftmost(&described).expect("checked non-empty above");
            info!(
                "no monitor specified, using {:?}",
                described[i].0.as_deref().unwrap_or("(unnamed)")
            );
            i
        }
    };
    let target = outputs[chosen].clone();

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

    // `--reload` has to wake a loop that is otherwise blocked in `dispatch`
    // waiting on vsync, which is what the signal source buys: a polled flag
    // would only be noticed at the next frame, and if the source were ever
    // static there would be no next frame at all.
    #[cfg(unix)]
    loop_handle
        .insert_source(
            calloop::signals::Signals::new(&[calloop::signals::Signal::SIGHUP])
                .context("failed to create SIGHUP source")?,
            |_, _, app: &mut App| {
                app.force_reload();
            },
        )
        .map_err(|e| anyhow!("failed to insert SIGHUP source: {e}"))?;

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
    battery: PowerMonitor,
    /// What the source currently believes about the power budget — not the
    /// machine's actual battery state, which is re-read from `battery` every
    /// time it matters. Keeping the two apart is what lets a config edit change
    /// the answer without the battery itself having changed.
    power: PowerState,
    /// The config in force, plus the file watcher and the manual `SIGHUP` path.
    /// Owns `[power_save]` too, so there is one place a reload writes it.
    reload: Reload<Config>,
    /// The single SHM buffer we draw into, reused frame after frame. Replacing
    /// this (only on resize) drops the old one and lets the slot pool reclaim
    /// the memory. Keeping one buffer — instead of allocating a new one every
    /// tick — is what stops the `SlotPool` from growing without bound.
    buffer: Option<Buffer>,
    buf_w: u32,
    buf_h: u32,
}

/// The power state to push to the source, or `None` if it already has it.
///
/// Split out from [`App::check_power`] so the transition table is testable
/// without a Wayland connection. Comparing the whole [`PowerState`] rather than
/// just the on/off half is what lets a `[power_save]` edit reach the source while
/// the machine is still unplugged: with a bool as the key, retuning the battery
/// frame rate produced the same key and so nothing was sent.
fn next_power_state(
    last: PowerState,
    power_save: &PowerSaveConfig,
    discharging: bool,
) -> Option<PowerState> {
    let now = power_save.resolve(discharging);
    (now != last).then_some(now)
}

impl App {
    /// Re-read the config if it changed on disk. `Reload::poll` returns `None`
    /// both when nothing changed and when the file is broken — the latter is
    /// logged inside, which is the whole point of doing this live rather than at
    /// startup: being mid-edit and saving a half-finished file must not blank the
    /// wallpaper or kill the process.
    fn check_reload(&mut self) {
        if self.reload.poll().is_some() {
            self.apply_config("config file changed");
        }
    }

    /// Re-read and apply the config, unconditionally. The `SIGHUP` source uses
    /// this, since it has to work whether or not `[reload] watch` is on and
    /// whether or not the file looks changed.
    fn force_reload(&mut self) {
        if self.reload.force().is_some() {
            self.apply_config("SIGHUP");
        }
    }

    /// Adopt the config `Reload` just installed.
    ///
    /// `Reload` has already read, validated and swapped it in by this point — the
    /// power-save table, the reload settings and the source's own settings all
    /// come out of the same new value, so they cannot disagree about which
    /// config is in force.
    fn apply_config(&mut self, reason: &str) {
        info!("applying config ({reason})");
        // The battery state may have changed *meaning*: `enabled` could have been
        // turned off, or the new cap applied while unplugged. `check_power`
        // recomputes from the new config and notifies the source if the
        // effective answer moved. The new cap itself reaches the source through
        // `reconfigure`, not through here — `set_power_saving` only carries the
        // on/off half.
        self.check_power();
        let (w, h) = (self.width, self.height);
        self.source.reconfigure(self.reload.config(), w, h);
        info!("config reloaded");
    }

    /// Notice a change of power source and tell the source about it.
    /// Unplugging costs a throttled wallpaper; plugging back in must always
    /// restore full speed. Called once per frame, and it costs one atomic load.
    fn check_power(&mut self) {
        let discharging = self.battery.poll();
        let Some(now) = next_power_state(self.power, &self.reload.config().power_save, discharging)
        else {
            return;
        };
        self.power = now;
        log::info!(
            "power state: {}",
            if now.throttling {
                "on battery, throttling"
            } else {
                "on mains, full speed"
            }
        );
        self.source.set_power_saving(now);
    }

    fn draw(&mut self, _qh: &QueueHandle<Self>) {
        // Cheap enough to do every frame (one atomic load), and it means a
        // battery change takes effect as soon as the background thread sees it.
        self.check_power();
        self.check_reload();
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
            // wallpaper that boots on battery starts throttled. There is no
            // prior belief to compare against, so the source is told either way.
            let discharging = self.battery.poll();
            let power = self.reload.config().power_save.resolve(discharging);
            self.power = power;
            self.source.set_power_saving(power);
            if power.throttling {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn power_save(enabled: bool, fps_cap: u32) -> PowerSaveConfig {
        PowerSaveConfig {
            enabled,
            fps_cap,
            ..Default::default()
        }
    }

    /// The source only hears about the power budget when the answer actually
    /// changes, so this table is the whole notification contract. Each row is
    /// (what the source currently believes, `enabled`, the battery cap, whether a
    /// battery is discharging, and what the source must be told).
    ///
    /// The last two rows are the bugs this exists to pin down, both cases where
    /// the *battery* never moves but the answer does:
    ///
    /// * `enabled` turned off while unplugged. A notification keyed only on
    ///   battery transitions never fires, and the wallpaper stays throttled
    ///   forever. An earlier version forced this by clearing the remembered
    ///   state before re-checking, which collided with a genuine "not
    ///   throttling" and instead skipped the notification for this row.
    /// * The battery cap retuned while unplugged. With the on/off half as the
    ///   key this produced an identical key, so the new cap stayed invisible —
    ///   it only worked because the source happened to keep its own copy of the
    ///   config. Comparing the whole resolved state is what makes it reach the
    ///   source on its own.
    #[test]
    fn the_source_is_told_exactly_when_the_budget_moves() {
        // `false` = "not throttling", which is what `PowerState::default` is.
        let idle = PowerState::default();
        let throttled_at = |cap| PowerState {
            throttling: true,
            fps_cap: cap,
            density_divisor: 4,
        };
        let cases: &[(PowerState, bool, u32, bool, Option<PowerState>)] = &[
            // Idle on mains: nothing to say, ever.
            (idle, true, 30, false, None),
            // Unplugged from a full-speed start: throttle.
            (idle, true, 30, true, Some(throttled_at(30))),
            // Plugged back in: always restore full speed.
            (throttled_at(30), true, 30, false, Some(idle)),
            // Disabled while unplugged: the battery is unchanged, but the answer
            // is not — the source must be told to stop throttling.
            (throttled_at(30), false, 30, true, Some(idle)),
            // Retuned while still unplugged: same battery, different budget.
            (throttled_at(30), true, 15, true, Some(throttled_at(15))),
            // Still throttled on the same budget: no change.
            (throttled_at(30), true, 30, true, None),
        ];
        for &(last, enabled, cap, discharging, expected) in cases {
            let got = next_power_state(last, &power_save(enabled, cap), discharging);
            assert_eq!(
                got, expected,
                "last={last:?} enabled={enabled} cap={cap} discharging={discharging}"
            );
        }
    }

    /// The whole point of the rule: a stable answer, and the leftmost screen, so
    /// the wallpaper lands where the bar does. Reordering the inputs must not
    /// change *which output* is chosen — that is the property registry order
    /// fails to give, and comparing names rather than indices is what proves it.
    #[test]
    fn the_default_output_is_the_leftmost_and_does_not_depend_on_order() {
        let names_and_geometry = [("DP-2", (1920, 0)), ("eDP-1", (0, 0)), ("DP-1", (0, 1080))];
        let chosen = |order: &[usize]| {
            let outputs: Vec<_> = order
                .iter()
                .map(|&i| {
                    let (name, at) = names_and_geometry[i];
                    (Some(name.to_string()), at)
                })
                .collect();
            let i = leftmost(&outputs).expect("non-empty");
            names_and_geometry[order[i]].0
        };

        assert_eq!(
            chosen(&[0, 1, 2]),
            "eDP-1",
            "leftmost is the one at x=0,y=0"
        );
        for order in [[2, 1, 0], [1, 2, 0], [2, 0, 1], [1, 0, 2], [0, 2, 1]] {
            assert_eq!(
                chosen(&order),
                "eDP-1",
                "reordering changed the answer: {order:?}"
            );
        }
    }

    /// Column before row, which is what makes this "leftmost" and not "topmost": a
    /// monitor stacked *below* the origin beats one off to the right, because x
    /// is compared first even though its y is much larger.
    #[test]
    fn x_is_compared_before_y() {
        let outputs = vec![
            (Some("below".to_string()), (0, 1080)),
            (Some("right".to_string()), (1920, 0)),
        ];
        assert_eq!(leftmost(&outputs), Some(0));
    }

    /// Two outputs at the same spot can't happen in practice, but the compositor
    /// is allowed to report every location as (0,0). The name has to break that
    /// tie or the choice falls back to registry order again.
    #[test]
    fn a_compositor_that_withholds_geometry_still_picks_deterministically() {
        let outputs = vec![
            (Some("DP-1".to_string()), (0, 0)),
            (Some("eDP-1".to_string()), (0, 0)),
        ];
        assert_eq!(leftmost(&outputs), Some(0), "names break the 0,0 tie");
        let mut reversed = outputs.clone();
        reversed.reverse();
        assert_eq!(leftmost(&reversed), Some(1));
    }

    /// An unnamed output still has to be usable — it's just beaten by a named one
    /// at the same position, since `--monitor` can't refer to it.
    #[test]
    fn an_unnamed_output_is_still_a_candidate() {
        assert_eq!(leftmost(&[]), None);
        assert_eq!(leftmost(&[(None, (500, 0))]), Some(0));
        let named = Some("HDMI-1".to_string());
        let outputs = vec![(None, (500, 0)), (named, (500, 0))];
        assert_eq!(leftmost(&outputs), Some(1));
    }

    /// The error has to say what was available, or the user has no way to find
    /// the right name to pass.
    #[test]
    fn the_error_lists_the_names_that_exist() {
        let outputs = vec![(Some("DP-1".to_string()), (0, 0)), (None, (1920, 0))];
        assert_eq!(names(&outputs), vec!["DP-1", "(unnamed)"]);
    }
}

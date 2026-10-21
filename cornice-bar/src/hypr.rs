//! The Hyprland event thread: turns compositor events into bar repaints.
//!
//! This thread owns every conversation with Hyprland. Both hyprland-rs's event
//! listener and its data queries are synchronous and blocking — each read opens
//! a fresh `UnixStream` to the command socket and waits for the reply — so
//! anything that needs Hyprland's state has to happen here, never on the
//! calloop event loop.

use calloop::channel::Sender;
use hyprland::data::{Workspace, Workspaces};
use hyprland::event_listener::EventListener;
use hyprland::shared::{HyprData, HyprDataActive, HyprDataVec};

use crate::workspaces::WorkspaceState;

pub enum HyprEvent {
    /// A fresh snapshot of which workspace is active and which ones exist.
    Workspaces(WorkspaceState),
    /// The keyboard layout was switched (Hyprland `activelayout` event).
    LayoutChanged,
}

/// Read the workspace list from Hyprland.
///
/// A read that fails degrades to "nothing active, nothing exists" rather than
/// propagating an error, matching what the bar drew when the same query failed
/// mid-frame: a slot with no highlight is a cosmetic loss, a dropped snapshot
/// would freeze the bar on stale data until the next event.
fn read_workspaces() -> WorkspaceState {
    let active = match Workspace::get_active() {
        Ok(w) => w.id,
        Err(err) => {
            log::warn!("hyprland: could not read the active workspace: {err}");
            -1
        }
    };
    let existing = match Workspaces::get() {
        Ok(ws) => ws.to_vec().into_iter().map(|w| w.id).collect(),
        Err(err) => {
            log::warn!("hyprland: could not read the workspace list: {err}");
            Vec::new()
        }
    };
    WorkspaceState { active, existing }
}

/// Read the workspace state here and hand it over as a value, so the bar can
/// paint a frame from cached data instead of querying per frame.
fn publish_workspaces(tx: &Sender<HyprEvent>) {
    let _ = tx.send(HyprEvent::Workspaces(read_workspaces()));
}

pub fn spawn(tx: Sender<HyprEvent>) {
    std::thread::spawn(move || {
        let mut listener = EventListener::new();

        // The bar needs a snapshot before any workspace event has fired, and the
        // event socket is only connected once `start_listener` runs — so this
        // read necessarily precedes the subscription. A workspace change landing
        // in that gap loses its event, and the bar keeps the older highlight
        // until the next switch.
        publish_workspaces(&tx);

        // Everything that can alter which workspaces exist or which one is
        // active. Each handler re-reads rather than patching the previous
        // snapshot: mirroring Hyprland's transitions here would be a second
        // source of truth to keep in step with the compositor, and these events
        // arrive at user speed, not frame speed.
        let tx_changed = tx.clone();
        listener.add_workspace_changed_handler(move |_| publish_workspaces(&tx_changed));
        let tx_added = tx.clone();
        listener.add_workspace_added_handler(move |_| publish_workspaces(&tx_added));
        let tx_deleted = tx.clone();
        listener.add_workspace_deleted_handler(move |_| publish_workspaces(&tx_deleted));
        let tx_moved = tx.clone();
        listener.add_workspace_moved_handler(move |_| publish_workspaces(&tx_moved));
        // A monitor appearing or taking its workspaces away changes the set too.
        let tx_monitor_added = tx.clone();
        listener.add_monitor_added_handler(move |_| publish_workspaces(&tx_monitor_added));
        let tx_monitor_removed = tx.clone();
        listener.add_monitor_removed_handler(move |_| publish_workspaces(&tx_monitor_removed));
        // A config reload can change workspace rules.
        let tx_reloaded = tx.clone();
        listener.add_config_reloaded_handler(move || publish_workspaces(&tx_reloaded));

        // `activelayout` fires the moment the layout is toggled, so the bar can
        // repaint the language segment immediately instead of waiting for the
        // next periodic 1s refresh (which also has to fork `hyprctl`).
        let tx_layout = tx.clone();
        listener.add_layout_changed_handler(move |_| {
            let _ = tx_layout.send(HyprEvent::LayoutChanged);
        });

        if let Err(err) = listener.start_listener() {
            log::error!("hyprland event listener died: {err}");
        }
    });
}

use calloop::channel::Sender;
use hyprland::event_listener::EventListener;

pub enum HyprEvent {
    WorkspaceChanged,
    /// The keyboard layout was switched (Hyprland `activelayout` event).
    LayoutChanged,
}

pub fn spawn(tx: Sender<HyprEvent>) {
    std::thread::spawn(move || {
        let mut listener = EventListener::new();

        let tx1 = tx.clone();
        listener.add_workspace_changed_handler(move |_| {
            let _ = tx1.send(HyprEvent::WorkspaceChanged);
        });
        let tx2 = tx.clone();
        listener.add_workspace_added_handler(move |_| {
            let _ = tx2.send(HyprEvent::WorkspaceChanged);
        });
        // `activelayout` fires the moment the layout is toggled, so the bar can
        // repaint the language segment immediately instead of waiting for the
        // next periodic 1s refresh (which also has to fork `hyprctl`).
        let tx3 = tx.clone();
        listener.add_layout_changed_handler(move |_| {
            let _ = tx3.send(HyprEvent::LayoutChanged);
        });

        if let Err(err) = listener.start_listener() {
            log::error!("hyprland event listener died: {err}");
        }
    });
}

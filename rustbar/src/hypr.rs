use calloop::channel::Sender;
use hyprland::event_listener::EventListener;

pub enum HyprEvent {
    WorkspaceChanged,
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

        if let Err(err) = listener.start_listener() {
            log::error!("hyprland event listener died: {err}");
        }
    });
}

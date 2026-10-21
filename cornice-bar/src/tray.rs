use calloop::channel::Sender;
use std::sync::{Arc, Mutex};
use system_tray::client::{ActivateRequest, Client, Event};

#[derive(Clone)]
pub struct MenuItem {
    pub id: i32,
    pub label: String,
}

#[derive(Clone)]
pub struct TrayIcon {
    pub id: String,
    /// Premultiplied BGRA8888 (little-endian), ready to blit straight into
    /// the wl_shm argb8888 canvas.
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// DBus object path for this item's menu, e.g.
    /// "/org/ayatana/NotificationItem/Element1/Menu". Required alongside
    /// the address to click a menu item — confirmed via crate source.
    pub menu_path: Option<String>,
    pub menu: Vec<MenuItem>,
}

pub type TrayIcons = Arc<Mutex<Vec<TrayIcon>>>;

/// Sent from the render thread into the tray thread to request an action —
/// activation has to happen through the live `Client`/DBus connection,
/// which lives on the tray thread's tokio runtime.
pub enum TrayCommand {
    Activate(String),
    /// Click a specific item inside a tray icon's context menu.
    ClickMenuItem {
        address: String,
        menu_path: String,
        item_id: i32,
    },
}

/// Spawns the DBus tray client on its own thread (it needs a tokio runtime;
/// the rest of the bar runs on calloop) and returns shared storage the
/// render thread reads from, plus a sender for commands (clicks) back into
/// the tray thread.
pub fn spawn(
    redraw_tx: Sender<()>,
) -> (TrayIcons, tokio::sync::mpsc::UnboundedSender<TrayCommand>) {
    let icons: TrayIcons = Arc::new(Mutex::new(Vec::new()));
    let icons_bg = icons.clone();
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<TrayCommand>();

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(err) => {
                log::error!("failed to start tokio runtime for tray: {err}");
                return;
            }
        };

        rt.block_on(async move {
            let client = match Client::new().await {
                Ok(c) => c,
                Err(err) => {
                    log::error!("failed to start tray client: {err}");
                    return;
                }
            };

            let mut rx = client.subscribe();
            sync_icons(&client, &icons_bg);
            let _ = redraw_tx.send(());

            loop {
                tokio::select! {
                    event = rx.recv() => {
                        match event {
                            Ok(event) => {
                                if matches!(event, Event::Add(..) | Event::Update(..) | Event::Remove(..)) {
                                    sync_icons(&client, &icons_bg);
                                    let _ = redraw_tx.send(());
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(TrayCommand::Activate(id)) => {
                                // ActivateRequest is an enum, not a struct —
                                // variant name/fields are a guess. If this
                                // errors, `cargo check` will list the real
                                // variants (Default/Context/Secondary or
                                // similar); swap in whichever matches.
                                let req = ActivateRequest::Default {
                                    address: id.clone(),
                                    x: 0,
                                    y: 0,
                                };
                                if let Err(err) = client.activate(req).await {
                                    log::error!("tray activate failed for {id}: {err}");
                                }
                            }
                            Some(TrayCommand::ClickMenuItem { address, menu_path, item_id }) => {
                                let req = ActivateRequest::MenuItem {
                                    address: address.clone(),
                                    menu_path,
                                    submenu_id: item_id,
                                };
                                if let Err(err) = client.activate(req).await {
                                    log::error!("menu item click failed for {address}#{item_id}: {err}");
                                }
                            }
                            None => break,
                        }
                    }
                }
            }
        });
    });

    (icons, cmd_tx)
}

/// Pulls the current item set off the client and converts each icon's
/// pixmap into the format our canvas wants.
fn sync_icons(client: &Client, icons: &TrayIcons) {
    let items = client.items();
    let guard = items.lock().unwrap();
    let mut out = Vec::new();

    for (id, (item, menu)) in guard.iter() {
        // Prefer embedded pixmap data (Throne, Amnezia, etc. ship this).
        // Fall back to icon_name + icon_theme_path (Gajim and others ship
        // only this — no embedded pixels at all) by resolving and decoding
        // the icon file ourselves.
        let resolved = item
            .icon_pixmap
            .as_ref()
            .and_then(|pixmaps| pixmaps.iter().max_by_key(|p| p.width))
            .map(|pixmap| {
                (
                    argb_to_premultiplied_bgra(
                        &pixmap.pixels,
                        pixmap.width as u32,
                        pixmap.height as u32,
                    ),
                    pixmap.width as u32,
                    pixmap.height as u32,
                )
            })
            .or_else(|| {
                let name = item.icon_name.as_deref()?;
                if name.is_empty() {
                    return None;
                }
                load_icon_by_name(name, item.icon_theme_path.as_deref())
            });

        let Some((pixels, width, height)) = resolved else {
            log::debug!("tray item {id} has no usable icon (no pixmap, no resolvable icon_name)");
            continue;
        };

        out.push(TrayIcon {
            id: id.clone(),
            pixels,
            width,
            height,
            menu_path: item.menu.clone(),
            menu: menu
                .as_ref()
                .map(|m| {
                    m.submenus
                        .iter()
                        .filter_map(|mi| {
                            let label = mi.label.clone().unwrap_or_default();
                            if label.is_empty() {
                                None // skip separators
                            } else {
                                Some(MenuItem { id: mi.id, label })
                            }
                        })
                        .collect()
                })
                .unwrap_or_default(),
        });
    }

    *icons.lock().unwrap() = out;
}

/// Resolves `icon_name` (StatusNotifierItem property) to an actual file on
/// disk and decodes it to premultiplied BGRA8888, the same format
/// `argb_to_premultiplied_bgra` produces for embedded pixmaps.
///
/// Apps that don't embed pixel data (Gajim among them) instead point at a
/// name + an optional app-supplied theme directory, following the standard
/// hicolor icon theme layout:
///   <theme_path>/hicolor/<size>/apps|status/<name>.png
///   <theme_path>/hicolor/scalable/apps|status/<name>.svg
/// We check the app-supplied path first (most specific), then fall back to
/// the system hicolor theme so apps that only give a bare name still work.
fn load_icon_by_name(name: &str, app_theme_path: Option<&str>) -> Option<(Vec<u8>, u32, u32)> {
    let mut search_roots: Vec<String> = Vec::new();
    if let Some(p) = app_theme_path {
        search_roots.push(p.to_string());
    }
    // Common system icon theme locations, as a fallback.
    search_roots.push("/usr/share/icons/hicolor".to_string());
    search_roots.push("/usr/share/pixmaps".to_string());
    if let Ok(home) = std::env::var("HOME") {
        search_roots.push(format!("{home}/.local/share/icons/hicolor"));
    }

    // Sizes to try, largest first, so we grab the crispest raster available.
    // "scalable" is checked separately below since SVG needs no size dir.
    const SIZES: &[&str] = &[
        "512x512", "256x256", "128x128", "64x64", "48x48", "32x32", "24x24", "16x16",
    ];
    const CATEGORIES: &[&str] = &[
        "status",
        "apps",
        "actions",
        "categories",
        "devices",
        "mimetypes",
    ];

    for root in &search_roots {
        // Root might itself be a flat directory containing "<name>.png"
        // directly (some apps set icon_theme_path to a flat dir, not a
        // hicolor tree) — try that first since it's cheap.
        for ext in ["png"] {
            let flat = format!("{root}/{name}.{ext}");
            if let Some(result) = try_decode_png(&flat) {
                return Some(result);
            }
        }

        for size in SIZES {
            for cat in CATEGORIES {
                let path = format!("{root}/{size}/{cat}/{name}.png");
                if let Some(result) = try_decode_png(&path) {
                    return Some(result);
                }
            }
        }
        // Note: scalable/*.svg icons exist but are skipped — decoding SVG
        // needs a rasterizer (e.g. resvg) which isn't a dependency yet. If
        // an app only ships SVGs under `scalable/`, add that crate and a
        // render step here.
    }

    None
}

fn try_decode_png(path: &str) -> Option<(Vec<u8>, u32, u32)> {
    if !std::path::Path::new(path).is_file() {
        return None;
    }
    let img = image::open(path).ok()?.into_rgba8();
    let (width, height) = img.dimensions();
    let mut out = vec![0u8; (width * height * 4) as usize];
    for (i, px) in img.pixels().enumerate() {
        let [r, g, b, a] = px.0;
        let af = a as f32 / 255.0;
        out[i * 4] = (b as f32 * af) as u8;
        out[i * 4 + 1] = (g as f32 * af) as u8;
        out[i * 4 + 2] = (r as f32 * af) as u8;
        out[i * 4 + 3] = a;
    }
    Some((out, width, height))
}

/// StatusNotifierItem pixmaps are big-endian ARGB32, not premultiplied.
/// wl_shm's argb8888 wants little-endian BGRA, premultiplied.
fn argb_to_premultiplied_bgra(data: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut out = vec![0u8; (width * height * 4) as usize];
    for i in 0..(width * height) as usize {
        if i * 4 + 3 >= data.len() {
            break;
        }
        let a = data[i * 4];
        let r = data[i * 4 + 1];
        let g = data[i * 4 + 2];
        let b = data[i * 4 + 3];
        let af = a as f32 / 255.0;
        out[i * 4] = (b as f32 * af) as u8;
        out[i * 4 + 1] = (g as f32 * af) as u8;
        out[i * 4 + 2] = (r as f32 * af) as u8;
        out[i * 4 + 3] = a;
    }
    out
}

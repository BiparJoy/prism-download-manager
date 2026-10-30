//! System tray icon with a quick menu and live state (idle / downloading / paused).

use crate::i18n::tr;
use prism_core::manager::Core;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

pub const TRAY_ID: &str = "prism-tray";

// Windows/Linux trays are usually dark: white glyphs. macOS uses template images
// (black + alpha) that the menu bar tints for light/dark automatically.
#[cfg(not(target_os = "macos"))]
static ICON_IDLE: &[u8] = include_bytes!("../icons/tray/tray-idle.png");
#[cfg(not(target_os = "macos"))]
static ICON_ACTIVE: &[u8] = include_bytes!("../icons/tray/tray-downloading.png");
#[cfg(target_os = "macos")]
static ICON_IDLE: &[u8] = include_bytes!("../icons/tray/tray-idle-template.png");
#[cfg(target_os = "macos")]
static ICON_ACTIVE: &[u8] = include_bytes!("../icons/tray/tray-downloading-template.png");

pub struct TrayState {
    pub limiter: CheckMenuItem<Wry>,
    /// Menu items with the English text they show (relabelled when the UI language changes).
    items: Vec<(MenuItem<Wry>, &'static str)>,
    /// The tray menu; active downloads are listed in it right under "Open Prism".
    menu: Menu<Wry>,
    /// The downloads listed now (rebuilt only when they change).
    listed: std::sync::Mutex<Listed>,
}

/// Rows the tray menu shows for active downloads, kept so they can be removed by handle.
#[derive(Default)]
struct Listed {
    ids: Vec<String>,
    entries: Vec<MenuItem<Wry>>,
    separator: Option<PredefinedMenuItem<Wry>>,
}

pub fn create(app: &AppHandle) -> tauri::Result<TrayIcon> {
    let item = |id: &str, text: &str| MenuItem::with_id(app, id, tr(text, &[]), true, None::<&str>);
    let show = item("show", "Open Prism")?;
    let add = item("add", "Add URL…")?;
    let pause = item("pause_all", "Pause all")?;
    let resume = item("resume_all", "Resume all")?;
    let limiter_on = app
        .state::<Core>()
        .settings()
        .downloads
        .speed_limiter_enabled;
    let limiter = CheckMenuItem::with_id(
        app,
        "limiter",
        tr("Speed limiter", &[]),
        true,
        limiter_on,
        None::<&str>,
    )?;
    let mini = item("mini", "Mini window")?;
    let drop = item("dropzone", "Drop zone")?;
    let quit = item("quit", "Quit Prism")?;
    let sep = || PredefinedMenuItem::separator(app);
    let menu = Menu::with_items(
        app,
        &[
            &show,
            &add,
            &sep()?,
            &pause,
            &resume,
            &limiter,
            &sep()?,
            &mini,
            &drop,
            &sep()?,
            &quit,
        ],
    )?;
    app.manage(TrayState {
        limiter: limiter.clone(),
        items: vec![
            (show.clone(), "Open Prism"),
            (add.clone(), "Add URL…"),
            (pause.clone(), "Pause all"),
            (resume.clone(), "Resume all"),
            (mini.clone(), "Mini window"),
            (drop.clone(), "Drop zone"),
            (quit.clone(), "Quit Prism"),
        ],
        menu: menu.clone(),
        listed: Default::default(),
    });

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(Image::from_bytes(ICON_IDLE)?)
        .icon_as_template(cfg!(target_os = "macos"))
        .tooltip("Prism")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            let app = app.clone();
            match event.id().as_ref() {
                "show" => crate::windows::show_main(&app),
                "add" => {
                    let _ = crate::windows::open(&app, "addurl", None);
                }
                "pause_all" => app.state::<Core>().pause_all(),
                "resume_all" => app.state::<Core>().resume_all(),
                "limiter" => {
                    let _ = app.state::<Core>().toggle_speed_limiter();
                    refresh(&app);
                }
                "mini" => {
                    let _ = crate::windows::toggle_mini(&app);
                }
                "dropzone" => {
                    let _ = crate::windows::toggle_dropzone(&app);
                }
                "quit" => {
                    tauri::async_runtime::spawn(async move { crate::quit(&app).await });
                }
                id => {
                    if let Some(id) = id.strip_prefix("open:") {
                        let _ = crate::windows::open_progress(&app, id);
                    }
                }
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                crate::windows::show_main(tray.app_handle());
            }
        })
        .build(app)
}

/// Sync the limiter check mark with settings.
pub fn refresh(app: &AppHandle) {
    let on = app
        .state::<Core>()
        .settings()
        .downloads
        .speed_limiter_enabled;
    if let Some(state) = app.try_state::<TrayState>() {
        let _ = state.limiter.set_checked(on);
    }
}

/// Show the menu in the current UI language.
pub fn relabel(app: &AppHandle) {
    if let Some(state) = app.try_state::<TrayState>() {
        for (item, text) in &state.items {
            let _ = item.set_text(tr(text, &[]));
        }
        let _ = state.limiter.set_text(tr("Speed limiter", &[]));
    }
}

/// Update icon + tooltip from aggregate progress (called on each progress event).
pub fn update(app: &AppHandle, active: usize, speed: u64, icon_changed: bool) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let tooltip = if active == 0 {
        tr("Prism — idle", &[])
    } else {
        tr(
            "Prism — {count} downloading · {speed}",
            &[
                ("count", &active.to_string()),
                ("speed", &human_speed(speed)),
            ],
        )
    };
    let _ = tray.set_tooltip(Some(tooltip));
    if !icon_changed {
        return;
    }
    let bytes = if active == 0 { ICON_IDLE } else { ICON_ACTIVE };
    if let Ok(img) = Image::from_bytes(bytes) {
        let _ = tray.set_icon(Some(img));
        let _ = tray.set_icon_as_template(cfg!(target_os = "macos"));
    }
}

/// Lists the active downloads (`ids`) right under "Open Prism", then a separator; nothing
/// when idle. Clicking one opens its progress window.
pub fn list_downloads(app: &AppHandle, ids: &[&str]) {
    let Some(state) = app.try_state::<TrayState>() else {
        return;
    };
    // By name, so the list is stable (progress arrives in hash order) and easy to scan.
    let core = app.state::<Core>();
    let mut wanted: Vec<(String, String)> = ids
        .iter()
        .filter_map(|id| core.item(id).map(|i| (i.file_name, id.to_string())))
        .collect();
    wanted.sort();
    let mut listed = state.listed.lock().unwrap_or_else(|e| e.into_inner());
    if listed.ids.iter().eq(wanted.iter().map(|(_, id)| id)) {
        return;
    }
    let menu = &state.menu;
    for entry in listed.entries.drain(..) {
        let _ = menu.remove(&entry);
    }
    if let Some(sep) = listed.separator.take() {
        let _ = menu.remove(&sep);
    }
    for (name, id) in &wanted {
        let label = format!("↓  {}", short(name));
        if let Ok(entry) = MenuItem::with_id(app, format!("open:{id}"), label, true, None::<&str>)
            && menu.insert(&entry, 1 + listed.entries.len()).is_ok()
        {
            listed.entries.push(entry);
        }
    }
    if !listed.entries.is_empty()
        && let Ok(sep) = PredefinedMenuItem::separator(app)
        && menu.insert(&sep, 1 + listed.entries.len()).is_ok()
    {
        listed.separator = Some(sep);
    }
    listed.ids = wanted.into_iter().map(|(_, id)| id).collect();
}

/// Keeps menu entries readable: long names are cut in the middle.
fn short(name: &str) -> String {
    const MAX: usize = 48;
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= MAX {
        return name.to_string();
    }
    let half = MAX / 2 - 1;
    let head: String = chars[..half].iter().collect();
    let tail: String = chars[chars.len() - half..].iter().collect();
    format!("{head}…{tail}")
}

fn human_speed(bps: u64) -> String {
    const UNITS: [&str; 4] = ["B/s", "KB/s", "MB/s", "GB/s"];
    let mut v = bps as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::short;

    #[test]
    fn long_names_are_cut_in_the_middle() {
        assert_eq!(short("setup.exe"), "setup.exe");
        let long = "Windows11_Client_x64_en-us_26300_9457_with_a_very_long_tail.iso";
        let cut = short(long);
        assert_eq!(cut.chars().count(), 47);
        assert!(cut.starts_with("Windows11_Client") && cut.ends_with("long_tail.iso"));
        assert!(cut.contains('…'));
        // Multi-byte names are cut on character boundaries.
        assert_eq!(short(&"ž".repeat(60)).chars().count(), 47);
    }
}

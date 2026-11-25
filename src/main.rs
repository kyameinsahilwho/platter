// Platter — see, clean, optimize and manage the disks on a Windows PC.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod appicon;
mod cache;
mod cleanup;
mod disks;
mod dupes;
mod manage;
mod optimize;
mod perf;
mod scan;
mod uninstall;
mod util;
mod volumes;

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::Duration;
use tauri::webview::PageLoadEvent;
use tauri::window::Color;
use tauri::{Emitter, Manager, PhysicalPosition, PhysicalSize, Theme};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SystemInfo {
    elevated: bool,
    /// Ask for administrator rights each time Platter starts (on unless the user turned it off).
    /// `None` when this Platter runs elevated as another account: the setting lives in the desktop
    /// user's profile, which this process can't see, so it can only be changed in standard mode.
    ask_admin_at_start: Option<bool>,
    computer_name: String,
    system_drive: String,
    user_profile: String,
    downloads: String,
}

#[tauri::command]
fn system_info() -> SystemInfo {
    let profile = util::env_path("USERPROFILE").unwrap_or_default();
    SystemInfo {
        elevated: util::is_elevated(),
        ask_admin_at_start: (!cleanup::other_account()).then(util::ask_admin_at_start),
        computer_name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "This PC".into()),
        system_drive: util::system_drive_letter(),
        downloads: profile.join("Downloads").to_string_lossy().into_owned(),
        user_profile: profile.to_string_lossy().into_owned(),
    }
}

/// Async so the UAC prompt (ShellExecuteW blocks until it is answered) never stalls the UI thread.
#[tauri::command]
async fn relaunch_elevated(app: tauri::AppHandle) -> Result<bool, String> {
    let owner = app
        .get_webview_window("main")
        .and_then(|w| w.hwnd().ok())
        .map(|h| h.0 as usize)
        .unwrap_or(0);
    // One UAC prompt at a time: with the secure desktop off, the window stays clickable.
    static PROMPTING: AtomicBool = AtomicBool::new(false);
    if PROMPTING.swap(true, Ordering::SeqCst) {
        return Ok(false);
    }
    let res = tauri::async_runtime::spawn_blocking(move || util::relaunch_elevated(owner)).await;
    PROMPTING.store(false, Ordering::SeqCst);
    let ok = res.map_err(|e| e.to_string())?;
    if ok {
        util::note_main_window_closing();
        save_window_state_now();
        app.exit(0);
    }
    Ok(ok)
}

/// Turn the startup administrator prompt on or off for the next launches.
#[tauri::command]
async fn set_ask_admin(enabled: bool) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        if cleanup::other_account() {
            return Err("Platter is running as a different Windows account. Change this setting in standard mode.".to_string());
        }
        cache::write(util::ASK_ADMIN_KEY, &enabled);
        if util::ask_admin_at_start() != enabled {
            return Err("Platter couldn't save this setting.".into());
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

// ------------------------------------------------------------------------------------- showing
//
// The main window is built hidden, placed and sized from the saved state, and shown as soon as that
// is done (well before the page has painted): its window and WebView background and its title bar are
// already in the theme the page will use, so it appears at once without a white or light flash, and
// the page fills in a moment later (measured: visible ~110 ms sooner than when shown after the first
// paint, page content on screen at about the same time). `app_ready` (called by the UI after its first paint) shows it too,
// as a safety net, and starts the background work that must not compete with startup; if that call
// never comes (a script error), that work starts after STARTUP_FALLBACK.

const STARTUP_FALLBACK: Duration = Duration::from_millis(1500);
static SHOWN: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);

/// Show the main window the first time this is called; later calls do nothing. Only shown, not
/// forced to the front: Windows activates it when this launch may take the foreground (the user just
/// started it) and flashes its taskbar button when the user has meanwhile gone on in another app.
fn show_main(app: &tauri::AppHandle, mark: &str) {
    if SHOWN.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
    }
    perf::mark(mark);
}

/// Work that waits until the UI is up, once.
fn after_startup(app: &tauri::AppHandle) {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    start_drive_watcher(app.clone());
}

/// Called by the UI after its first paint.
#[tauri::command]
fn app_ready(app: tauri::AppHandle) {
    perf::mark("rust:app-ready");
    show_main(&app, "rust:window-shown-late");
    after_startup(&app);
}

// --------------------------------------------------------------------------------------- theme

const BG_LIGHT: Color = Color(0xED, 0xEF, 0xEA, 0xFF);
const BG_DARK: Color = Color(0x0F, 0x17, 0x14, 0xFF);

fn valid_theme(t: &str) -> bool {
    matches!(t, "light" | "dark" | "system")
}

/// The theme the UI last asked for ("light" | "dark" | "system"), if it ever did.
fn saved_theme_pref() -> Option<String> {
    cache::read("theme").and_then(|e| e.data.as_str().map(str::to_owned)).filter(|t| valid_theme(t))
}

/// Runs in the page before its own scripts. The backend's choice (the cache is shared by every
/// Platter of this user) is the one that counts: an elevated Platter has its own WebView2 profile and
/// so its own, empty localStorage, and a choice made there must also reach the normal one. Seeding
/// both keys the UI reads keeps the page, the window background and the title bar in one theme, and
/// keeps the UI from pushing a stale or default value back.
fn theme_seed_script(pref: &str) -> String {
    let t = serde_json::to_string(pref).unwrap_or_else(|_| "\"system\"".into());
    format!("try{{localStorage.setItem(\"platter.theme\",{t});localStorage.setItem(\"platter.themeSynced\",{t})}}catch(e){{}}")
}

fn is_dark(pref: &str) -> bool {
    match pref {
        "dark" => true,
        "light" => false,
        _ => util::system_uses_light_theme() == Some(false),
    }
}

/// Title-bar theme: forced for an explicit choice, `None` follows Windows.
fn window_theme(pref: &str) -> Option<Theme> {
    match pref {
        "dark" => Some(Theme::Dark),
        "light" => Some(Theme::Light),
        _ => None,
    }
}

/// Remember the UI's theme choice so the next launch paints the window and title bar in it from the
/// first frame, and apply it to the title bar and the page background now.
#[tauri::command]
async fn set_theme_pref(app: tauri::AppHandle, theme: String) -> Result<(), String> {
    if !valid_theme(&theme) {
        return Err("Unknown theme.".into());
    }
    let pref = theme.clone();
    tauri::async_runtime::spawn_blocking(move || cache::write("theme", &pref)).await.map_err(|e| e.to_string())?;
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.set_theme(window_theme(&theme));
        let _ = w.set_background_color(Some(if is_dark(&theme) { BG_DARK } else { BG_LIGHT }));
    }
    Ok(())
}

// -------------------------------------------------------------------------------- window state
//
// Size, position and maximized state are kept in the cache ("window") and restored while the window
// is still hidden, so it appears in place without a jump. `x`/`y` are the outer position and
// `width`/`height` the inner size, in physical pixels; they always describe the restored (normal)
// rectangle, also while maximized.

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
struct WindowState {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    maximized: bool,
}

static WINDOW_STATE: Mutex<Option<WindowState>> = Mutex::new(None);
static SAVE_TX: OnceLock<Mutex<mpsc::Sender<()>>> = OnceLock::new();

/// A rectangle worth restoring: big enough, and with the middle of its title bar on a monitor that is
/// connected now. Returns the rectangle clamped to that monitor's work area.
fn usable_rect(s: &WindowState, monitors: &[(i32, i32, u32, u32)]) -> Option<(i32, i32, u32, u32)> {
    if s.width < 200 || s.height < 150 {
        return None;
    }
    let (px, py) = (s.x.saturating_add((s.width / 2) as i32), s.y.saturating_add(24));
    let &(_, _, mw, mh) = monitors.iter().find(|&&(mx, my, mw, mh)| {
        px >= mx && py >= my && (px as i64) < mx as i64 + mw as i64 && (py as i64) < my as i64 + mh as i64
    })?;
    Some((s.x, s.y, s.width.min(mw), s.height.min(mh)))
}

fn restore_window_state(w: &tauri::WebviewWindow) {
    let saved = cache::read("window").and_then(|e| serde_json::from_value::<WindowState>(e.data).ok());
    let rect = saved.and_then(|s| {
        let monitors: Vec<_> = w
            .available_monitors()
            .unwrap_or_default()
            .iter()
            .map(|m| {
                let a = m.work_area();
                (a.position.x, a.position.y, a.size.width, a.size.height)
            })
            .collect();
        usable_rect(&s, &monitors)
    });
    match rect {
        Some((x, y, width, height)) => {
            let _ = w.set_position(PhysicalPosition::new(x, y));
            let _ = w.set_size(PhysicalSize::new(width, height));
        }
        None => fit_default_size(w),
    }
    // Start from what the window really is now (default size when nothing usable was saved).
    let mut state = saved.unwrap_or_default();
    if let (Ok(p), Ok(sz)) = (w.outer_position(), w.inner_size()) {
        (state.x, state.y, state.width, state.height) = (p.x, p.y, sz.width, sz.height);
    }
    // Last, because tao's set_position/set_size clear its maximized flag: so the window also opens
    // maximized when the saved rectangle's monitor is gone (with the default as its restored size).
    // While hidden this only sets the flag; the window is shown maximized.
    if state.maximized {
        let _ = w.maximize();
    }
    if let Ok(mut g) = WINDOW_STATE.lock() {
        *g = Some(state);
    }
}

/// The default size (tauri.conf.json) is taller than the work area of a 1080p screen at 125 %: shrink
/// it to fit the monitor's work area (never below the minimum size) and center it there.
fn fit_default_size(w: &tauri::WebviewWindow) {
    let (Ok(Some(m)), Ok(outer), Ok(inner)) = (w.current_monitor(), w.outer_size(), w.inner_size()) else { return };
    let a = m.work_area();
    let (fw, fh) = (outer.width.min(a.size.width), outer.height.min(a.size.height));
    if (fw, fh) != (outer.width, outer.height) {
        let width = inner.width.saturating_sub(outer.width - fw);
        let height = inner.height.saturating_sub(outer.height - fh);
        let _ = w.set_size(PhysicalSize::new(width, height));
    }
    let x = a.position.x + (a.size.width.saturating_sub(fw) / 2) as i32;
    let y = a.position.y + (a.size.height.saturating_sub(fh) / 2) as i32;
    let _ = w.set_position(PhysicalPosition::new(x, y));
}

/// Track the main window's geometry after a move or resize and save it shortly after it settles.
/// The rectangle is only taken while the window is neither maximized nor minimized, as Windows has it
/// right now (tao's maximized flag is updated only after the move event of a maximize, or of a restore
/// to maximized from minimized), so it always stays the restored (normal) rectangle.
fn note_window_change(w: &tauri::Window) {
    if !SHOWN.load(Ordering::SeqCst) {
        return;
    }
    let Ok(h) = w.hwnd() else { return };
    let (maximized, minimized) = util::window_zoomed_iconic(h.0 as usize);
    if minimized {
        return;
    }
    let Ok(mut g) = WINDOW_STATE.lock() else { return };
    let mut s = g.unwrap_or_default();
    s.maximized = maximized;
    if !maximized {
        if let (Ok(p), Ok(sz)) = (w.outer_position(), w.inner_size()) {
            (s.x, s.y, s.width, s.height) = (p.x, p.y, sz.width, sz.height);
        }
    }
    if *g == Some(s) {
        return;
    }
    *g = Some(s);
    drop(g);
    let tx = SAVE_TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<()>();
        let _ = std::thread::Builder::new().name("platter-window-state".into()).spawn(move || {
            while rx.recv().is_ok() {
                // Wait until moves/resizes stop for a moment, then write once.
                while rx.recv_timeout(Duration::from_millis(500)).is_ok() {}
                save_window_state_now();
            }
        });
        Mutex::new(tx)
    });
    if let Ok(tx) = tx.lock() {
        let _ = tx.send(());
    }
}

fn save_window_state_now() {
    let s = WINDOW_STATE.lock().ok().and_then(|g| *g);
    if let Some(s) = s {
        cache::write("window", &s);
    }
}

// -------------------------------------------------------------------------------- drive watcher

/// Watch for drives being plugged in or removed, and for media going in or out of card readers and
/// optical drives (their letter stays put, so the mask alone misses that). Started once the window is
/// shown, so it never competes with startup.
fn start_drive_watcher(handle: tauri::AppHandle) {
    let _ = std::thread::Builder::new().name("platter-drive-watch".into()).spawn(move || {
        let mut last_mask = volumes::drive_mask();
        let mut last_media = volumes::media_signature(last_mask, &[None; 26]);
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let mask = volumes::drive_mask();
            let media = volumes::media_signature(mask, &last_media);
            if mask != last_mask || media != last_media {
                last_mask = mask;
                last_media = media;
                let _ = handle.emit("drives-changed", ());
            }
        }
    });
}

// ------------------------------------------------------------------------------------- commands

#[tauri::command]
async fn list_volumes() -> Result<Vec<volumes::Volume>, String> {
    tauri::async_runtime::spawn_blocking(volumes::list).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn allocation_map(letter: String, cells: usize) -> Result<volumes::AllocationMap, String> {
    tauri::async_runtime::spawn_blocking(move || {
        // Only ask this drive: listing every drive here would wait on slow network shares.
        let cluster = Some(volumes::cluster_size(&letter)).filter(|&c| c > 0).unwrap_or(4096);
        volumes::allocation_map(&letter, cells, cluster)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Marks the first IPC call from the page on the perf timeline.
fn with_first_ipc_mark<F>(handler: F) -> impl Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static
where
    F: Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static,
{
    static FIRST_IPC: AtomicBool = AtomicBool::new(false);
    move |invoke| {
        perf::mark_once(&FIRST_IPC, "rust:first-ipc");
        handler(invoke)
    }
}

fn main() {
    perf::init();
    perf::mark("rust:main");
    // Polling empty card readers / optical drives must never pop "There is no disk in the drive".
    util::suppress_critical_error_dialogs();
    // Started by relaunch_elevated: take over as the single instance at once (the old instance still
    // holds the mutex, so it is never free in between), let the old instance know, then wait for it
    // (and its WebView2) to go away before starting our own.
    let elevated_relaunch = util::wait_for_pid_arg(std::env::args().skip(1)).is_some();
    if !util::claim_single_instance(elevated_relaunch) && util::defer_to_existing_instance() {
        perf::mark("rust:already-running");
        return;
    }
    if elevated_relaunch {
        util::note_taken_over();
        util::wait_for_parent_from_args();
        util::hold_instance_mutex();
    }
    // This is the one Platter: ask for administrator rights before any window or WebView exists.
    // Accepted, the elevated copy takes over once this process has exited; declined, run standard.
    if util::should_elevate_at_start(elevated_relaunch) && util::elevate_at_start() {
        perf::mark("rust:elevated-at-start");
        return;
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            perf::mark("rust:setup");
            // The main window is declared in tauri.conf.json with "create": false and built here, so an
            // elevated instance can get its own WebView2 user data folder: WebView2 can't share one
            // folder between processes running at different integrity levels.
            let elevated = util::is_elevated();
            let cfg = app
                .config()
                .app
                .windows
                .iter()
                .find(|w| w.label == "main")
                .cloned()
                .ok_or("tauri.conf.json has no \"main\" window")?;
            let saved_pref = saved_theme_pref();
            let pref = saved_pref.clone().unwrap_or_else(|| "system".into());
            let mut builder = tauri::WebviewWindowBuilder::from_config(app.handle(), &cfg)?
                .title(if elevated { "Platter (Administrator)" } else { "Platter" })
                .visible(false)
                .window_classname(util::MAIN_WINDOW_CLASS)
                // Window and WebView background in the theme the page will use, so nothing flashes.
                .background_color(if is_dark(&pref) { BG_DARK } else { BG_LIGHT })
                .theme(window_theme(&pref))
                .on_page_load(|_, payload| match payload.event() {
                    PageLoadEvent::Started => perf::mark("rust:page-load-started"),
                    PageLoadEvent::Finished => perf::mark("rust:page-load-finished"),
                });
            if let Some(p) = &saved_pref {
                builder = builder.initialization_script(theme_seed_script(p));
            }
            if elevated {
                if let Ok(dir) = app.path().app_local_data_dir() {
                    builder = builder.data_directory(dir.join("elevated"));
                }
            }
            let window = builder.build()?;
            perf::mark("rust:window-built");
            restore_window_state(&window);
            if let Ok(h) = window.hwnd() {
                util::install_activation_handler(h.0 as usize);
            }
            show_main(app.handle(), "rust:window-shown");

            let handle = app.handle().clone();
            let _ = std::thread::Builder::new().name("platter-startup-fallback".into()).spawn(move || {
                std::thread::sleep(STARTUP_FALLBACK);
                after_startup(&handle);
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() != "main" {
                return;
            }
            match event {
                tauri::WindowEvent::Moved(_) | tauri::WindowEvent::Resized(_) => note_window_change(window),
                tauri::WindowEvent::CloseRequested { .. } => {
                    util::note_main_window_closing();
                    save_window_state_now();
                }
                _ => {}
            }
        })
        .invoke_handler(with_first_ipc_mark(tauri::generate_handler![
            system_info,
            perf::perf_mark,
            perf::perf_enabled,
            app_ready,
            set_theme_pref,
            cache::cache_get_many,
            relaunch_elevated,
            set_ask_admin,
            list_volumes,
            allocation_map,
            disks::list_disks,
            disks::rescan_disks,
            optimize::optimize_start,
            optimize::optimize_cancel,
            optimize::optimize_history,
            optimize::optimize_schedule,
            manage::set_label,
            manage::free_letters,
            manage::change_letter,
            manage::check_volume,
            manage::open_path,
            manage::reveal_path,
            manage::recycle_paths,
            manage::open_recycle_bin,
            scan::scan_start,
            scan::scan_cancel,
            scan::scan_node,
            scan::scan_top_files,
            cleanup::cleanup_analyze,
            cleanup::cleanup_run,
            uninstall::apps_list,
            uninstall::app_icon,
            uninstall::app_uninstall,
            uninstall::app_uninstall_stop_waiting,
            uninstall::app_leftovers,
            uninstall::orphan_leftovers,
            uninstall::leftovers_remove,
            dupes::dupes_default_roots,
            dupes::dupes_start,
            dupes::dupes_cancel,
            dupes::dupes_result,
            dupes::dupes_thumb,
            dupes::dupes_delete,
        ]))
        .run(tauri::generate_context!())
        .expect("error while running Platter");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_rect_must_be_on_a_connected_monitor() {
        let mons = [(0, 0, 1920, 1040), (1920, 0, 2560, 1400)];
        let s = WindowState { x: 100, y: 50, width: 1320, height: 860, maximized: false };
        assert_eq!(usable_rect(&s, &mons), Some((100, 50, 1320, 860)));
        // On the second monitor, larger than the first one.
        let s2 = WindowState { x: 2000, y: 10, width: 2400, height: 1300, ..s };
        assert_eq!(usable_rect(&s2, &mons), Some((2000, 10, 2400, 1300)));
        // Bigger than its monitor: clamped.
        let s3 = WindowState { x: 0, y: 0, width: 3000, height: 2000, ..s };
        assert_eq!(usable_rect(&s3, &mons), Some((0, 0, 1920, 1040)));
        // Monitor gone (was left of the primary), title bar off every monitor, or junk size.
        assert_eq!(usable_rect(&WindowState { x: -1800, ..s }, &mons), None);
        assert_eq!(usable_rect(&WindowState { y: 1100, ..s }, &mons), None);
        assert_eq!(usable_rect(&WindowState { width: 0, height: 0, ..s }, &mons), None);
        assert_eq!(usable_rect(&s, &[]), None);
    }

    #[test]
    fn theme_prefs() {
        assert!(valid_theme("light") && valid_theme("dark") && valid_theme("system") && !valid_theme("blue"));
        assert!(is_dark("dark") && !is_dark("light"));
        assert_eq!(window_theme("system"), None);
        assert_eq!(window_theme("dark"), Some(Theme::Dark));
        assert_eq!(
            theme_seed_script("dark"),
            r#"try{localStorage.setItem("platter.theme","dark");localStorage.setItem("platter.themeSynced","dark")}catch(e){}"#
        );
    }
}

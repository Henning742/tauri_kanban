use serde::{Deserialize, Serialize};
use std::fs;
use tauri::Manager;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

// ── Storage layout ─────────────────────────────────────────────
// <exe_dir>/kanban-data/
//   latest.json           <- always the most recent saved board (raw board JSON)
//   history/
//     index.json          <- ordered list of snapshot metadata
//     state_<ts>.json     <- individual historical snapshots (raw board JSON)

const APP_SUBDIR: &str = "kanban-data";
const HIST_SUBDIR: &str = "history";
const MAX_HISTORY: usize = 50;

#[derive(Serialize, Deserialize, Clone, Default)]
struct HistoryEntry {
    filename: String,
    timestamp: String,
    columns: usize,
    cards: usize,
}

#[derive(Serialize, Deserialize, Default)]
struct HistoryIndex {
    snapshots: Vec<HistoryEntry>,
}

fn data_dir() -> Result<PathBuf, String> {
    // NOTE: tauri's `path().executable_dir()` is NOT "the folder containing
    // the running executable" — it maps to the `dirs` crate's XDG
    // executable-dir concept, which is Linux-only and returns an
    // "unknown path" error on Windows and macOS. To reliably get the
    // directory the .exe itself lives in, resolve it directly.
    let exe_path = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_dir = exe_path
        .parent()
        .ok_or_else(|| "could not resolve executable directory".to_string())?;
    Ok(exe_dir.join(APP_SUBDIR))
}

fn history_dir() -> Result<PathBuf, String> {
    Ok(data_dir()?.join(HIST_SUBDIR))
}

fn read_index(path: &PathBuf) -> HistoryIndex {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Write `contents` to `final_path` via a temp file + rename so a crash or
/// power loss mid-write can never leave a half-written/corrupt file behind.
fn atomic_write(dir: &PathBuf, final_path: &PathBuf, contents: &str) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_path = dir.join(format!(".tmp_{}_{}", std::process::id(), nanos));
    fs::write(&tmp_path, contents).map_err(|e| e.to_string())?;
    fs::rename(&tmp_path, final_path).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_storage_dir() -> Result<String, String> {
    let dir = data_dir()?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.to_string_lossy().to_string())
}

/// Returns the raw board JSON of the most recently saved state, or None if
/// this is a fresh install / no state has ever been saved.
#[tauri::command]
fn load_latest_state() -> Result<Option<String>, String> {
    let path = data_dir()?.join("latest.json");
    if !path.exists() {
        return Ok(None);
    }
    fs::read_to_string(&path).map(Some).map_err(|e| e.to_string())
}

/// Returns the metadata for every saved history snapshot, oldest first.
#[tauri::command]
fn list_history() -> Result<Vec<HistoryEntry>, String> {
    let idx_path = history_dir()?.join("index.json");
    Ok(read_index(&idx_path).snapshots)
}

/// Returns the raw board JSON stored in a specific history snapshot file.
#[tauri::command]
fn load_history_entry(filename: String) -> Result<String, String> {
    // Guard against path traversal since `filename` comes from the frontend.
    if filename.contains("..") || filename.contains('/') || filename.contains('\\') {
        return Err("invalid filename".to_string());
    }
    let path = history_dir()?.join(&filename);
    fs::read_to_string(&path).map_err(|e| e.to_string())
}

/// Persists a new snapshot of the board: writes it into history, updates
/// latest.json to match, prunes old history beyond MAX_HISTORY, and returns
/// the refreshed history index so the frontend can update its UI in one call.
#[tauri::command]
fn save_snapshot(
    board_json: String,
    timestamp: String,
    iso: String,
) -> Result<Vec<HistoryEntry>, String> {
    if timestamp.contains("..") || timestamp.contains('/') || timestamp.contains('\\') {
        return Err("invalid timestamp".to_string());
    }

    let board: serde_json::Value =
        serde_json::from_str(&board_json).map_err(|e| e.to_string())?;
    let columns = board
        .get("columns")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let cards = board
        .get("cards")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    let d_dir = data_dir()?;
    let h_dir = history_dir()?;
    fs::create_dir_all(&h_dir).map_err(|e| e.to_string())?;

    // 1. Write the history snapshot file (atomic).
    let filename = format!("state_{}.json", timestamp);
    let hist_path = h_dir.join(&filename);
    atomic_write(&h_dir, &hist_path, &board_json)?;

    // 2. Update latest.json to mirror the newest snapshot (atomic).
    let latest_path = d_dir.join("latest.json");
    atomic_write(&d_dir, &latest_path, &board_json)?;

    // 3. Update the history index, pruning anything past MAX_HISTORY.
    let idx_path = h_dir.join("index.json");
    let mut index = read_index(&idx_path);
    index.snapshots.push(HistoryEntry {
        filename,
        timestamp: iso,
        columns,
        cards,
    });
    while index.snapshots.len() > MAX_HISTORY {
        let old = index.snapshots.remove(0);
        let _ = fs::remove_file(h_dir.join(&old.filename));
    }
    let idx_json = serde_json::to_string_pretty(&index).map_err(|e| e.to_string())?;
    atomic_write(&h_dir, &idx_path, &idx_json)?;

    Ok(index.snapshots)
}

/// Portrait size used while the app is in widget mode (logical pixels).
const WIDGET_WINDOW_WIDTH: f64 = 380.0;
const WIDGET_WINDOW_HEIGHT: f64 = 680.0;

#[derive(Clone, Copy, PartialEq)]
struct WindowGeometry {
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    maximized: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
struct WindowState {
    // Normal-mode geometry. These flat field names are kept so existing
    // window-state.json files keep working without migration.
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    maximized: bool,
    // Widget-mode geometry (physical pixels). A zero width/height means the
    // widget geometry has not been saved yet, in which case a portrait-sized
    // default is used the first time widget mode is entered.
    #[serde(default)]
    widget_width: u32,
    #[serde(default)]
    widget_height: u32,
    #[serde(default)]
    widget_x: i32,
    #[serde(default)]
    widget_y: i32,
    #[serde(default)]
    widget_maximized: bool,
    #[serde(default)]
    widget_mode: bool,
    #[serde(default)]
    always_on_top: bool,
}

impl WindowState {
    fn normal_geometry(&self) -> WindowGeometry {
        WindowGeometry {
            width: self.width,
            height: self.height,
            x: self.x,
            y: self.y,
            maximized: self.maximized,
        }
    }

    fn set_normal_geometry(&mut self, geometry: WindowGeometry) {
        self.width = geometry.width;
        self.height = geometry.height;
        self.x = geometry.x;
        self.y = geometry.y;
        self.maximized = geometry.maximized;
    }

    fn widget_geometry(&self) -> Option<WindowGeometry> {
        if self.widget_width == 0 || self.widget_height == 0 {
            return None;
        }
        Some(WindowGeometry {
            width: self.widget_width,
            height: self.widget_height,
            x: self.widget_x,
            y: self.widget_y,
            maximized: self.widget_maximized,
        })
    }

    fn set_widget_geometry(&mut self, geometry: WindowGeometry) {
        self.widget_width = geometry.width;
        self.widget_height = geometry.height;
        self.widget_x = geometry.x;
        self.widget_y = geometry.y;
        self.widget_maximized = geometry.maximized;
    }
}

fn fallback_window_state() -> WindowState {
    WindowState {
        width: 800,
        height: 600,
        x: 0,
        y: 0,
        maximized: false,
        widget_width: 0,
        widget_height: 0,
        widget_x: 0,
        widget_y: 0,
        widget_maximized: false,
        widget_mode: false,
        always_on_top: false,
    }
}

static WIDGET_MODE: AtomicBool = AtomicBool::new(false);
static ALWAYS_ON_TOP: AtomicBool = AtomicBool::new(false);
static SESSION_NORMAL_GEOMETRY: OnceLock<Mutex<Option<WindowGeometry>>> = OnceLock::new();
static SESSION_WIDGET_GEOMETRY: OnceLock<Mutex<Option<WindowGeometry>>> = OnceLock::new();

fn session_normal_geometry() -> &'static Mutex<Option<WindowGeometry>> {
    SESSION_NORMAL_GEOMETRY.get_or_init(|| Mutex::new(None))
}

fn session_widget_geometry() -> &'static Mutex<Option<WindowGeometry>> {
    SESSION_WIDGET_GEOMETRY.get_or_init(|| Mutex::new(None))
}

fn window_state_path() -> Result<PathBuf, String> {
    Ok(data_dir()?.join("window-state.json"))
}

fn read_saved_window_state() -> Option<WindowState> {
    let path = window_state_path().ok()?;
    let raw = fs::read_to_string(&path).ok()?;
    serde_json::from_str::<WindowState>(&raw).ok()
}

fn current_window_geometry(window: &tauri::WebviewWindow) -> Option<WindowGeometry> {
    // Capture the client-area (inner) size: `apply_geometry` restores it with
    // `set_size`, which maps to the window's inner size. Capturing the outer
    // size here made the window grow by the title-bar/border delta on every
    // mode switch or restart. Position stays outer, matching `set_position`.
    let size = window.inner_size().ok()?;
    let pos = window.outer_position().ok()?;
    Some(WindowGeometry {
        width: size.width,
        height: size.height,
        x: pos.x,
        y: pos.y,
        maximized: window.is_maximized().unwrap_or(false),
    })
}

fn apply_geometry(window: &tauri::WebviewWindow, geometry: WindowGeometry) {
    apply_geometry_in_order(window, geometry, true);
}

/// Applies a saved geometry to the window, issuing the async size/position
/// requests in the given order. The requests are processed asynchronously by
/// the window manager, and resizing a window can move it (and vice versa), so
/// the last request "wins" where the window finally lands.
fn apply_geometry_in_order(
    window: &tauri::WebviewWindow,
    geometry: WindowGeometry,
    size_first: bool,
) {
    if geometry.maximized {
        let _ = window.maximize();
        return;
    }

    let set_size = || {
        let _ = window.set_size(tauri::Size::Physical(tauri::PhysicalSize {
            width: geometry.width,
            height: geometry.height,
        }));
    };
    let set_position = || {
        let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition {
            x: geometry.x,
            y: geometry.y,
        }));
    };

    if size_first {
        set_size();
        std::thread::sleep(std::time::Duration::from_millis(100));
        set_position();
    } else {
        set_position();
        std::thread::sleep(std::time::Duration::from_millis(100));
        set_size();
    }
}

/// Applies geometry on a background thread after a short delay, giving the
/// window-flag changes (skip taskbar / always-on-bottom) and any window-manager
/// restack a chance to settle first. Applying immediately left the async
/// resize half-settled, so a quick mode switch could read back a mixed
/// geometry (new size, old position) and save the wrong value.
fn apply_geometry_delayed(
    window: tauri::WebviewWindow,
    geometry: WindowGeometry,
    size_first: bool,
) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        apply_geometry_in_order(&window, geometry, size_first);
    });
}

fn apply_widget_window_flags(window: &tauri::WebviewWindow) {
    // Keeps the window minimizable and in the taskbar. Always-on-top is handled
    // independently by `apply_always_on_top`, so it can be toggled in both
    // normal and widget mode.
    let _ = window.set_minimizable(true);
    let _ = window.set_skip_taskbar(false);
    let _ = window.set_always_on_bottom(false);
    let _ = window.set_always_on_top(ALWAYS_ON_TOP.load(Ordering::SeqCst));
}

fn apply_always_on_top(window: &tauri::WebviewWindow, enabled: bool) {
    ALWAYS_ON_TOP.store(enabled, Ordering::SeqCst);
    let _ = window.set_always_on_top(enabled);
    let _ = window.set_always_on_bottom(false);
}

/// First-time widget geometry: a portrait window at the normal window's
/// position (centered on the current monitor when no usable normal position
/// exists, e.g. the normal state was maximized).
fn default_widget_geometry(
    window: &tauri::WebviewWindow,
    normal: Option<WindowGeometry>,
) -> WindowGeometry {
    let scale = window.scale_factor().unwrap_or(1.0).max(0.1);
    let width = ((WIDGET_WINDOW_WIDTH * scale).round() as u32).max(1);
    let height = ((WIDGET_WINDOW_HEIGHT * scale).round() as u32).max(1);

    let (x, y) = match normal {
        Some(normal) if !normal.maximized => (normal.x, normal.y),
        _ => {
            if let Ok(Some(monitor)) = window.current_monitor() {
                let monitor_size = monitor.size();
                let monitor_pos = monitor.position();
                (
                    monitor_pos.x + ((monitor_size.width as i32 - width as i32) / 2),
                    monitor_pos.y + ((monitor_size.height as i32 - height as i32) / 2),
                )
            } else {
                (0, 0)
            }
        }
    };

    WindowGeometry {
        width,
        height,
        x,
        y,
        maximized: false,
    }
}

/// Returns the widget geometry the window is currently showing. When the
/// window geometry is still the normal-mode geometry (the async resize has
/// not been applied yet) and an intended widget geometry exists, prefer the
/// intended one.
fn capture_widget_geometry(window: &tauri::WebviewWindow) -> Option<WindowGeometry> {
    let actual = current_window_geometry(window);
    let intended = session_widget_geometry()
        .lock()
        .ok()
        .and_then(|guard| *guard);
    let normal = session_normal_geometry()
        .lock()
        .ok()
        .and_then(|guard| *guard);

    match (actual, intended, normal) {
        (Some(actual), Some(intended), Some(normal))
            if actual == normal && intended != normal =>
        {
            Some(intended)
        }
        (Some(actual), _, _) => Some(actual),
        (None, intended, _) => intended,
    }
}

fn write_window_state(state: WindowState) {
    if let Ok(path) = window_state_path() {
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(&state) {
            let _ = fs::write(&path, json);
        }
    }
}

/// Writes window-state.json without reading the on-screen geometry. A `None`
/// geometry leaves that section untouched, which lets us persist a known
/// normal/widget geometry before the async window resize has settled.
fn persist_window_state(
    normal: Option<WindowGeometry>,
    widget: Option<WindowGeometry>,
    widget_mode: bool,
) {
    let mut state = read_saved_window_state().unwrap_or_else(fallback_window_state);
    if let Some(normal) = normal {
        state.set_normal_geometry(normal);
    }
    if let Some(widget) = widget {
        state.set_widget_geometry(widget);
    }
    state.widget_mode = widget_mode;
    state.always_on_top = ALWAYS_ON_TOP.load(Ordering::SeqCst);
    write_window_state(state);
}

fn apply_fallback_window_geometry(window: &tauri::WebviewWindow) {
    if let Ok(Some(monitor)) = window.current_monitor() {
        let size = monitor.size();
        let target_w = (size.width as f64 * 0.8) as u32;
        let target_h = (size.height as f64 * 0.8) as u32;
        let _ = window.set_size(tauri::Size::Physical(tauri::PhysicalSize {
            width: target_w,
            height: target_h,
        }));
        let _ = window.center();
    }
}

fn apply_initial_window_state(window: &tauri::WebviewWindow) {
    if let Some(state) = read_saved_window_state() {
        let normal = state.normal_geometry();
        WIDGET_MODE.store(state.widget_mode, Ordering::SeqCst);
        apply_always_on_top(window, state.always_on_top);
        apply_widget_window_flags(window);
        if state.widget_mode {
            let widget = state
                .widget_geometry()
                .unwrap_or_else(|| default_widget_geometry(window, Some(normal)));
            apply_geometry(window, widget);
        } else {
            apply_geometry(window, normal);
        }
        return;
    }

    // No saved state yet (first launch): size to ~80% of the current monitor, centered.
    WIDGET_MODE.store(false, Ordering::SeqCst);
    ALWAYS_ON_TOP.store(false, Ordering::SeqCst);
    apply_fallback_window_geometry(window);
}

fn save_window_state(window: &tauri::WebviewWindow) {
    let widget_mode = WIDGET_MODE.load(Ordering::SeqCst);

    if widget_mode {
        let normal = session_normal_geometry()
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .or_else(|| read_saved_window_state().map(|state| state.normal_geometry()));
        let widget = capture_widget_geometry(window);
        persist_window_state(normal, widget, true);
    } else {
        // Normal mode: the on-screen window is authoritative, and previously
        // saved widget geometry is preserved untouched.
        persist_window_state(current_window_geometry(window), None, false);
    }
}

/// Enables/disables widget mode. Both modes remember their own position and
/// size across switches and restarts.
#[tauri::command]
fn set_widget_mode(app: tauri::AppHandle, enabled: bool) -> Result<bool, String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main window not found".to_string())?;

    if enabled == WIDGET_MODE.load(Ordering::SeqCst) {
        return Ok(enabled);
    }

    if enabled {
        // Remember the current normal-mode geometry.
        let normal = current_window_geometry(&window)
            .or_else(|| read_saved_window_state().map(|state| state.normal_geometry()));
        if let Some(normal) = normal {
            if let Ok(mut guard) = session_normal_geometry().lock() {
                *guard = Some(normal);
            }
        }

        // Restore the saved widget geometry, or create the default portrait one.
        let widget = read_saved_window_state()
            .and_then(|state| state.widget_geometry())
            .unwrap_or_else(|| default_widget_geometry(&window, normal));
        if let Ok(mut guard) = session_widget_geometry().lock() {
            *guard = Some(widget);
        }

        WIDGET_MODE.store(true, Ordering::SeqCst);
        apply_widget_window_flags(&window);
        let _ = window.unmaximize();

        // Persist with the known geometries instead of the still-settling window.
        persist_window_state(normal, Some(widget), true);

        // Entering widget mode: delay, then resize before positioning so the
        // window shrinks around its saved top-left corner.
        apply_geometry_delayed(window.clone(), widget, true);
    } else {
        let normal = session_normal_geometry()
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .or_else(|| read_saved_window_state().map(|state| state.normal_geometry()));

        // Capture the widget geometry before the normal geometry is restored.
        let widget = capture_widget_geometry(&window);
        if let Some(widget) = widget {
            if let Ok(mut guard) = session_widget_geometry().lock() {
                *guard = Some(widget);
            }
        }

        WIDGET_MODE.store(false, Ordering::SeqCst);
        persist_window_state(normal, widget, false);

        apply_widget_window_flags(&window);
        if let Some(normal) = normal {
            // Returning to normal mode: delay, then position before resizing.
            apply_geometry_delayed(window.clone(), normal, false);
        } else {
            apply_fallback_window_geometry(&window);
        }
    }

    Ok(enabled)
}

#[tauri::command]
fn get_widget_mode() -> bool {
    WIDGET_MODE.load(Ordering::SeqCst)
}

/// Toggles always-on-top independently of widget mode. Applies to both normal
/// and widget mode and persists across restarts.
#[tauri::command]
fn set_always_on_top(app: tauri::AppHandle, enabled: bool) -> Result<bool, String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "main window not found".to_string())?;
    apply_always_on_top(&window, enabled);

    // Persist the flag without disturbing the geometry currently in the file.
    persist_window_state(None, None, WIDGET_MODE.load(Ordering::SeqCst));

    Ok(enabled)
}

#[tauri::command]
fn get_always_on_top() -> bool {
    ALWAYS_ON_TOP.load(Ordering::SeqCst)
}

#[tauri::command]
fn read_external_file(path: String) -> Result<String, String> {
    std::fs::read_to_string(&path).map_err(|e| e.to_string())
}

#[tauri::command]
fn write_external_file(path: String, contents: String) -> Result<(), String> {
    let p = std::path::Path::new(&path);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, &contents).map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            use tauri::Manager;
            if let Some(window) = app.get_webview_window("main") {
                apply_initial_window_state(&window);
                let _ = window.show();

                let window_for_close = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { .. } = event {
                        save_window_state(&window_for_close);
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_storage_dir,
            load_latest_state,
            list_history,
            load_history_entry,
            save_snapshot,
            read_external_file,
            write_external_file,
            set_widget_mode,
            get_widget_mode,
            set_always_on_top,
            get_always_on_top,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

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

#[derive(Serialize, Deserialize)]
struct WindowState {
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    maximized: bool,
}

fn window_state_path() -> Result<PathBuf, String> {
    Ok(data_dir()?.join("window-state.json"))
}

fn apply_initial_window_state(window: &tauri::WebviewWindow) {
    if let Ok(path) = window_state_path() {
        if let Ok(raw) = fs::read_to_string(&path) {
            if let Ok(state) = serde_json::from_str::<WindowState>(&raw) {
                if state.maximized {
                    let _ = window.maximize();
                } else {
                    let _ = window.set_size(tauri::Size::Physical(tauri::PhysicalSize {
                        width: state.width,
                        height: state.height,
                    }));
                    let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition {
                        x: state.x,
                        y: state.y,
                    }));
                }
                return;
            }
        }
    }

    // No saved state yet (first launch): size to ~80% of the current monitor, centered.
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

fn save_window_state(window: &tauri::WebviewWindow) {
    let maximized = window.is_maximized().unwrap_or(false);
    let size = window.outer_size().unwrap_or(tauri::PhysicalSize {
        width: 800,
        height: 600,
    });
    let pos = window
        .outer_position()
        .unwrap_or(tauri::PhysicalPosition { x: 0, y: 0 });

    let state = WindowState {
        width: size.width,
        height: size.height,
        x: pos.x,
        y: pos.y,
        maximized,
    };

    if let Ok(path) = window_state_path() {
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(&state) {
            let _ = fs::write(&path, json);
        }
    }
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

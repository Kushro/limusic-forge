//! The main window's size, position and maximized flag in portable mode, kept in
//! `<data>/window.json`.
//!
//! The installed build uses `tauri_plugin_window_state`, but that plugin resolves
//! `app_config_dir()` and creates it at startup whatever file name it is given, which would leave
//! an empty `%APPDATA%\<identifier>` behind a portable copy. This is the small part of it the app
//! needs: geometry is tracked while the window moves, and written when it closes (hidden to the
//! tray included) and when the app exits. Physical pixels, like the plugin.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{PhysicalPosition, PhysicalSize, WebviewWindow, WindowEvent};

const FILE: &str = "window.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Geometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
}

/// The last known geometry, and the file it goes to.
static CURRENT: Mutex<Option<(PathBuf, Geometry)>> = Mutex::new(None);

fn load(file: &Path) -> Option<Geometry> {
    let text = std::fs::read_to_string(file).ok()?;
    serde_json::from_str::<Geometry>(&text).ok().filter(|g| g.width > 0 && g.height > 0)
}

fn store(file: &Path, g: &Geometry) {
    match serde_json::to_string(g) {
        Ok(text) => {
            if let Err(e) = std::fs::write(file, text) {
                tracing::warn!(error = %e, "could not save the window geometry");
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not encode the window geometry"),
    }
}

/// Whether a window at `pos` with `size` would overlap any of `monitors` (each `(x, y, w, h)`).
/// A saved position on a monitor that is gone is dropped, so the window cannot open off screen.
fn on_screen(pos: (i32, i32), size: (u32, u32), monitors: &[(i32, i32, u32, u32)]) -> bool {
    let (x, y) = (pos.0 as i64, pos.1 as i64);
    let (r, b) = (x + size.0 as i64, y + size.1 as i64);
    monitors.iter().any(|&(mx, my, mw, mh)| {
        let (mx, my) = (mx as i64, my as i64);
        x < mx + mw as i64 && r > mx && y < my + mh as i64 && b > my
    })
}

/// Apply the saved geometry to `win` (before it is shown) and start tracking it.
pub fn attach(win: &WebviewWindow, data_dir: &Path) {
    let file = data_dir.join(FILE);
    if let Some(g) = load(&file) {
        let _ = win.set_size(PhysicalSize::new(g.width, g.height));
        let monitors: Vec<_> = win
            .available_monitors()
            .unwrap_or_default()
            .iter()
            .map(|m| (m.position().x, m.position().y, m.size().width, m.size().height))
            .collect();
        if on_screen((g.x, g.y), (g.width, g.height), &monitors) {
            let _ = win.set_position(PhysicalPosition::new(g.x, g.y));
        }
        if g.maximized {
            let _ = win.maximize();
        }
        *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = Some((file, g));
    } else {
        let g = read(win, None);
        *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = g.map(|g| (file, g));
    }

    let w = win.clone();
    win.on_window_event(move |event| match event {
        WindowEvent::Moved(_) | WindowEvent::Resized(_) => track(&w),
        WindowEvent::CloseRequested { .. } | WindowEvent::Destroyed => save(),
        _ => {}
    });
}

/// The window's geometry now. Size and position are only taken from a normal window: a maximized
/// one reports the monitor's, and a minimized one on Windows sits at -32000.
fn read(win: &WebviewWindow, prev: Option<Geometry>) -> Option<Geometry> {
    if win.is_minimized().unwrap_or(false) {
        return prev;
    }
    let maximized = win.is_maximized().unwrap_or(false);
    if maximized {
        return prev.map(|g| Geometry { maximized: true, ..g });
    }
    let size = win.inner_size().ok()?;
    let pos = win.outer_position().ok()?;
    Some(Geometry { x: pos.x, y: pos.y, width: size.width, height: size.height, maximized: false })
}

fn track(win: &WebviewWindow) {
    let mut cur = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((file, prev)) = cur.take() {
        let g = read(win, Some(prev)).unwrap_or(prev);
        *cur = Some((file, g));
    }
}

/// Write the last known geometry. Called on close and from the app's exit.
pub fn save() {
    let cur = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((file, g)) = cur.as_ref() {
        store(file, g);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winstate_round_trips_through_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join(FILE);
        let g = Geometry { x: -10, y: 20, width: 1200, height: 800, maximized: true };
        store(&file, &g);
        assert_eq!(load(&file), Some(g));
    }

    #[test]
    fn winstate_ignores_a_missing_or_broken_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join(FILE);
        assert_eq!(load(&file), None);
        std::fs::write(&file, "{not json").unwrap();
        assert_eq!(load(&file), None);
        std::fs::write(&file, r#"{"x":0,"y":0,"width":0,"height":0,"maximized":false}"#).unwrap();
        assert_eq!(load(&file), None);
    }

    #[test]
    fn winstate_position_must_overlap_a_monitor() {
        let monitors = [(0, 0, 1920, 1080), (1920, 0, 2560, 1440)];
        assert!(on_screen((100, 100), (1200, 800), &monitors));
        assert!(on_screen((3000, 200), (1200, 800), &monitors));
        assert!(!on_screen((5000, 0), (1200, 800), &monitors));
        assert!(!on_screen((-32000, -32000), (160, 28), &monitors));
        assert!(!on_screen((0, 0), (100, 100), &[]));
    }
}

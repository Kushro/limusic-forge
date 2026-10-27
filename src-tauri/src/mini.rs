//! Mini player: a small always-on-top widget that stands in for the main window.
//!
//! It loads the same SPA as the main window — the root layout branches on the window label — so
//! there is no second bundle and no second copy of the playback state: every event this app emits
//! is global (`app.emit`, never `emit_to`), so both webviews are driven by the same stream.
//!
//! Opening it hides the main window; the app keeps running in the tray. Coming back is always
//! [`crate::tray::show_main`] — the widget's restore button, a tray click, the tray menu and a
//! second launch all land there, so they cannot drift apart.
//!
//! ponytail: a second webview costs a second WebKit web process. Resizing the main window in place
//! would be cheaper, but it throws away everything the app has rendered on every toggle. Revisit
//! only if idle memory in mini mode ever matters more than an instant restore.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{
    AppHandle, LogicalSize, Manager, Monitor, PhysicalPosition, PhysicalSize, WebviewUrl,
    WebviewWindow, WebviewWindowBuilder, WindowEvent,
};

use crate::db::Db;
use crate::mini_aspect;
use crate::state::AppState;

pub const LABEL: &str = "mini";

/// Which widget `mini_mode` asks for: the classic pill, or the dynamic 16:9 one that resizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Classic,
    Dynamic,
}

/// What `mini_mode` accepts (`commands::setting_rule`), in [`Mode`]'s order.
pub const MODE_VALUES: [&str; 2] = ["classic", "dynamic"];
const MODE_KEY: &str = "mini_mode";

impl Mode {
    /// Anything but `dynamic`, a missing key included, is the classic widget users already have.
    fn parse(value: Option<&str>) -> Self {
        if value == Some(MODE_VALUES[1]) {
            Mode::Dynamic
        } else {
            Mode::Classic
        }
    }
}

/// The mode of the widget that is up, fixed when it was built. The setting can change under an
/// open widget; this is what the window on screen actually is.
static MODE: Mutex<Mode> = Mutex::new(Mode::Classic);

fn live_mode() -> Mode {
    *MODE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where the dynamic widget puts the lyrics: over the picture, or in a column beside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Lyrics {
    Overlay,
    Side,
}

/// What `mini_dyn_lyrics` accepts (`commands::setting_rule`), in [`Lyrics`]' order.
pub const LYRICS_VALUES: [&str; 2] = ["overlay", "side"];
const LYRICS_KEY: &str = "mini_dyn_lyrics";
/// `"false"` hides the dynamic widget's resting progress line; anything else shows it.
const PROGRESS_KEY: &str = "mini_dyn_progress";

impl Lyrics {
    fn parse(value: Option<&str>) -> Self {
        if value == Some(LYRICS_VALUES[1]) {
            Lyrics::Side
        } else {
            Lyrics::Overlay
        }
    }
}

/// What the widget's page finds in `window.__LIMUSIC_MINI__` before any of its own code runs, so
/// its first paint is already the right layout. Both modes get it; the classic one ignores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct Config {
    mode: Mode,
    progress: bool,
    lyrics: Lyrics,
}

impl Config {
    /// From the stored settings. Anything missing or unreadable is the default.
    fn read(get: impl Fn(&str) -> Option<String>) -> Self {
        Config {
            mode: Mode::parse(get(MODE_KEY).as_deref()),
            progress: get(PROGRESS_KEY).as_deref() != Some("false"),
            lyrics: Lyrics::parse(get(LYRICS_KEY).as_deref()),
        }
    }

    /// The initialization script. Serialised from the parsed values above, never from stored text,
    /// so nothing in the database can end up as script.
    fn script(&self) -> String {
        let json = serde_json::to_string(self).expect("two enums and a bool always serialise");
        format!("window.__LIMUSIC_MINI__ = {json};")
    }
}

/// The dynamic widget's own size and spot, apart from the classic one's so switching modes never
/// mixes them. The width is *logical*, so it means the same on displays of any scale; the height
/// always follows at 16:9. The position is physical `"x,y"`, like [`POS_KEY`].
const DYN_WIDTH_KEY: &str = "mini_dyn_width";
const DYN_POS_KEY: &str = "mini_dyn_position";

/// Logical sizes of the widget, full and compact (#301). Not user-resizable: it's a pill, not a
/// window you arrange, and its own shrink/expand button is the only way between the two.
const FULL: (f64, f64) = (560.0, 180.0);
const COMPACT: (f64, f64) = (320.0, 88.0);
/// `"true"` while the user wants it compact. Sticky: the widget reopens at the size it was left.
const COMPACT_KEY: &str = "mini_compact";
/// Inset from the screen edge the first time it opens.
const MARGIN: f64 = 24.0;
/// Where the user last dragged it, as physical `"x,y"`. Physical because monitor geometry is, and
/// two displays can disagree on scale factor.
const POS_KEY: &str = "mini_position";

/// Build (or re-show) the widget, and hide the main window behind it.
///
/// **Main thread only** — GTK wants window creation there, same rule as the login and cipher
/// webviews. `commands::open_mini` does the hop.
pub fn open(app: &AppHandle) -> Result<(), String> {
    let state = app.try_state::<Arc<AppState>>();
    let get = |key: &str| state.as_ref().and_then(|s| s.db.get_setting(key));
    let config = Config::read(&get);
    let mode = config.mode;
    let mut existing = app.get_webview_window(LABEL);
    // A widget of the other mode can't be turned into this one: the two are built differently.
    // Unreachable today (the setting lives in the main window, hidden while a widget is up), so
    // this only keeps a stale one from being shown as if it were the one asked for.
    let mut replaced = false;
    if existing.is_some() && live_mode() != mode {
        // Remembered first, under the old mode's keys: `live_mode` still names it.
        save_position(app);
        if let Some(w) = existing.take() {
            let _ = w.destroy();
            replaced = true;
        }
    }
    if let Some(w) = existing {
        let _ = w.show();
        let _ = w.set_focus();
    } else {
        // The dynamic widget reopens where it was left, which may be another display than the
        // main window's: its width is measured against the display it will land on.
        let spot = match mode {
            Mode::Classic => None,
            Mode::Dynamic => get(DYN_POS_KEY).as_deref().and_then(parse_pos),
        };
        let monitors = app.available_monitors().unwrap_or_default();
        let display = width_at(&displays(&monitors), spot).or_else(|| display_width(app));
        // The dynamic widget may grow to 60 % of the display; unbounded if there is none to ask.
        let max_w = display.map(mini_aspect::max_width);
        let (w, h) = match mode {
            Mode::Classic => size(app),
            Mode::Dynamic => {
                let w = dyn_width(get(DYN_WIDTH_KEY).as_deref(), display);
                (w, mini_aspect::height_for(w))
            }
        };
        let builder = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html".into()));
        // Portable: the same profile as the main window, inside `data` (paths.rs).
        let builder = match crate::paths::webview_dir() {
            Some(dir) => builder.data_directory(dir),
            None => builder,
        };
        let builder = match mode {
            Mode::Classic => builder.resizable(false),
            // Resizable, but never maximised: without `maximizable(false)` a double click on the
            // drag region maximises it. The limits are 16:9 too, so a clamp keeps the ratio.
            Mode::Dynamic => {
                let min = mini_aspect::MIN_WIDTH;
                let builder = builder
                    .resizable(true)
                    .maximizable(false)
                    .min_inner_size(min, mini_aspect::height_for(min));
                match max_w {
                    Some(max) => builder.max_inner_size(max, mini_aspect::height_for(max)),
                    None => builder,
                }
            }
        };
        let win = builder
            // The layout the page renders first, decided here so it never paints the wrong one.
            .initialization_script(config.script())
            // Distinct from the main window's "LiMusic Forge" so compositor window rules (niri,
            // KDE) can match it, and set here because they only read the title the window is
            // created with (#362). Deliberately untranslated: a localised title breaks the rule on
            // a language switch. The app_id can't differ instead, GTK sets that once per process.
            .title("LiMusic Forge Mini Player")
            .inner_size(w, h)
            .decorations(false)
            .transparent(true)
            .always_on_top(true)
            .skip_taskbar(true)
            // A square WM shadow around a rounded transparent window looks broken.
            .shadow(false)
            // Positioned before it is shown, so it never flashes wherever the WM guessed first.
            .visible(false);
        let win = crate::http::proxied(win).build().map_err(|e| {
            // The old widget's label may still be registered: its teardown finishes later.
            // The main window is still up (it is hidden last), so the user can just retry.
            if replaced {
                format!("the previous mini player is still closing, try again: {e}")
            } else {
                format!("couldn't open the mini player: {e}")
            }
        })?;

        // WebKitGTK's view keeps a 200px minimum height and GTK sizes the window to fit it, so on
        // Linux the 180px widget came out 200 and the compact one couldn't shrink at all. Without
        // the view's own size request, the window's size is the only one left. Synchronous here:
        // this runs on the main thread, so it lands before the window is first shown.
        #[cfg(target_os = "linux")]
        let _ = win.with_webview(|wv| {
            use gtk::prelude::WidgetExt;
            wv.inner().set_size_request(1, 1);
        });
        if mode == Mode::Dynamic {
            mini_aspect::set_limits(mini_aspect::MIN_WIDTH, max_w.unwrap_or(f64::MAX));
            mini_aspect::attach(&win);
            follow_display(&win);
        }
        *MODE.lock().unwrap_or_else(|e| e.into_inner()) = mode;
        if let Some(p) = placement(app, &win, mode, (w, h)) {
            let _ = win.set_position(p);
        }
        if mode == Mode::Dynamic {
            // Placed while hidden, possibly on a display of another scale than it was built on.
            mini_aspect::refresh_scale(&win);
        }
        // Same treatment as the main window: this is a second web process, and it needs the media
        // and 3D stacks even less than the app does. Media only while it shows the music video
        // (`mini_video`, which counts only on top of `music_videos`): a `<video>` element, never
        // mpv's picture. Read once, here: the settings that change it live in the main window,
        // which is hidden while this one is up.
        #[cfg(target_os = "linux")]
        {
            let media = app.try_state::<Arc<AppState>>().is_some_and(|s| {
                let on = |k: &str| s.db.get_setting(k).as_deref() == Some("true");
                on("music_videos") && on("mini_video")
            });
            crate::tune_webview_labelled(app, LABEL, media);
        }
        let _ = win.show();
        let _ = win.set_focus();
    }
    // Last, and only once the widget is actually up: a failed build must not leave the app with
    // no window at all, reachable only from the tray.
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.hide();
        crate::tray::set_main_visible(app, false);
    }
    Ok(())
}

/// Switch the widget between its full and compact size, and remember the choice. The widget's
/// layout follows its window's width, so the resize is the whole switch: there is no second copy
/// of the mode in the webview to disagree with this one.
pub fn set_compact(app: &AppHandle, compact: bool) -> Result<(), String> {
    let win = app.get_webview_window(LABEL).ok_or("the mini player isn't open")?;
    if live_mode() == Mode::Dynamic {
        return Err("the dynamic mini player has no compact size".into());
    }
    resize_anchored(&win, if compact { COMPACT } else { FULL })?;
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        state.db.set_setting(COMPACT_KEY, if compact { "true" } else { "false" });
    }
    Ok(())
}

/// The dynamic widget's own reset button: back to the size it first opened at, now, held against
/// the screen edges it is parked near, and remembered.
pub fn reset_size(app: &AppHandle) -> Result<(), String> {
    let win = app.get_webview_window(LABEL).ok_or("the mini player isn't open")?;
    if live_mode() != Mode::Dynamic {
        return Err("only the dynamic mini player resizes".into());
    }
    // A display under 800 wide can't take the default; the limits say how wide it may be.
    let w = mini_aspect::DEFAULT_WIDTH.min(mini_aspect::max_limit()).max(mini_aspect::MIN_WIDTH);
    resize_anchored(&win, (w, mini_aspect::height_for(w)))?;
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        state.db.set_setting(DYN_WIDTH_KEY, &format_width(w));
    }
    Ok(())
}

/// Settings' reset for the dynamic widget: forget its size and spot, so it next opens at the
/// default size in the bottom-right corner. The classic widget's keys stay as they are.
pub fn reset_dynamic(db: &Db) {
    db.delete_setting(DYN_WIDTH_KEY);
    db.delete_setting(DYN_POS_KEY);
}

/// Resize `win` to the logical `(w, h)`, keeping it against the screen edges it is near.
fn resize_anchored(win: &WebviewWindow, (w, h): (f64, f64)) -> Result<(), String> {
    let scale = win.scale_factor().map_err(|e| e.to_string())?;
    let new = PhysicalSize::new((w * scale).round() as u32, (h * scale).round() as u32);
    // Read before the resize: afterwards the old size may already be gone.
    let to = match (win.outer_position(), win.outer_size(), win.current_monitor()) {
        (Ok(pos), Ok(old), Ok(Some(m))) => Some(anchored(pos, old, new, *m.position(), *m.size())),
        _ => None,
    };
    win.set_size(new).map_err(|e| e.to_string())?;
    // A no-op on Wayland, where the compositor keeps the top-left corner where it was.
    if let Some(p) = to {
        let _ = win.set_position(p);
    }
    Ok(())
}

/// The dynamic widget's logical width to open at: the saved one if readable, else the default,
/// within `[MIN_WIDTH, 60 %]` of the display (`None`: no display to ask, no upper bound).
fn dyn_width(saved: Option<&str>, display: Option<f64>) -> f64 {
    let max = display.map_or(f64::MAX, mini_aspect::max_width);
    saved
        .and_then(parse_width)
        .unwrap_or(mini_aspect::DEFAULT_WIDTH)
        .clamp(mini_aspect::MIN_WIDTH, max)
}

/// A logical width as [`format_width`] wrote it.
fn parse_width(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|w| w.is_finite() && *w > 0.0)
}

/// Whole logical pixels: a fractional width is only scaling noise.
fn format_width(w: f64) -> String {
    format!("{}", w.round() as i64)
}

/// Keep the dynamic widget's maximum at 60 % of the display it is on now, not the one it opened
/// on: dragged to a smaller display, it shrinks to fit; to a bigger one, it may grow.
fn follow_display(win: &WebviewWindow) {
    // The handle, not the window: a window holding a clone of itself in its own handler is a
    // cycle that outlives every close.
    let app = win.app_handle().clone();
    win.on_window_event(move |event| match event {
        WindowEvent::Moved(_) | WindowEvent::ScaleFactorChanged { .. } => {
            if let Some(w) = app.get_webview_window(LABEL) {
                refresh_limits(&w);
            }
        }
        _ => {}
    });
}

fn refresh_limits(win: &WebviewWindow) {
    mini_aspect::refresh_scale(win);
    let Ok(Some(m)) = win.current_monitor() else { return };
    let max = mini_aspect::max_width(m.size().width as f64 / m.scale_factor());
    // Moved fires all through a drag; only another display changes anything. The limits go in
    // first, so whatever the new maximum makes the window do is corrected against them.
    if max == mini_aspect::max_limit() {
        return;
    }
    mini_aspect::set_limits(mini_aspect::MIN_WIDTH, max);
    let _ = win.set_max_size(Some(LogicalSize::new(max, mini_aspect::height_for(max))));
    #[cfg(target_os = "linux")]
    mini_aspect::gtk_hints(win);
}

/// The size to open at: whichever the user left it at.
fn size(app: &AppHandle) -> (f64, f64) {
    let compact = app
        .try_state::<Arc<AppState>>()
        .and_then(|s| s.db.get_setting(COMPACT_KEY))
        .is_some_and(|v| v == "true");
    if compact {
        COMPACT
    } else {
        FULL
    }
}

/// Logical width of the display the main window is on, which is where [`bottom_right`] puts the
/// widget.
fn display_width(app: &AppHandle) -> Option<f64> {
    let main = app.get_webview_window("main")?;
    let m =
        main.current_monitor().ok().flatten().or_else(|| main.primary_monitor().ok().flatten())?;
    Some(m.size().width as f64 / m.scale_factor())
}

/// Top-left for a widget going from `old` to `new` size, so it stays against the screen edges it
/// is parked near: shrunk in the bottom-right corner, it stays in that corner instead of leaving a
/// gap below and to the right. Decided per axis by which half of the display its centre is in, so
/// shrinking and growing back always returns it to the same spot.
fn anchored(
    pos: PhysicalPosition<i32>,
    old: PhysicalSize<u32>,
    new: PhysicalSize<u32>,
    origin: PhysicalPosition<i32>,
    display: PhysicalSize<u32>,
) -> PhysicalPosition<i32> {
    let axis = |p: i32, old: u32, new: u32, start: i32, len: u32| {
        let far_half = 2 * p + old as i32 > 2 * start + len as i32;
        if far_half {
            p + old as i32 - new as i32
        } else {
            p
        }
    };
    PhysicalPosition::new(
        axis(pos.x, old.width, new.width, origin.x, display.width),
        axis(pos.y, old.height, new.height, origin.y, display.height),
    )
}

/// Remember where the widget currently sits. No-op when it isn't up. Its own function because
/// quitting from the tray is a way down that never reaches [`close`].
///
/// The dynamic widget keeps its own spot, and its width too, since the user sizes it.
pub fn save_position(app: &AppHandle) {
    let Some(w) = app.get_webview_window(LABEL) else { return };
    let Some(state) = app.try_state::<Arc<AppState>>() else { return };
    match live_mode() {
        Mode::Classic => {
            if let Ok(p) = w.outer_position() {
                state.db.set_setting(POS_KEY, &format_pos(p));
            }
        }
        Mode::Dynamic => {
            // A minimised window reports a parked spot and a size worth nothing.
            if w.is_minimized().unwrap_or(false) {
                return;
            }
            if let Ok(p) = w.outer_position() {
                state.db.set_setting(DYN_POS_KEY, &format_pos(p));
            }
            if let (Ok(size), Ok(scale)) = (w.outer_size(), w.scale_factor()) {
                if size.width > 0 && scale > 0.0 {
                    state.db.set_setting(DYN_WIDTH_KEY, &format_width(size.width as f64 / scale));
                }
            }
        }
    }
}

/// Take the widget down, remembering where it ended up. Callable from any thread; bringing the
/// main window back is [`crate::tray::show_main`]'s job, which is what calls this.
pub fn close(app: &AppHandle) {
    if app.get_webview_window(LABEL).is_none() {
        return;
    }
    save_position(app);
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(w) = handle.get_webview_window(LABEL) {
            let _ = w.destroy();
        }
    });
}

/// Where to put it: the last position if that spot still exists (a display can be unplugged
/// between sessions), otherwise the bottom-right of whichever display the app is on. The dynamic
/// widget, whose size the user picks, is also pulled back inside that display's work area.
fn placement(
    app: &AppHandle,
    win: &WebviewWindow,
    mode: Mode,
    size: (f64, f64),
) -> Option<PhysicalPosition<i32>> {
    let saved = |key: &str| {
        app.try_state::<Arc<AppState>>()
            .and_then(|s| s.db.get_setting(key))
            .and_then(|v| parse_pos(&v))
    };
    let spot = match mode {
        Mode::Classic => {
            saved(POS_KEY).filter(|p| on_a_display(win, *p, win.outer_size().unwrap_or_default()))
        }
        Mode::Dynamic => saved(DYN_POS_KEY).and_then(|p| {
            let monitors = win.available_monitors().unwrap_or_default();
            inside_a_display(&displays(&monitors), p, size)
        }),
    };
    spot.or_else(|| bottom_right(app, win, size))
}

/// `"x,y"` in physical pixels, as [`format_pos`] wrote it.
fn parse_pos(s: &str) -> Option<PhysicalPosition<i32>> {
    let (x, y) = s.split_once(',')?;
    Some(PhysicalPosition::new(x.trim().parse().ok()?, y.trim().parse().ok()?))
}

fn format_pos(p: PhysicalPosition<i32>) -> String {
    format!("{},{}", p.x, p.y)
}

/// A connected display as placement needs it, physical pixels.
#[derive(Clone, Copy, Debug)]
struct Display {
    origin: PhysicalPosition<i32>,
    size: PhysicalSize<u32>,
    work_origin: PhysicalPosition<i32>,
    work_size: PhysicalSize<u32>,
    scale: f64,
}

fn displays(monitors: &[Monitor]) -> Vec<Display> {
    monitors
        .iter()
        .map(|m| Display {
            origin: *m.position(),
            size: *m.size(),
            work_origin: m.work_area().position,
            work_size: m.work_area().size,
            scale: m.scale_factor(),
        })
        .collect()
}

/// Logical width of the display the saved spot `p` is on, which is where [`inside_a_display`]
/// puts the widget. `None` with no spot, or one on no display, which leaves it to the main
/// window's display like [`bottom_right`].
fn width_at(displays: &[Display], p: Option<PhysicalPosition<i32>>) -> Option<f64> {
    let p = p?;
    let d = displays.iter().find(|d| contains(d.origin, d.size, p))?;
    Some(d.size.width as f64 / d.scale)
}

/// The top-left for a widget of logical `(w, h)` saved at `p`: on the display that corner is on,
/// moved just enough for the whole widget to fit in its work area. `None` when `p` is on no
/// display at all, which leaves it to [`bottom_right`].
fn inside_a_display(
    displays: &[Display],
    p: PhysicalPosition<i32>,
    (w, h): (f64, f64),
) -> Option<PhysicalPosition<i32>> {
    let d = displays.iter().find(|d| contains(d.origin, d.size, p))?;
    let px = |logical: f64| (logical * d.scale).round() as u32;
    Some(fit_into(p, PhysicalSize::new(px(w), px(h)), d.work_origin, d.work_size))
}

/// `pos` moved the least so a `size` rect starting there lies inside the `area` rect. Too big to
/// fit, its top-left edge wins: that is where the controls are.
fn fit_into(
    pos: PhysicalPosition<i32>,
    size: PhysicalSize<u32>,
    origin: PhysicalPosition<i32>,
    area: PhysicalSize<u32>,
) -> PhysicalPosition<i32> {
    let axis = |p: i32, len: u32, start: i32, avail: u32| {
        p.min(start + avail as i32 - len as i32).max(start)
    };
    PhysicalPosition::new(
        axis(pos.x, size.width, origin.x, area.width),
        axis(pos.y, size.height, origin.y, area.height),
    )
}

/// Is a window at `p`, this size, on a display that is currently connected? Its middle has to be.
/// A corner is not enough: one can sit on a display with only the invisible resize border showing,
/// which is how the main window came back off-screen after a monitor layout change (#459).
pub(crate) fn on_a_display(
    win: &WebviewWindow,
    p: PhysicalPosition<i32>,
    size: PhysicalSize<u32>,
) -> bool {
    let mid = middle(p, size);
    win.available_monitors()
        .is_ok_and(|monitors| monitors.iter().any(|m| contains(*m.position(), *m.size(), mid)))
}

fn middle(p: PhysicalPosition<i32>, size: PhysicalSize<u32>) -> PhysicalPosition<i32> {
    PhysicalPosition::new(p.x + size.width as i32 / 2, p.y + size.height as i32 / 2)
}

/// Bottom-right of the display the main window is on, inside its *work area* so a taskbar or dock
/// doesn't end up sitting on top of the widget.
fn bottom_right(
    app: &AppHandle,
    win: &WebviewWindow,
    (w, h): (f64, f64),
) -> Option<PhysicalPosition<i32>> {
    let anchor = app.get_webview_window("main").unwrap_or_else(|| win.clone());
    let m = anchor
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| anchor.primary_monitor().ok().flatten())?;
    let area = m.work_area();
    let px = |logical: f64| (logical * m.scale_factor()).round() as i32;
    Some(PhysicalPosition::new(
        area.position.x + area.size.width as i32 - px(w + MARGIN),
        area.position.y + area.size.height as i32 - px(h + MARGIN),
    ))
}

/// Point-in-rect, physical pixels. Its own function because a second monitor placed left of or
/// above the primary has a negative origin, which is exactly the case that goes wrong.
fn contains(
    origin: PhysicalPosition<i32>,
    size: PhysicalSize<u32>,
    p: PhysicalPosition<i32>,
) -> bool {
    (origin.x..origin.x + size.width as i32).contains(&p.x)
        && (origin.y..origin.y + size.height as i32).contains(&p.y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri::{PhysicalPosition, PhysicalSize};

    #[test]
    fn script_is_built_from_the_parsed_settings() {
        let stored = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(key, _)| *key == k).map(|(_, v)| v.to_string())
        };
        assert_eq!(
            Config::read(stored(&[])).script(),
            r#"window.__LIMUSIC_MINI__ = {"mode":"classic","progress":true,"lyrics":"overlay"};"#
        );
        let dynamic = stored(&[
            ("mini_mode", "dynamic"),
            ("mini_dyn_progress", "false"),
            ("mini_dyn_lyrics", "side"),
        ]);
        assert_eq!(
            Config::read(dynamic).script(),
            r#"window.__LIMUSIC_MINI__ = {"mode":"dynamic","progress":false,"lyrics":"side"};"#
        );
        // Junk, script included, never reaches the page: each value falls back to its default.
        let junk = stored(&[
            ("mini_mode", "\"};alert(1);//"),
            ("mini_dyn_progress", "no"),
            ("mini_dyn_lyrics", "</script>"),
        ]);
        assert_eq!(
            Config::read(junk),
            Config { mode: Mode::Classic, progress: true, lyrics: Lyrics::Overlay }
        );
    }

    #[test]
    fn dynamic_size_and_spot_round_trip() {
        for w in [320.0, 480.0, 1152.0] {
            assert_eq!(parse_width(&format_width(w)), Some(w));
        }
        // Logical width from a fractional scale: whole pixels.
        assert_eq!(format_width(1500.0 / 1.25), "1200");
        assert_eq!(format_width(641.0 / 1.5), "427");
        for junk in ["", "wide", "-480", "0", "inf", "NaN"] {
            assert_eq!(parse_width(junk), None, "{junk:?}");
        }
        for p in [PhysicalPosition::new(1416, 786), PhysicalPosition::new(-1800, -200)] {
            assert_eq!(parse_pos(&format_pos(p)), Some(p));
        }
    }

    #[test]
    fn dynamic_width_is_clamped_to_the_display() {
        let fhd = Some(1920.0);
        assert_eq!(dyn_width(None, fhd), 480.0);
        assert_eq!(dyn_width(Some("garbage"), fhd), 480.0);
        assert_eq!(dyn_width(Some("800"), fhd), 800.0);
        assert_eq!(dyn_width(Some("100"), fhd), 320.0);
        // 60 % of the display, whatever was saved on a bigger one.
        assert_eq!(dyn_width(Some("3000"), fhd), 1152.0);
        assert_eq!(dyn_width(Some("1536"), Some(1366.0)), 819.0);
        // A tiny display still gets the minimum, the default included.
        assert_eq!(dyn_width(None, Some(400.0)), 320.0);
        // No display to ask: no upper bound, still the minimum.
        assert_eq!(dyn_width(Some("3000"), None), 3000.0);
        assert_eq!(dyn_width(Some("10"), None), 320.0);
    }

    #[test]
    fn dynamic_width_follows_the_display_it_reopens_on() {
        let at = |x, y| PhysicalPosition::new(x, y);
        let display = |origin, w, h, scale| Display {
            origin,
            size: PhysicalSize::new(w, h),
            work_origin: origin,
            work_size: PhysicalSize::new(w, h),
            scale,
        };
        // A 1920x1080 primary, and a 3840x2160 display at 2.0 to its right: 1920 logical too, but
        // a 1920x1200 one at 1.25 to its left is only 1536 logical.
        let primary = display(at(0, 0), 1920, 1080, 1.0);
        let right = display(at(1920, 0), 3840, 2160, 2.0);
        let left = display(at(-1920, 0), 1920, 1200, 1.25);
        let all = [primary, right, left];

        assert_eq!(width_at(&all, Some(at(1416, 746))), Some(1920.0));
        assert_eq!(width_at(&all, Some(at(4000, 1500))), Some(1920.0));
        let narrow = width_at(&all, Some(at(-800, 1000)));
        assert_eq!(narrow, Some(1536.0));
        assert_eq!(narrow.map(mini_aspect::max_width), Some(921.0));
        // Saved wide on the primary, reopened on the left display: held to that display's 60 %.
        assert_eq!(dyn_width(Some("1152"), narrow), 921.0);
        assert_eq!(dyn_width(Some("1152"), width_at(&all, Some(at(100, 100)))), 1152.0);
        // No spot, or one on a display unplugged since: the main window's display decides.
        assert_eq!(width_at(&all, None), None);
        assert_eq!(width_at(&all, Some(at(9000, 0))), None);
        assert_eq!(width_at(&[], Some(at(0, 0))), None);
    }

    #[test]
    fn dynamic_spot_is_kept_inside_its_display() {
        let at = |x, y| PhysicalPosition::new(x, y);
        // 1920x1080 primary with a 40 px taskbar, a 2560x1440 display at 1.5 to its left.
        let primary = Display {
            origin: at(0, 0),
            size: PhysicalSize::new(1920, 1080),
            work_origin: at(0, 0),
            work_size: PhysicalSize::new(1920, 1040),
            scale: 1.0,
        };
        let left = Display {
            origin: at(-2560, 0),
            size: PhysicalSize::new(2560, 1440),
            work_origin: at(-2560, 0),
            work_size: PhysicalSize::new(2560, 1440),
            scale: 1.5,
        };
        let all = [primary, left];
        let size = (480.0, 270.0);

        // Fits: untouched.
        assert_eq!(inside_a_display(&all, at(1416, 746), size), Some(at(1416, 746)));
        // Over the taskbar and the right edge: pulled in, both axes.
        assert_eq!(inside_a_display(&all, at(1800, 1000), size), Some(at(1440, 770)));
        // On the left display, measured at its own scale: 720x405 physical.
        assert_eq!(inside_a_display(&all, at(-100, 1300), size), Some(at(-720, 1035)));
        // Off every display (one unplugged since): left to bottom_right.
        assert_eq!(inside_a_display(&all, at(5000, 200), size), None);
        assert_eq!(inside_a_display(&[], at(0, 0), size), None);

        // Bigger than the area: the top-left edge wins.
        let tiny = (PhysicalPosition::new(0, 0), PhysicalSize::new(300u32, 100));
        assert_eq!(fit_into(at(50, 50), PhysicalSize::new(480, 270), tiny.0, tiny.1), at(0, 0));
    }

    #[test]
    fn dynamic_reset_forgets_only_the_dynamic_keys() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        for (key, value) in [
            (DYN_WIDTH_KEY, "800"),
            (DYN_POS_KEY, "100,100"),
            (POS_KEY, "1336,876"),
            (COMPACT_KEY, "true"),
            (MODE_KEY, "dynamic"),
        ] {
            db.set_setting(key, value);
        }
        reset_dynamic(&db);
        assert_eq!(db.get_setting(DYN_WIDTH_KEY), None);
        assert_eq!(db.get_setting(DYN_POS_KEY), None);
        assert_eq!(db.get_setting(POS_KEY).as_deref(), Some("1336,876"));
        assert_eq!(db.get_setting(COMPACT_KEY).as_deref(), Some("true"));
        assert_eq!(db.get_setting(MODE_KEY).as_deref(), Some("dynamic"));
        // Nothing to forget is fine too.
        reset_dynamic(&db);
    }

    #[test]
    fn mode_is_classic_unless_it_says_dynamic() {
        assert_eq!(Mode::parse(Some("dynamic")), Mode::Dynamic);
        assert_eq!(Mode::parse(Some("classic")), Mode::Classic);
        for junk in [None, Some(""), Some("Dynamic"), Some("compact")] {
            assert_eq!(Mode::parse(junk), Mode::Classic, "{junk:?}");
        }
    }

    #[test]
    fn parses_what_close_wrote() {
        assert_eq!(parse_pos("120,64"), Some(PhysicalPosition::new(120, 64)));
        // A monitor left of the primary gives negative coordinates.
        assert_eq!(parse_pos("-1800,-200"), Some(PhysicalPosition::new(-1800, -200)));
        assert_eq!(parse_pos("garbage"), None);
        assert_eq!(parse_pos("12,"), None);
    }

    #[test]
    fn point_lands_on_the_right_display() {
        let primary = (PhysicalPosition::new(0, 0), PhysicalSize::new(1920u32, 1080));
        // Second monitor to the *left* of the primary: origin is negative.
        let left = (PhysicalPosition::new(-1920, 0), PhysicalSize::new(1920u32, 1080));

        assert!(contains(primary.0, primary.1, PhysicalPosition::new(1300, 800)));
        assert!(!contains(primary.0, primary.1, PhysicalPosition::new(-500, 800)));
        assert!(contains(left.0, left.1, PhysicalPosition::new(-500, 800)));
        // Right/bottom edges are exclusive — that pixel belongs to the next display.
        assert!(!contains(primary.0, primary.1, PhysicalPosition::new(1920, 500)));
        assert!(!contains(primary.0, primary.1, PhysicalPosition::new(500, 1080)));
    }

    #[test]
    fn a_window_counts_by_its_middle() {
        // #459: the window was saved maximized on a laptop display that sat right of monitor A,
        // then restored with A as the only display at that height.
        let a = (PhysicalPosition::new(0, 0), PhysicalSize::new(1920u32, 1080));
        let (at, size) = (PhysicalPosition::new(1911, -9), PhysicalSize::new(1900u32, 1008));
        // Its bottom-left corner is inside A, which is all the window-state plugin checks...
        assert!(contains(a.0, a.1, PhysicalPosition::new(at.x, at.y + size.height as i32)));
        // ...but its middle is on nothing.
        assert!(!contains(a.0, a.1, middle(at, size)));
        // Maximized on A, the frame overhangs every edge by its border and still counts.
        let maxed = middle(PhysicalPosition::new(-9, -9), PhysicalSize::new(1938, 1098));
        assert!(contains(a.0, a.1, maxed));
    }

    #[test]
    fn resizing_keeps_the_nearest_screen_edges() {
        let full = PhysicalSize::new(560u32, 180);
        let small = PhysicalSize::new(320u32, 88);
        let at = |x, y| PhysicalPosition::new(x, y);
        let shrink = |p, origin, display| anchored(p, full, small, origin, display);
        let grow = |p, origin, display| anchored(p, small, full, origin, display);
        let fhd = PhysicalSize::new(1920u32, 1080);

        // Bottom-right (the default spot): right and bottom edges stay put.
        assert_eq!(shrink(at(1336, 876), at(0, 0), fhd), at(1576, 968));
        // Top-left: nothing moves.
        assert_eq!(shrink(at(24, 24), at(0, 0), fhd), at(24, 24));
        // Top-right: only the right edge is held.
        assert_eq!(shrink(at(1336, 24), at(0, 0), fhd), at(1576, 24));
        // Same corner on a display left of the primary, where the origin is negative.
        assert_eq!(shrink(at(-584, 876), at(-1920, 0), fhd), at(-344, 968));

        // Shrinking and growing back is a round trip, wherever it sits.
        for (p, origin) in
            [(at(1336, 876), at(0, 0)), (at(24, 24), at(0, 0)), (at(900, 300), at(0, 0))]
        {
            assert_eq!(grow(shrink(p, origin, fhd), origin, fhd), p);
        }
    }
}

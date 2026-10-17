//! Keeps the dynamic mini player at 16:9 while it is resized (mini.rs).
//!
//! Neither Tauri 2.11 nor tao 0.35 can lock a window's aspect ratio, so each platform gets the
//! closest thing it has:
//!
//! - **Windows**: a subclass on the widget's HWND rewrites the drag rectangle in `WM_SIZING`,
//!   before the window takes it, so a resize never shows a wrong frame. The native borders and
//!   `startResizeDragging` both end up in that same modal loop. `WM_WINDOWPOSCHANGING` catches what
//!   resizes without it (Snap layouts, Win+arrows, a DPI change).
//! - **Linux**: GTK geometry hints with a fixed aspect, which X11 window managers honour.
//! - **Everywhere but Windows**: a correction shortly after the resize settles, because Wayland
//!   compositors may ignore the hints and macOS gets nothing else. If the subclass cannot be
//!   installed, Windows falls back to this as well.
//!
//! The geometry is [`constrain`], pure and tested; the platform code only feeds it rectangles.

use std::sync::atomic::{AtomicU64, Ordering};

use tauri::WebviewWindow;

/// Logical width the widget can't shrink below; 180 tall.
pub const MIN_WIDTH: f64 = 320.0;
/// Logical width it opens at the first time; 270 tall.
pub const DEFAULT_WIDTH: f64 = 480.0;
/// The widest it may get, as a share of its display's logical width.
const MAX_SHARE: f64 = 0.6;

/// Current logical width limits, as `f64` bits. Atomics because the Win32 subclass proc reads
/// them, and whoever learns the widget moved to another display rewrites them.
static MIN_W: AtomicU64 = AtomicU64::new(0);
static MAX_W: AtomicU64 = AtomicU64::new(0);

/// The 16:9 height for a logical width.
pub fn height_for(width: f64) -> f64 {
    (width * 9.0 / 16.0).round()
}

/// The widest the widget may be on a display `monitor_width` logical pixels wide: 60 % of it, but
/// never below the minimum, so a tiny display still gets a usable widget.
pub fn max_width(monitor_width: f64) -> f64 {
    (monitor_width * MAX_SHARE).floor().max(MIN_WIDTH)
}

/// Set the logical width limits every correction clamps to. Heights follow at 16:9.
pub fn set_limits(min_width: f64, max_width: f64) {
    MIN_W.store(min_width.to_bits(), Ordering::Relaxed);
    MAX_W.store(max_width.max(min_width).to_bits(), Ordering::Relaxed);
}

/// The limits in physical pixels at `scale`: the minimum rounded up and the maximum down, so the
/// result never falls outside the window's own `min_inner_size`/`max_inner_size`.
fn physical_limits(scale: f64) -> (i32, i32) {
    let load = |a: &AtomicU64| f64::from_bits(a.load(Ordering::Relaxed));
    let min = load(&MIN_W).max(1.0);
    let max = load(&MAX_W).max(min);
    let min = (min * scale).ceil() as i32;
    (min, ((max * scale).floor() as i32).max(min))
}

/// A rectangle in physical pixels, edges as Win32 `RECT` has them (right and bottom exclusive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// The border or corner being dragged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// `round(w * 9 / 16)` for a positive width, in integers.
fn height_of(w: i32) -> i32 {
    (w * 9 + 8) / 16
}

/// `round(h * 16 / 9)` for a positive height, in integers.
fn width_of(h: i32) -> i32 {
    (h * 16 + 4) / 9
}

/// `rect` as the user dragged it by `edge`, turned 16:9 and clamped to `[min_w, max_w]` wide.
///
/// The edge or corner opposite the dragged one never moves. Dragging a side, the width decides and
/// the bottom border follows; dragging the top or bottom, the height decides and the right border
/// follows; dragging a corner, the width decides and the dragged corner moves along both axes.
pub fn constrain(edge: Edge, rect: Rect, min_w: i32, max_w: i32) -> Rect {
    use Edge::*;
    let w = match edge {
        Top | Bottom => width_of(rect.bottom - rect.top),
        _ => rect.right - rect.left,
    }
    .clamp(min_w, max_w.max(min_w));
    let h = height_of(w);
    let (left, right) = match edge {
        Left | TopLeft | BottomLeft => (rect.right - w, rect.right),
        _ => (rect.left, rect.left + w),
    };
    let (top, bottom) = match edge {
        Top | TopLeft | TopRight => (rect.bottom - h, rect.bottom),
        _ => (rect.top, rect.top + h),
    };
    Rect { left, top, right, bottom }
}

/// The 16:9 size for an inner size the platform settled on by itself, or `None` when it is already
/// within a pixel. Keeps the top-left corner, which is all a size change can do.
fn settled(width: u32, height: u32, scale: f64) -> Option<(u32, u32)> {
    let (min, max) = physical_limits(scale);
    let (w, h) = (width as i32, height as i32);
    let out = constrain(Edge::Right, Rect { left: 0, top: 0, right: w, bottom: h }, min, max);
    let off = (out.right - w).abs() > 1 || (out.bottom - h).abs() > 1;
    off.then_some((out.right as u32, out.bottom as u32))
}

/// Hold `win` at 16:9 from now on. Call once, on the main thread, right after building it, and
/// after [`set_limits`]. Never fails: whatever cannot be installed is logged and replaced by the
/// correction after the resize.
pub fn attach(win: &WebviewWindow) {
    #[cfg(windows)]
    {
        let scale = win.scale_factor().unwrap_or(1.0);
        match win.hwnd() {
            Ok(hwnd) if win32::attach(hwnd, scale) => return,
            Ok(_) => tracing::warn!("mini 16:9: SetWindowSubclass failed, correcting after resize"),
            Err(e) => tracing::warn!(error = %e, "mini 16:9: no HWND, correcting after resize"),
        }
    }
    #[cfg(target_os = "linux")]
    gtk_hints(win);
    after_resize(win);
}

/// Fix the size once a resize has been still for a moment. Each `Resized` bumps a generation; a
/// correction only runs if no newer one arrived while it waited, so a drag in progress is left
/// alone and gets one correction at the end. The correction's own `Resized` is already 16:9, so it
/// stops there.
fn after_resize(win: &WebviewWindow) {
    use std::sync::Arc;
    use tauri::Manager;
    const SETTLE: std::time::Duration = std::time::Duration::from_millis(150);

    let generation = Arc::new(AtomicU64::new(0));
    let app = win.app_handle().clone();
    let label = win.label().to_owned();
    win.on_window_event(move |event| {
        let tauri::WindowEvent::Resized(size) = event else { return };
        let (width, height) = (size.width, size.height);
        let mine = generation.fetch_add(1, Ordering::Relaxed) + 1;
        let (generation, app, label) = (generation.clone(), app.clone(), label.clone());
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(SETTLE).await;
            if generation.load(Ordering::Relaxed) != mine {
                return;
            }
            let Some(win) = app.get_webview_window(&label) else { return };
            // Minimised windows report a zero size; there is nothing to fix there.
            if width == 0 || height == 0 || win.is_minimized().unwrap_or(false) {
                return;
            }
            let scale = win.scale_factor().unwrap_or(1.0);
            if let Some((w, h)) = settled(width, height, scale) {
                let _ = win.set_size(tauri::PhysicalSize::new(w, h));
            }
        });
    });
}

/// Ask the window manager for the aspect itself. GTK takes logical pixels, and the hints replace
/// the ones tao set, so the size limits go in again with it.
#[cfg(target_os = "linux")]
fn gtk_hints(win: &WebviewWindow) {
    use gtk::gdk::{Geometry, Gravity, WindowHints};
    use gtk::prelude::GtkWindowExt;

    let Ok(window) = win.gtk_window() else {
        tracing::warn!("mini 16:9: no GTK window for the aspect hints");
        return;
    };
    let load = |a: &AtomicU64| f64::from_bits(a.load(Ordering::Relaxed));
    let (min, max) = (load(&MIN_W), load(&MAX_W));
    let ratio = 16.0 / 9.0;
    let geometry = Geometry::new(
        min as i32,
        height_for(min) as i32,
        max as i32,
        height_for(max) as i32,
        0,
        0,
        0,
        0,
        ratio,
        ratio,
        Gravity::NorthWest,
    );
    window.set_geometry_hints(
        None::<&gtk::Widget>,
        Some(&geometry),
        WindowHints::MIN_SIZE | WindowHints::MAX_SIZE | WindowHints::ASPECT,
    );
}

#[cfg(windows)]
mod win32 {
    use std::sync::atomic::{AtomicU32, Ordering};

    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClientRect, GetWindowRect, IsIconic, SWP_NOSIZE, WINDOWPOS, WMSZ_BOTTOM,
        WMSZ_BOTTOMLEFT, WMSZ_LEFT, WMSZ_RIGHT, WMSZ_TOP, WMSZ_TOPLEFT, WMSZ_TOPRIGHT,
        WM_DPICHANGED, WM_NCDESTROY, WM_SIZING, WM_WINDOWPOSCHANGING,
    };

    use super::{constrain, physical_limits, Edge, Rect};

    const SUBCLASS_ID: usize = 0x16_09_a5_9e;
    /// The widget's DPI, kept current from `WM_DPICHANGED` so the limits convert at the scale
    /// the window is on now, before tao resizes it for the new display.
    static DPI: AtomicU32 = AtomicU32::new(96);

    /// Install the subclass. `false` if Windows refused it.
    pub fn attach(hwnd: HWND, scale: f64) -> bool {
        DPI.store((scale * 96.0).round() as u32, Ordering::Relaxed);
        // Removed again in WM_NCDESTROY: the widget is destroyed on every close.
        unsafe { SetWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID, 0) }.as_bool()
    }

    fn limits() -> (i32, i32) {
        physical_limits(DPI.load(Ordering::Relaxed) as f64 / 96.0)
    }

    /// How much wider and taller the window is than its client area. Zero for this undecorated
    /// widget today, but the ratio is about the page, so measure rather than assume.
    fn frame(hwnd: HWND) -> (i32, i32) {
        let (mut outer, mut inner) = (RECT::default(), RECT::default());
        let ok = unsafe { GetWindowRect(hwnd, &mut outer).is_ok() }
            && unsafe { GetClientRect(hwnd, &mut inner).is_ok() };
        if !ok {
            return (0, 0);
        }
        let dw = (outer.right - outer.left) - (inner.right - inner.left);
        let dh = (outer.bottom - outer.top) - (inner.bottom - inner.top);
        (dw.max(0), dh.max(0))
    }

    fn edge(wmsz: u32) -> Edge {
        match wmsz {
            WMSZ_LEFT => Edge::Left,
            WMSZ_RIGHT => Edge::Right,
            WMSZ_TOP => Edge::Top,
            WMSZ_BOTTOM => Edge::Bottom,
            WMSZ_TOPLEFT => Edge::TopLeft,
            WMSZ_TOPRIGHT => Edge::TopRight,
            WMSZ_BOTTOMLEFT => Edge::BottomLeft,
            _ => Edge::BottomRight,
        }
    }

    /// `rect` (window coordinates) with its client area made 16:9 by `edge`.
    fn fit(hwnd: HWND, edge: Edge, rect: Rect) -> Rect {
        let (dw, dh) = frame(hwnd);
        let (min, max) = limits();
        let inner = Rect { right: rect.right - dw, bottom: rect.bottom - dh, ..rect };
        let out = constrain(edge, inner, min, max);
        Rect { right: out.right + dw, bottom: out.bottom + dh, ..out }
    }

    unsafe extern "system" fn subclass_proc(
        hwnd: HWND,
        umsg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _data: usize,
    ) -> LRESULT {
        match umsg {
            WM_SIZING => {
                // The rectangle the drag would give, which the window takes as rewritten here.
                if let Some(r) = unsafe { (lparam.0 as *mut RECT).as_mut() } {
                    let rect = Rect { left: r.left, top: r.top, right: r.right, bottom: r.bottom };
                    let out = fit(hwnd, edge(wparam.0 as u32), rect);
                    *r =
                        RECT { left: out.left, top: out.top, right: out.right, bottom: out.bottom };
                    let _ = unsafe { DefSubclassProc(hwnd, umsg, wparam, lparam) };
                    return LRESULT(1);
                }
            }
            WM_WINDOWPOSCHANGING => {
                // After the default handling, which applies tao's min/max track size: whatever
                // resized the window outside a drag (Snap, Win+arrows, a DPI change) ends up 16:9,
                // top-left kept. A drag's own move arrives here already 16:9 and is left alone.
                let result = unsafe { DefSubclassProc(hwnd, umsg, wparam, lparam) };
                if let Some(pos) = unsafe { (lparam.0 as *mut WINDOWPOS).as_mut() } {
                    let resizing = pos.flags.0 & SWP_NOSIZE.0 == 0;
                    // A window being minimised is parked at -32000 with a caption-sized rect.
                    let parked = pos.x <= -32000 || unsafe { IsIconic(hwnd) }.as_bool();
                    if resizing && !parked && pos.cx > 0 && pos.cy > 0 {
                        let rect = Rect {
                            left: pos.x,
                            top: pos.y,
                            right: pos.x + pos.cx,
                            bottom: pos.y + pos.cy,
                        };
                        let out = fit(hwnd, Edge::Right, rect);
                        pos.cx = out.right - out.left;
                        pos.cy = out.bottom - out.top;
                    }
                }
                return result;
            }
            // LOWORD is the new X DPI. Stored before tao handles it and resizes for the new scale.
            WM_DPICHANGED => DPI.store((wparam.0 & 0xffff) as u32, Ordering::Relaxed),
            WM_NCDESTROY => {
                let _ = unsafe { RemoveWindowSubclass(hwnd, Some(subclass_proc), SUBCLASS_ID) };
            }
            _ => {}
        }
        unsafe { DefSubclassProc(hwnd, umsg, wparam, lparam) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i32 = 320;
    const MAX: i32 = 1152; // 60 % of a 1920-wide display

    fn rect(left: i32, top: i32, w: i32, h: i32) -> Rect {
        Rect { left, top, right: left + w, bottom: top + h }
    }

    fn size(r: Rect) -> (i32, i32) {
        (r.right - r.left, r.bottom - r.top)
    }

    /// Within a pixel of 16:9: `|h * 16 - w * 9| <= 16` is `|h - w * 9 / 16| <= 1`.
    fn assert_16_9(r: Rect) {
        let (w, h) = size(r);
        assert!((h * 16 - w * 9).abs() <= 16, "{w}x{h} is not 16:9");
    }

    /// The window starts at 480x270 at (100, 100) and is dragged to a non-16:9 rect.
    const START: Rect = Rect { left: 100, top: 100, right: 580, bottom: 370 };

    #[test]
    fn sides_follow_the_width_and_move_the_bottom() {
        // Right border dragged out by 160: left and top stay, the bottom follows.
        let out = constrain(Edge::Right, Rect { right: 740, ..START }, MIN, MAX);
        assert_eq!(out, rect(100, 100, 640, 360));
        // Left border dragged out by 160: right and top stay.
        let out = constrain(Edge::Left, Rect { left: -60, ..START }, MIN, MAX);
        assert_eq!(out, rect(-60, 100, 640, 360));
        assert_eq!(out.right, START.right);
        // Odd widths round the height and stay within a pixel.
        for w in [321, 333, 500, 777, 1000, 1151] {
            assert_16_9(constrain(Edge::Right, rect(0, 0, w, 10), MIN, MAX));
            assert_16_9(constrain(Edge::Left, rect(0, 0, w, 999), MIN, MAX));
        }
    }

    #[test]
    fn top_and_bottom_follow_the_height_and_move_the_right() {
        // Bottom border dragged down by 90: top and left stay, the right border follows.
        let out = constrain(Edge::Bottom, Rect { bottom: 460, ..START }, MIN, MAX);
        assert_eq!(out, rect(100, 100, 640, 360));
        // Top border dragged up by 90: bottom and left stay.
        let out = constrain(Edge::Top, Rect { top: 10, ..START }, MIN, MAX);
        assert_eq!(out, rect(100, 10, 640, 360));
        assert_eq!(out.bottom, START.bottom);
        for h in [181, 200, 271, 333, 500, 647] {
            assert_16_9(constrain(Edge::Bottom, rect(0, 0, 5, h), MIN, MAX));
            assert_16_9(constrain(Edge::Top, rect(0, 0, 5000, h), MIN, MAX));
        }
    }

    #[test]
    fn corners_follow_the_width_and_hold_the_opposite_corner() {
        // Each corner dragged 160 wider and only 10 taller: the width wins.
        let cases = [
            (
                Edge::BottomRight,
                Rect { right: 740, bottom: 380, ..START },
                rect(100, 100, 640, 360),
            ),
            (Edge::BottomLeft, Rect { left: -60, bottom: 380, ..START }, rect(-60, 100, 640, 360)),
            (Edge::TopRight, Rect { right: 740, top: 90, ..START }, rect(100, 10, 640, 360)),
            (Edge::TopLeft, Rect { left: -60, top: 90, ..START }, rect(-60, 10, 640, 360)),
        ];
        for (edge, dragged, want) in cases {
            let out = constrain(edge, dragged, MIN, MAX);
            assert_eq!(out, want, "{edge:?}");
            assert_16_9(out);
        }
        // The corner opposite the dragged one never moves.
        let held = |edge, out: Rect| match edge {
            Edge::BottomRight => (out.left, out.top) == (START.left, START.top),
            Edge::BottomLeft => (out.right, out.top) == (START.right, START.top),
            Edge::TopRight => (out.left, out.bottom) == (START.left, START.bottom),
            _ => (out.right, out.bottom) == (START.right, START.bottom),
        };
        for (edge, dragged, _) in cases {
            assert!(held(edge, constrain(edge, dragged, MIN, MAX)), "{edge:?}");
        }
    }

    #[test]
    fn clamps_to_the_minimum_and_maximum() {
        let all = [
            Edge::Left,
            Edge::Right,
            Edge::Top,
            Edge::Bottom,
            Edge::TopLeft,
            Edge::TopRight,
            Edge::BottomLeft,
            Edge::BottomRight,
        ];
        for edge in all {
            // Dragged far too small, past zero even.
            let tiny = constrain(edge, rect(100, 100, -40, -20), MIN, MAX);
            assert_eq!(size(tiny), (320, 180), "{edge:?}");
            let tiny = constrain(edge, rect(100, 100, 50, 30), MIN, MAX);
            assert_eq!(size(tiny), (320, 180), "{edge:?}");
            // Dragged far too big.
            let huge = constrain(edge, rect(100, 100, 4000, 3000), MIN, MAX);
            assert_eq!(size(huge), (1152, 648), "{edge:?}");
        }
        // The clamp still holds the edge opposite the drag.
        let out = constrain(Edge::TopLeft, rect(0, 0, 4000, 3000), MIN, MAX);
        assert_eq!((out.right, out.bottom), (4000, 3000));
        // A maximum below the minimum (a tiny display) gives the minimum, not a panic.
        assert_eq!(size(constrain(Edge::Right, rect(0, 0, 900, 500), MIN, 200)), (320, 180));
    }

    #[test]
    fn the_maximum_is_sixty_percent_of_the_display() {
        assert_eq!(max_width(1920.0), 1152.0);
        assert_eq!(height_for(max_width(1920.0)), 648.0);
        assert_eq!(max_width(2560.0), 1536.0);
        assert_eq!(max_width(1366.0), 819.0);
        // Never below the minimum.
        assert_eq!(max_width(400.0), MIN_WIDTH);
        assert_eq!(height_for(MIN_WIDTH), 180.0);
        assert_eq!(height_for(DEFAULT_WIDTH), 270.0);
    }

    #[test]
    fn physical_limits_stay_inside_the_logical_ones() {
        set_limits(MIN_WIDTH, max_width(1920.0));
        assert_eq!(physical_limits(1.0), (320, 1152));
        assert_eq!(physical_limits(1.5), (480, 1728));
        // 320 * 1.25 = 400 exactly; 1152 * 1.25 = 1440.
        assert_eq!(physical_limits(1.25), (400, 1440));
        // Already 16:9, or within a pixel of it: left alone.
        assert_eq!(settled(480, 270, 1.0), None);
        assert_eq!(settled(481, 270, 1.0), None);
        // Wayland handed back a free-form size: the width decides.
        assert_eq!(settled(800, 300, 1.0), Some((800, 450)));
        assert_eq!(settled(5000, 300, 1.0), Some((1152, 648)));
        assert_eq!(settled(100, 300, 1.0), Some((320, 180)));
    }
}

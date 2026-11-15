//! [fork] Forwards the mouse wheel to the dock.
//!
//! Windows only delivers `WM_MOUSEWHEEL` to the hovered window if "Scroll inactive windows when hovering
//! over them" is enabled, and the dock is never the focused window. So a low-level mouse hook catches the
//! wheel while the cursor is over a dock window, swallows it (so the focused app doesn't scroll too) and
//! forwards it to that dock webview, where `fork/weg/wheelBridge.ts` replays it as a DOM `wheel` event.
//!
//! Transparent areas of the dock ignore cursor events, so they are skipped by `WindowFromPoint` and the
//! wheel keeps reaching the apps behind them.
//!
//! ## Keeping the hook fast
//!
//! Windows waits for a low-level hook before delivering any mouse input, system wide, and silently removes
//! a hook that keeps timing out. So the hook only makes cheap Win32 calls and one lock-free read:
//!
//! - The dock windows are cached in [`DOCKS`]. Asking Tauri for a window handle blocks on a round trip to
//!   the main thread, so the cache is fed by upstream's widget status event instead, which already carries
//!   the label and handle of every widget window and is sent whenever one is created or changes status.
//! - Emitting to the webview happens on the forwarder thread, which also merges the wheel deltas queued
//!   while it was busy, so a fast-spinning wheel or a touchpad doesn't flood the webview with events.

use std::sync::{
    Arc, LazyLock, OnceLock,
    mpsc::{Receiver, Sender, channel},
};

use arc_swap::ArcSwap;
use seelen_core::{handlers::SeelenEvent, resource::WidgetId, state::WidgetDebugInfo};
use tauri::{Emitter, Listener};
use windows::Win32::{
    Foundation::{LPARAM, LRESULT, POINT, WPARAM},
    UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GA_ROOT, GetAncestor, GetMessageW,
        GetWindowThreadProcessId, HC_ACTION, MSG, MSLLHOOKSTRUCT, SetWindowsHookExW,
        TranslateMessage, WH_MOUSE_LL, WM_MOUSEWHEEL, WindowFromPoint,
    },
};

use crate::{
    app::get_app_handle, error::ResultLogExt, utils::spawn_named_thread,
    widgets::notify_widget_statuses_change, windows_api::WindowsApi,
};

const WHEEL_EVENT: &str = "fork::wheel";

/// Dock windows currently alive, read by the hook on every wheel event
static DOCKS: LazyLock<ArcSwap<Vec<DockWindow>>> =
    LazyLock::new(|| ArcSwap::from_pointee(Vec::new()));

/// (dock window handle, wheel delta) pairs, emitted outside of the hook to keep it as fast as possible
static FORWARDER: OnceLock<Sender<(isize, i16)>> = OnceLock::new();

struct DockWindow {
    hwnd: isize,
    label: String,
}

impl DockWindow {
    fn from_widgets(widgets: Vec<WidgetDebugInfo>) -> Vec<Self> {
        let weg = WidgetId::known_weg().to_string();
        widgets
            .into_iter()
            .filter(|widget| widget.widget_id == weg)
            .filter_map(|widget| {
                Some(Self {
                    hwnd: widget.webview_window_id?,
                    label: widget.label,
                })
            })
            .collect()
    }
}

pub fn start() {
    let (sender, receiver) = channel::<(isize, i16)>();
    if FORWARDER.set(sender).is_err() {
        return;
    }

    get_app_handle().listen_any(SeelenEvent::WidgetDebugInfoChanged, |event| {
        match serde_json::from_str::<Vec<WidgetDebugInfo>>(event.payload()) {
            Ok(widgets) => DOCKS.store(Arc::new(DockWindow::from_widgets(widgets))),
            Err(err) => log::error!("Failed to read the widget windows for the dock wheel: {err}"),
        }
    });
    // the docks may already exist, so ask upstream to send the current widget windows once
    notify_widget_statuses_change();

    spawn_named_thread("Fork Wheel Forwarder", move || forward_wheel(receiver));

    // low-level hooks are called on the installing thread, so it needs its own message loop
    spawn_named_thread("Fork Wheel Hook", || {
        let module = WindowsApi::module_handle_w().ok().map(Into::into);
        if let Err(err) = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), module, 0) } {
            log::error!("Failed to install the dock wheel hook: {err}");
            return;
        }

        let mut msg = MSG::default();
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    });
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && wparam.0 == WM_MOUSEWHEEL as usize {
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        if let Some(hwnd) = dock_at(info.pt) {
            let delta = (info.mouseData >> 16) as u16 as i16;
            if let Some(forwarder) = FORWARDER.get() {
                let _ = forwarder.send((hwnd, delta));
            }
            return LRESULT(1);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Handle of the dock window under the point, if any
fn dock_at(point: POINT) -> Option<isize> {
    let root = unsafe { GetAncestor(WindowFromPoint(point), GA_ROOT) };
    if root.is_invalid() {
        return None;
    }

    let hwnd = root.0 as isize;
    if !DOCKS.load().iter().any(|dock| dock.hwnd == hwnd) {
        return None;
    }

    // guards against a stale cache entry whose handle was reused by another process
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(root, Some(&mut pid)) };
    (pid == std::process::id()).then_some(hwnd)
}

/// Emits the wheel to the dock webviews, merging the deltas that queued up while the last emit ran
fn forward_wheel(receiver: Receiver<(isize, i16)>) {
    let mut batch: Vec<(isize, i32)> = Vec::new();
    while let Ok(first) = receiver.recv() {
        batch.clear();
        for (hwnd, delta) in std::iter::once(first).chain(receiver.try_iter()) {
            match batch.iter_mut().find(|(h, _)| *h == hwnd) {
                Some((_, total)) => *total += i32::from(delta),
                None => batch.push((hwnd, i32::from(delta))),
            }
        }

        let docks = DOCKS.load();
        for &(hwnd, delta) in &batch {
            if delta == 0 {
                continue;
            }
            if let Some(dock) = docks.iter().find(|dock| dock.hwnd == hwnd) {
                get_app_handle()
                    .emit_to(dock.label.as_str(), WHEEL_EVENT, delta)
                    .log_error();
            }
        }
    }
}

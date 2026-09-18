//! macOS tray icon, popover show/hide, and blur-grace handling.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tauri::image::Image;
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{App, Manager};
use tauri_plugin_positioner::{Position, WindowExt};
use tokio::sync::Notify;

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Skip hide-on-blur for this long after a tray click shows the window.
/// macOS focuses out immediately after that show.
const TRAY_SHOW_BLUR_GRACE_MS: u64 = 500;

/// Ignore a tray mouse-up that arrives this soon after hide-on-blur.
/// macOS 27 gives the status item key focus on mouse-down, so blur
/// hides the popover ~80ms before the mouse-up that would toggle it.
const TRAY_CLICK_CLOSE_GRACE_MS: u64 = 250;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayLeftClickAction {
    Show,
    Hide,
    AlreadyClosed,
}

fn tray_left_click_action(
    window_visible: bool,
    now_ms: u64,
    last_blur_hide_ms: u64,
) -> TrayLeftClickAction {
    if window_visible {
        return TrayLeftClickAction::Hide;
    }
    if now_ms.saturating_sub(last_blur_hide_ms) < TRAY_CLICK_CLOSE_GRACE_MS {
        return TrayLeftClickAction::AlreadyClosed;
    }
    TrayLeftClickAction::Show
}

fn should_skip_blur_hide(now_ms: u64, last_tray_show_ms: u64) -> bool {
    now_ms.saturating_sub(last_tray_show_ms) < TRAY_SHOW_BLUR_GRACE_MS
}

pub(crate) struct TrayState {
    pub(crate) icon: Mutex<tauri::tray::TrayIcon>,
    pub(crate) icon_reset: AtomicBool,
    /// Millisecond timestamp of the last tray-click window show.
    /// The blur handler skips hide events within a short grace
    /// period to prevent the tray click from immediately
    /// dismissing the window on macOS.
    pub(crate) last_tray_show_ms: AtomicU64,
    /// Millisecond timestamp of the last hide-on-blur. Used to
    /// ignore the trailing tray mouse-up after macOS 27 steals
    /// key focus on mouse-down.
    pub(crate) last_blur_hide_ms: AtomicU64,
}
pub(crate) fn setup_tray_and_window(
    app: &App,
    is_visible: &Arc<AtomicBool>,
    wake: &Arc<Notify>,
) -> Result<(), Box<dyn std::error::Error>> {
    let icon = Image::from_bytes(include_bytes!("../icons/tray-default.png"))?;

    // Do not attach an NSMenu to the status item. On macOS 27 AppKit
    // swallows mouse events while a menu is attached, so every click
    // only opens that menu and the popover never appears. Quit lives
    // in the popover footer instead. See tauri-apps/tray-icon#355.
    let tray_visible = Arc::clone(is_visible);
    let tray_wake = Arc::clone(wake);
    let tray = TrayIconBuilder::new()
        .icon(icon)
        .icon_as_template(true)
        .on_tray_icon_event(move |tray, event| {
            tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);

            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                handle_tray_left_click(tray, &tray_visible, &tray_wake);
            }
        })
        .build(app)?;

    app.manage(TrayState {
        icon: Mutex::new(tray),
        icon_reset: AtomicBool::new(false),
        last_tray_show_ms: AtomicU64::new(0),
        last_blur_hide_ms: AtomicU64::new(0),
    });

    let blur_handle = app.handle().clone();
    let blur_visible = Arc::clone(is_visible);
    let blur_wake = Arc::clone(wake);
    if let Some(window) = app.get_webview_window("main") {
        let w = window.clone();
        window.on_window_event(move |event| {
            if let tauri::WindowEvent::Focused(false) = event {
                handle_window_blur(&blur_handle, &w, &blur_visible, &blur_wake);
            }
        });
    }

    Ok(())
}

fn handle_tray_left_click(tray: &TrayIcon, tray_visible: &AtomicBool, tray_wake: &Notify) {
    let app = tray.app_handle();
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let visible = window.is_visible().unwrap_or(false);
    let last_blur = app
        .try_state::<TrayState>()
        .map_or(0, |s| s.last_blur_hide_ms.load(Ordering::Acquire));

    match tray_left_click_action(visible, unix_now_ms(), last_blur) {
        TrayLeftClickAction::Hide => hide_tray_window(&window, tray_visible, tray_wake),
        TrayLeftClickAction::AlreadyClosed => {}
        TrayLeftClickAction::Show => show_tray_window(tray, &window, tray_visible, tray_wake),
    }
}

fn hide_tray_window(window: &tauri::WebviewWindow, visible: &AtomicBool, wake: &Notify) {
    if let Err(e) = window.hide() {
        tracing::warn!("failed to hide window: {e}");
    }
    visible.store(false, Ordering::Release);
    wake.notify_one();
}

fn show_tray_window(
    tray: &TrayIcon,
    window: &tauri::WebviewWindow,
    visible: &AtomicBool,
    wake: &Notify,
) {
    let app = tray.app_handle();
    if let Ok(img) = Image::from_bytes(include_bytes!("../icons/tray-default.png")) {
        let _ = tray.set_icon(Some(img));
        let _ = tray.set_icon_as_template(true);
        let _ = tray.set_tooltip(Some("Observer Ward"));
    }
    if let Some(state) = app.try_state::<TrayState>() {
        state.icon_reset.store(true, Ordering::Release);
        state
            .last_tray_show_ms
            .store(unix_now_ms(), Ordering::Release);
    }
    if let Err(e) = window.move_window(Position::TrayCenter) {
        tracing::warn!("failed to position window: {e}");
    }
    if let Err(e) = window.show() {
        tracing::warn!("failed to show window: {e}");
    }
    if let Err(e) = window.set_focus() {
        tracing::warn!("failed to focus window: {e}");
    }
    visible.store(true, Ordering::Release);
    wake.notify_one();
}

fn handle_window_blur(
    app: &tauri::AppHandle,
    window: &tauri::WebviewWindow,
    visible: &AtomicBool,
    wake: &Notify,
) {
    if !visible.load(Ordering::Acquire) {
        return;
    }
    let now = unix_now_ms();
    if let Some(state) = app.try_state::<TrayState>() {
        let shown_at = state.last_tray_show_ms.load(Ordering::Acquire);
        if should_skip_blur_hide(now, shown_at) {
            return;
        }
        state.last_blur_hide_ms.store(now, Ordering::Release);
    }
    hide_tray_window(window, visible, wake);
}

#[cfg(test)]
mod tests {
    use super::{
        TRAY_CLICK_CLOSE_GRACE_MS, TRAY_SHOW_BLUR_GRACE_MS, TrayLeftClickAction,
        should_skip_blur_hide, tray_left_click_action, unix_now_ms,
    };

    #[test]
    fn unix_now_ms_is_millisecond_resolution() {
        let a = unix_now_ms();
        std::thread::sleep(std::time::Duration::from_millis(15));
        let b = unix_now_ms();
        assert!(b > a, "expected millisecond tick, got {a} then {b}");
    }

    #[test]
    fn tray_left_click_hides_when_window_is_visible() {
        assert_eq!(
            tray_left_click_action(true, 10_000, 0),
            TrayLeftClickAction::Hide
        );
    }

    #[test]
    fn tray_left_click_shows_when_window_is_hidden() {
        assert_eq!(
            tray_left_click_action(false, 10_000, 0),
            TrayLeftClickAction::Show
        );
    }

    #[test]
    fn tray_left_click_ignores_reopen_when_blur_just_hid_the_window() {
        // macOS 27 steals key focus on tray mouse-down, so hide-on-blur
        // runs ~80ms before the mouse-up that would otherwise toggle.
        let hidden_at = 10_000;
        let mouse_up = hidden_at + 80;
        assert!(mouse_up - hidden_at < TRAY_CLICK_CLOSE_GRACE_MS);
        assert_eq!(
            tray_left_click_action(false, mouse_up, hidden_at),
            TrayLeftClickAction::AlreadyClosed
        );
    }

    #[test]
    fn tray_left_click_reopens_after_blur_close_grace() {
        let hidden_at = 10_000;
        let later = hidden_at + TRAY_CLICK_CLOSE_GRACE_MS;
        assert_eq!(
            tray_left_click_action(false, later, hidden_at),
            TrayLeftClickAction::Show
        );
    }

    #[test]
    fn hidden_window_blur_must_not_block_next_show() {
        // If last_blur_hide_ms is stamped while already hidden, a tray
        // mouse-up within 250ms would be AlreadyClosed instead of Show.
        assert_eq!(
            tray_left_click_action(false, 10_080, 10_000),
            TrayLeftClickAction::AlreadyClosed
        );
        assert_eq!(
            tray_left_click_action(false, 10_080, 0),
            TrayLeftClickAction::Show
        );
    }

    #[test]
    fn blur_hide_is_skipped_within_show_grace() {
        let shown_at = 5_000;
        assert!(should_skip_blur_hide(
            shown_at + TRAY_SHOW_BLUR_GRACE_MS - 1,
            shown_at
        ));
        assert!(!should_skip_blur_hide(
            shown_at + TRAY_SHOW_BLUR_GRACE_MS,
            shown_at
        ));
    }
}

//! macOS tray icon, popover show/hide, blur-grace, and staying alive
//! while that popover is hidden.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

#[cfg(target_os = "macos")]
use objc2_foundation::{NSProcessInfo, NSString};
use tauri::image::Image;
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{App, AppHandle, Manager, PhysicalPosition, Position, Rect, Size, WebviewWindow};
use tokio::sync::Notify;

/// Skip hide-on-blur for this long after a tray click shows the window.
/// macOS focuses out immediately after that show.
const TRAY_SHOW_BLUR_GRACE_MS: u64 = 500;

/// Ignore a tray mouse-up that arrives this soon after hide-on-blur.
/// macOS 27 gives the status item key focus on mouse-down, so blur
/// hides the popover ~80ms before the mouse-up that would toggle it.
const TRAY_CLICK_CLOSE_GRACE_MS: u64 = 250;

/// Stored in a timestamp atomic that has never been written.
const NEVER: u64 = u64::MAX;

/// What the tray icon shows. Chosen by the poller, drawn here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrayIconKind {
    Default,
    Warn,
    Crit,
    Restart,
}

/// Tray PNGs, decoded once at startup.
struct TrayIcons {
    default: Image<'static>,
    warn: Image<'static>,
    crit: Image<'static>,
    restart: Image<'static>,
}

impl TrayIcons {
    fn decode() -> tauri::Result<Self> {
        Ok(Self {
            default: Image::from_bytes(include_bytes!("../icons/tray-default.png"))?,
            warn: Image::from_bytes(include_bytes!("../icons/tray-warn.png"))?,
            crit: Image::from_bytes(include_bytes!("../icons/tray-crit.png"))?,
            restart: Image::from_bytes(include_bytes!("../icons/tray-restart.png"))?,
        })
    }

    /// The image for `kind` and whether macOS should tint it as a
    /// template (only the monochrome default icon).
    fn get(&self, kind: TrayIconKind) -> (&Image<'static>, bool) {
        match kind {
            TrayIconKind::Default => (&self.default, true),
            TrayIconKind::Warn => (&self.warn, false),
            TrayIconKind::Crit => (&self.crit, false),
            TrayIconKind::Restart => (&self.restart, false),
        }
    }
}

fn tooltip(kind: TrayIconKind) -> &'static str {
    match kind {
        TrayIconKind::Default => "Observer Ward — all clear",
        TrayIconKind::Warn => "Observer Ward — warning",
        TrayIconKind::Crit => "Observer Ward — critical",
        TrayIconKind::Restart => "Observer Ward — restart detected",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayLeftClickAction {
    Show,
    Hide,
    AlreadyClosed,
}

/// Milliseconds between two readings of the monotonic clock, or `None`
/// when `then` was never recorded.
fn since(now_ms: u64, then_ms: u64) -> Option<u64> {
    (then_ms != NEVER).then(|| now_ms.saturating_sub(then_ms))
}

fn tray_left_click_action(
    window_visible: bool,
    now_ms: u64,
    last_blur_hide_ms: u64,
) -> TrayLeftClickAction {
    if window_visible {
        return TrayLeftClickAction::Hide;
    }
    match since(now_ms, last_blur_hide_ms) {
        Some(elapsed) if elapsed < TRAY_CLICK_CLOSE_GRACE_MS => TrayLeftClickAction::AlreadyClosed,
        Some(_) | None => TrayLeftClickAction::Show,
    }
}

fn should_skip_blur_hide(now_ms: u64, last_tray_show_ms: u64, native_dialog_open: bool) -> bool {
    // A native panel (file picker) takes key focus from the popover; hiding
    // then would dismiss the form the user is filling in.
    native_dialog_open
        || since(now_ms, last_tray_show_ms).is_some_and(|elapsed| elapsed < TRAY_SHOW_BLUR_GRACE_MS)
}

/// Physical pixels of the status item that was clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TrayIconRect {
    /// Left edge.
    x: i32,
    /// Top edge. On a macOS menu bar this is near zero.
    y: i32,
    /// Width. The popover is centered on this.
    width: i32,
}

/// Physical pixels of the popover's outer frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PopoverSize {
    /// Outer width.
    width: i32,
    /// Outer height.
    height: i32,
}

/// Read the status-item rectangle from a tray click.
///
/// tray-icon 0.24 reports a physical rectangle, and Tauri stores it as
/// `Position::Physical` / `Size::Physical`. A logical rectangle is not
/// produced on this path. Returning `None` skips placement instead of
/// guessing a scale factor.
fn tray_icon_rect(rect: &Rect) -> Option<TrayIconRect> {
    let (x, y) = match rect.position {
        Position::Physical(position) => (position.x, position.y),
        Position::Logical(_) => return None,
    };
    let width = match rect.size {
        Size::Physical(size) => i32::try_from(size.width).ok()?,
        Size::Logical(_) => return None,
    };
    Some(TrayIconRect { x, y, width })
}

/// Top-left of a popover centered on a menu-bar icon.
///
/// When `icon.y - popover.height` would be negative or overflow, the top of
/// the popover is pinned to the icon so it hangs downward. That is the macOS
/// menu-bar case. It is the same rule `tauri-plugin-positioner` 2.3.2 used
/// for `Position::TrayCenter`.
///
/// The click rect is already in global physical pixels: tray-icon 0.24
/// `get_tray_rect` flips the status item into main-display top-left space,
/// then converts to physical pixels. `TrayCenter` did not add a monitor
/// origin. It still called `current_monitor()?.unwrap()` before that match.
/// For a hidden window that call is `Ok(None)` after its display goes away
/// (`NSWindow.screen` is nil). The unwrap panicked and the process exited 101.
fn popover_origin(icon: TrayIconRect, popover: PopoverSize) -> (i32, i32) {
    let x = icon
        .x
        .saturating_add(icon.width / 2)
        .saturating_sub(popover.width / 2);
    let y = match icon.y.checked_sub(popover.height) {
        Some(y) if y >= 0 => y,
        Some(_) | None => icon.y,
    };
    (x, y)
}

fn popover_size(outer_width: u32, outer_height: u32) -> Option<PopoverSize> {
    Some(PopoverSize {
        width: i32::try_from(outer_width).ok()?,
        height: i32::try_from(outer_height).ok()?,
    })
}

/// Place the popover centered on the icon. A failure leaves it where it is
/// and still lets the caller show it: placement must not take the process down.
fn place_popover(window: &WebviewWindow, icon: TrayIconRect) {
    let outer = match window.outer_size() {
        Ok(outer) => outer,
        Err(e) => {
            tracing::warn!("failed to read popover size: {e}");
            return;
        }
    };
    let Some(popover) = popover_size(outer.width, outer.height) else {
        tracing::warn!("popover size does not fit in a screen coordinate");
        return;
    };
    let (x, y) = popover_origin(icon, popover);
    if let Err(e) = window.set_position(PhysicalPosition::new(x, y)) {
        tracing::warn!("failed to position window: {e}");
    }
}

/// Tray handle plus the click/blur bookkeeping, managed as Tauri state.
///
/// Timestamps are milliseconds since `epoch`, a monotonic `Instant`. Wall
/// clock time would let an NTP step or sleep/wake adjustment backwards
/// keep the grace windows open, swallowing clicks until it caught up.
pub(crate) struct TrayState {
    /// Behind a mutex so the icon, template flag and tooltip of one update
    /// are applied together, never interleaved with another update.
    icon: Mutex<TrayIcon>,
    icons: TrayIcons,
    epoch: Instant,
    /// Set when a tray click resets the icon, so the poller re-applies
    /// the current level even if it has not changed.
    icon_reset: AtomicBool,
    /// When a tray click last showed the window. The blur handler skips
    /// hide events within a short grace period after it.
    last_tray_show_ms: AtomicU64,
    /// When hide-on-blur last ran. Used to ignore the trailing tray
    /// mouse-up after macOS 27 steals key focus on mouse-down.
    last_blur_hide_ms: AtomicU64,
    /// Number of native dialogs opened from the popover that are still on
    /// screen. A count, not a flag, so closing one of two overlapping
    /// dialogs does not re-enable hide-on-blur under the other. Changed
    /// only through [`NativeDialogGuard`].
    native_dialogs_open: AtomicUsize,
}

impl TrayState {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(NEVER - 1)
    }

    /// Draw `kind` on the tray icon.
    pub(crate) fn show_kind(&self, kind: TrayIconKind) {
        let (image, is_template) = self.icons.get(kind);
        let tray = self.icon.lock().unwrap_or_else(PoisonError::into_inner);

        if let Err(e) = tray.set_icon(Some(image.clone())) {
            tracing::warn!("failed to set tray icon: {e}");
        }
        if let Err(e) = tray.set_icon_as_template(is_template) {
            tracing::warn!("failed to set tray icon template flag: {e}");
        }
        if let Err(e) = tray.set_tooltip(Some(tooltip(kind))) {
            tracing::warn!("failed to set tray tooltip: {e}");
        }
    }

    /// True once after each tray click reset the icon.
    pub(crate) fn take_icon_reset(&self) -> bool {
        self.icon_reset.swap(false, Ordering::AcqRel)
    }
}

/// Keeps the popover from hiding on blur while a native dialog is open.
/// Clears the flag on drop, so an early return or a cancelled command
/// future cannot leave hide-on-blur disabled.
pub(crate) struct NativeDialogGuard(AppHandle);

impl NativeDialogGuard {
    pub(crate) fn open(app: &AppHandle) -> Self {
        if let Some(state) = app.try_state::<TrayState>() {
            state.native_dialogs_open.fetch_add(1, Ordering::Relaxed);
        }
        Self(app.clone())
    }
}

impl Drop for NativeDialogGuard {
    fn drop(&mut self) {
        if let Some(state) = self.0.try_state::<TrayState>() {
            // Saturating: TrayState is managed before any command can run,
            // so open and drop always see it, but never wrap on a mismatch.
            // The closure always returns Some, so the update cannot fail.
            let _always_ok =
                state
                    .native_dialogs_open
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                        Some(n.saturating_sub(1))
                    });
        }
    }
}

/// Keeps the process ineligible for automatic termination.
///
/// One unpaired `disableAutomaticTermination:` for the process
/// lifetime. `AppKit` turns that support on for an ordered-out window,
/// and setting `automaticTerminationSupportEnabled` to false is a
/// no-op. The counter is recorded before support is enabled and
/// applies once it is.
///
/// <https://developer.apple.com/documentation/foundation/processinfo/disableautomatictermination(_:)>
#[cfg(target_os = "macos")]
pub(crate) fn disable_automatic_termination() {
    let reason = NSString::from_str("hidden tray popover");
    NSProcessInfo::processInfo().disableAutomaticTermination(&reason);
}

pub(crate) fn setup_tray_and_window(
    app: &App,
    is_visible: &Arc<AtomicBool>,
    wake: &Arc<Notify>,
) -> Result<(), Box<dyn std::error::Error>> {
    let icons = TrayIcons::decode()?;

    // Do not attach an NSMenu to the status item. On macOS 27 AppKit
    // swallows mouse events while a menu is attached, so every click
    // only opens that menu and the popover never appears. Quit lives
    // in the popover footer instead. See tauri-apps/tray-icon#355.
    let tray_visible = Arc::clone(is_visible);
    let tray_wake = Arc::clone(wake);
    let tray = TrayIconBuilder::new()
        .icon(icons.default.clone())
        .icon_as_template(true)
        .tooltip(tooltip(TrayIconKind::Default))
        .on_tray_icon_event(move |tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                ..
            } = event
            {
                handle_tray_left_click(
                    tray.app_handle(),
                    &tray_visible,
                    &tray_wake,
                    tray_icon_rect(&rect),
                );
            }
        })
        .build(app)?;

    app.manage(TrayState {
        icon: Mutex::new(tray),
        icons,
        epoch: Instant::now(),
        icon_reset: AtomicBool::new(false),
        last_tray_show_ms: AtomicU64::new(NEVER),
        last_blur_hide_ms: AtomicU64::new(NEVER),
        native_dialogs_open: AtomicUsize::new(0),
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

fn handle_tray_left_click(
    app: &AppHandle,
    visible: &AtomicBool,
    wake: &Notify,
    icon: Option<TrayIconRect>,
) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let Some(state) = app.try_state::<TrayState>() else {
        return;
    };
    let window_visible = window.is_visible().unwrap_or(false);
    let last_blur = state.last_blur_hide_ms.load(Ordering::Relaxed);

    match tray_left_click_action(window_visible, state.now_ms(), last_blur) {
        TrayLeftClickAction::Hide => hide_tray_window(&window, visible, wake),
        TrayLeftClickAction::AlreadyClosed => {}
        TrayLeftClickAction::Show => show_tray_window(&state, &window, visible, wake, icon),
    }
}

fn hide_tray_window(window: &WebviewWindow, visible: &AtomicBool, wake: &Notify) {
    if let Err(e) = window.hide() {
        tracing::warn!("failed to hide window: {e}");
    }
    visible.store(false, Ordering::Release);
    wake.notify_one();
}

/// Show the popover. Opening it acknowledges the current state, so the
/// icon drops back to the default until the poller sees a new level.
fn show_tray_window(
    state: &TrayState,
    window: &WebviewWindow,
    visible: &AtomicBool,
    wake: &Notify,
    icon: Option<TrayIconRect>,
) {
    state.show_kind(TrayIconKind::Default);
    state.icon_reset.store(true, Ordering::Release);
    state
        .last_tray_show_ms
        .store(state.now_ms(), Ordering::Relaxed);

    match icon {
        Some(icon) => place_popover(window, icon),
        None => {
            tracing::warn!("tray click had no physical icon rect; leaving the popover where it is");
        }
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
    app: &AppHandle,
    window: &WebviewWindow,
    visible: &AtomicBool,
    wake: &Notify,
) {
    if !visible.load(Ordering::Acquire) {
        return;
    }
    if let Some(state) = app.try_state::<TrayState>() {
        let now = state.now_ms();
        let shown_at = state.last_tray_show_ms.load(Ordering::Relaxed);
        let dialog_open = state.native_dialogs_open.load(Ordering::Relaxed) > 0;
        if should_skip_blur_hide(now, shown_at, dialog_open) {
            return;
        }
        state.last_blur_hide_ms.store(now, Ordering::Relaxed);
    }
    hide_tray_window(window, visible, wake);
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    use super::disable_automatic_termination;
    use super::{
        NEVER, PopoverSize, TRAY_CLICK_CLOSE_GRACE_MS, TRAY_SHOW_BLUR_GRACE_MS, TrayIconRect,
        TrayIcons, TrayLeftClickAction, popover_origin, popover_size, should_skip_blur_hide,
        tray_icon_rect, tray_left_click_action,
    };
    use tauri::{PhysicalPosition, PhysicalSize, Position, Rect, Size};

    #[cfg(target_os = "macos")]
    #[test]
    fn disable_automatic_termination_returns() {
        // The opt-out counter is a private ivar, so this only checks
        // that the call returns.
        disable_automatic_termination();
    }

    #[test]
    fn bundled_tray_icons_decode() {
        assert!(TrayIcons::decode().is_ok());
    }

    #[test]
    fn tray_left_click_hides_when_window_is_visible() {
        assert_eq!(
            tray_left_click_action(true, 10_000, NEVER),
            TrayLeftClickAction::Hide
        );
    }

    #[test]
    fn tray_left_click_shows_when_window_is_hidden() {
        assert_eq!(
            tray_left_click_action(false, 10_000, NEVER),
            TrayLeftClickAction::Show
        );
    }

    #[test]
    fn first_click_right_after_launch_shows() {
        // With a process-relative clock "now" is tiny at startup; a
        // never-recorded blur must not read as "just hidden".
        assert_eq!(
            tray_left_click_action(false, 5, NEVER),
            TrayLeftClickAction::Show
        );
        assert!(!should_skip_blur_hide(5, NEVER, false));
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
    fn blur_hide_is_skipped_within_show_grace() {
        let shown_at = 5_000;
        assert!(should_skip_blur_hide(
            shown_at + TRAY_SHOW_BLUR_GRACE_MS - 1,
            shown_at,
            false
        ));
        assert!(!should_skip_blur_hide(
            shown_at + TRAY_SHOW_BLUR_GRACE_MS,
            shown_at,
            false
        ));
    }

    #[test]
    fn blur_hide_is_skipped_while_native_dialog_is_open() {
        // The file picker steals key focus long after the show grace.
        let shown_at = 5_000;
        let much_later = shown_at + 60_000;
        assert!(should_skip_blur_hide(much_later, shown_at, true));
        assert!(!should_skip_blur_hide(much_later, shown_at, false));
    }

    #[test]
    fn popover_hangs_from_a_top_menu_bar() {
        // Icon frame from the 2026-10-08 death, after the menu bar moved
        // onto the built-in display. Subtracting the popover height would
        // place it above the screen.
        let (x, y) = popover_origin(
            TrayIconRect {
                x: 999,
                y: 4,
                width: 35,
            },
            PopoverSize {
                width: 380,
                height: 120,
            },
        );
        assert_eq!(x, 999 + 35 / 2 - 380 / 2);
        assert_eq!(y, 4);
    }

    #[test]
    fn popover_sits_above_an_icon_when_there_is_room() {
        let (x, y) = popover_origin(
            TrayIconRect {
                x: 1_000,
                y: 800,
                width: 36,
            },
            PopoverSize {
                width: 380,
                height: 120,
            },
        );
        assert_eq!(x, 1_000 + 36 / 2 - 380 / 2);
        assert_eq!(y, 800 - 120);
    }

    #[test]
    fn popover_touches_the_top_when_it_fits_exactly() {
        let (_, y) = popover_origin(
            TrayIconRect {
                x: 0,
                y: 120,
                width: 10,
            },
            PopoverSize {
                width: 10,
                height: 120,
            },
        );
        assert_eq!(y, 0);
    }

    #[test]
    fn popover_origin_does_not_panic_when_subtraction_overflows() {
        let (_, y) = popover_origin(
            TrayIconRect {
                x: i32::MAX,
                y: i32::MIN,
                width: i32::MAX,
            },
            PopoverSize {
                width: i32::MAX,
                height: 1,
            },
        );
        assert_eq!(y, i32::MIN);
    }

    #[test]
    fn tray_icon_rect_reads_a_physical_click() {
        let rect = Rect {
            position: Position::Physical(PhysicalPosition::new(999, 4)),
            size: Size::Physical(PhysicalSize::new(35, 29)),
        };
        assert_eq!(
            tray_icon_rect(&rect),
            Some(TrayIconRect {
                x: 999,
                y: 4,
                width: 35,
            })
        );
    }

    #[test]
    fn tray_icon_rect_rejects_a_logical_rect() {
        let rect = Rect {
            position: Position::Logical(tauri::LogicalPosition::new(10.0, 4.0)),
            size: Size::Physical(PhysicalSize::new(35, 29)),
        };
        assert_eq!(tray_icon_rect(&rect), None);
    }

    #[test]
    fn tray_icon_rect_rejects_a_width_that_does_not_fit_i32() {
        let rect = Rect {
            position: Position::Physical(PhysicalPosition::new(0, 0)),
            size: Size::Physical(PhysicalSize::new(u32::MAX, 1)),
        };
        assert_eq!(tray_icon_rect(&rect), None);
    }

    #[test]
    fn popover_size_rejects_a_dimension_that_does_not_fit_i32() {
        assert_eq!(
            popover_size(380, 120).map(|size| (size.width, size.height)),
            Some((380, 120))
        );
        assert!(popover_size(u32::MAX, 120).is_none());
    }
}

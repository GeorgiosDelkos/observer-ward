mod config;
mod error;
mod grafana_backend;
mod k8s_backend;
mod metrics;
mod poller;
mod ssh_backend;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tauri::image::Image;
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{App, Manager, State};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_positioner::{Position, WindowExt};
use tokio::sync::Notify;

struct ConfigState(Arc<Mutex<config::AppConfig>>);
struct WakeState(Arc<Notify>);
struct LatestMetrics(Arc<Mutex<Option<metrics::MetricsUpdate>>>);
struct LatestAlerts(Arc<Mutex<Option<metrics::AlertsUpdate>>>);

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

pub struct TrayState {
    pub icon: Mutex<tauri::tray::TrayIcon>,
    pub icon_reset: AtomicBool,
    /// Millisecond timestamp of the last tray-click window show.
    /// The blur handler skips hide events within a short grace
    /// period to prevent the tray click from immediately
    /// dismissing the window on macOS.
    pub last_tray_show_ms: AtomicU64,
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned State parameters"
)]
fn get_config(state: State<'_, ConfigState>) -> Result<config::AppConfig, String> {
    let config = state.0.lock().map_err(|e| format!("lock error: {e}"))?;
    Ok(config.clone())
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned State \
              and deserialized parameters"
)]
fn save_config_cmd(
    state: State<'_, ConfigState>,
    wake: State<'_, WakeState>,
    new_config: config::AppConfig,
) -> Result<(), String> {
    let mut config = state.0.lock().map_err(|e| format!("lock error: {e}"))?;
    config::save_config(&new_config).map_err(|e| error::error_chain(&e))?;
    *config = new_config;
    drop(config);
    wake.0.notify_one();
    Ok(())
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned State \
              and deserialized parameters"
)]
fn add_server(
    state: State<'_, ConfigState>,
    wake: State<'_, WakeState>,
    server: config::ServerConfig,
) -> Result<config::AppConfig, String> {
    let mut config = state.0.lock().map_err(|e| format!("lock error: {e}"))?;
    if config.servers.iter().any(|s| s.name() == server.name()) {
        return Err(format!("server '{}' already exists", server.name()));
    }
    let mut next = config.clone();
    next.servers.push(server);
    config::save_config(&next).map_err(|e| error::error_chain(&e))?;
    *config = next.clone();
    drop(config);
    wake.0.notify_one();
    Ok(next)
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned State \
              and deserialized parameters"
)]
fn remove_server(
    state: State<'_, ConfigState>,
    wake: State<'_, WakeState>,
    name: String,
) -> Result<config::AppConfig, String> {
    let mut config = state.0.lock().map_err(|e| format!("lock error: {e}"))?;
    let mut next = config.clone();
    let before = next.servers.len();
    next.servers.retain(|s| s.name() != name);
    if next.servers.len() == before {
        return Err(format!("server '{name}' not found"));
    }
    config::save_config(&next).map_err(|e| error::error_chain(&e))?;
    *config = next.clone();
    drop(config);
    wake.0.notify_one();
    Ok(next)
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro injects WebviewWindow by value"
)]
fn resize_window(window: tauri::WebviewWindow, width: f64, height: f64) -> Result<(), String> {
    window
        .set_size(tauri::LogicalSize::new(width, height))
        .map_err(|e| format!("resize failed: {e}"))
}

/// Only allow characters safe for interpolation into
/// `AppleScript` `do script` strings and shell commands.
fn validate_shell_safe(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{field} is empty"));
    }
    if !value.chars().all(|c| {
        c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '@' | ':' | '~' | '+' | ' ')
    }) {
        return Err(format!("{field} contains unsafe characters"));
    }
    Ok(())
}

fn run_in_terminal(cmd: &str) -> Result<(), String> {
    if std::path::Path::new("/Applications/Warp.app").exists() {
        run_in_warp(cmd)
    } else {
        run_in_terminal_app(cmd)
    }
}

/// Monotonic per-process counter making each temp-script filename unique.
/// Combined with pid + millis it prevents `create_new` from spuriously
/// failing when two terminal launches land in the same millisecond.
static SCRIPT_SEQ: AtomicU64 = AtomicU64::new(0);

fn run_in_warp(cmd: &str) -> Result<(), String> {
    let tmp = std::env::temp_dir().join(format!(
        "ow-cmd-{}-{}-{}.sh",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis()),
        SCRIPT_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    // Create the script exclusively (O_EXCL via create_new) so a symlink
    // pre-planted at this predictable path cannot redirect the write, and
    // with mode 0o700 so only the owner can read/execute it.
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&tmp)
            .map_err(|e| format!("failed to create temp script: {e}"))?;
        file.write_all(format!("#!/bin/bash\n{cmd}\n").as_bytes())
            .map_err(|e| format!("failed to write temp script: {e}"))?;
    }
    let path_str = tmp
        .to_str()
        .ok_or_else(|| "temp path is not valid UTF-8".to_string())?;
    std::process::Command::new("open")
        .args(["-a", "Warp", path_str])
        .spawn()
        .map_err(|e| format!("failed to open Warp: {e}"))?;
    let cleanup_path = tmp.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(10));
        let _ = std::fs::remove_file(&cleanup_path);
    });
    Ok(())
}

fn run_in_terminal_app(cmd: &str) -> Result<(), String> {
    let escaped = cmd.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "tell application \"Terminal\"\n\
         activate\n\
         do script \"{escaped}\"\n\
         end tell"
    );
    std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .spawn()
        .map_err(|e| format!("failed to open Terminal: {e}"))?;
    Ok(())
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned parameters"
)]
fn open_ssh_terminal(
    host: String,
    port: u16,
    user: String,
    key_path: String,
) -> Result<(), String> {
    validate_shell_safe(&host, "host")?;
    validate_shell_safe(&user, "user")?;
    validate_shell_safe(&key_path, "key_path")?;
    let cmd = format!("ssh '{user}'@'{host}' -p {port} -i '{key_path}'");
    run_in_terminal(&cmd)
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned parameters"
)]
fn open_pod_logs(
    pod_name: String,
    namespace: String,
    context: String,
    kubeconfig: Option<String>,
) -> Result<(), String> {
    validate_shell_safe(&pod_name, "pod_name")?;
    validate_shell_safe(&namespace, "namespace")?;
    validate_shell_safe(&context, "context")?;
    if let Some(kc) = &kubeconfig {
        validate_shell_safe(kc, "kubeconfig")?;
    }
    let cmd = if let Some(kc) = &kubeconfig {
        format!(
            "kubectl logs -f '{pod_name}' -n '{namespace}' \
             --context '{context}' --kubeconfig '{kc}'"
        )
    } else {
        format!(
            "kubectl logs -f '{pod_name}' -n '{namespace}' \
             --context '{context}'"
        )
    };
    run_in_terminal(&cmd)
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned AppHandle"
)]
fn copy_to_clipboard(app: tauri::AppHandle, text: String) -> Result<(), String> {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    app.clipboard()
        .write_text(&text)
        .map_err(|e| format!("clipboard write failed: {e}"))
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned parameters"
)]
fn set_grafana_token(
    wake: State<'_, WakeState>,
    name: String,
    token: String,
) -> Result<(), String> {
    let entry = keyring_core::Entry::new(grafana_backend::KEYCHAIN_SERVICE, &name)
        .map_err(|e| format!("keychain error: {e}"))?;
    entry
        .set_password(&token)
        .map_err(|e| format!("keychain write failed: {e}"))?;
    wake.0.notify_one();
    Ok(())
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned parameters"
)]
fn has_grafana_token(name: String) -> bool {
    // Returns false on any error (missing token or keychain failure); the
    // UI only needs "is it configured", and never reads the secret back.
    grafana_backend::read_token(&name).is_ok()
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned parameters"
)]
fn delete_grafana_token(wake: State<'_, WakeState>, name: String) -> Result<(), String> {
    let entry = keyring_core::Entry::new(grafana_backend::KEYCHAIN_SERVICE, &name)
        .map_err(|e| format!("keychain error: {e}"))?;
    match entry.delete_credential() {
        // Deleting a token that was never stored is a no-op success.
        Ok(()) | Err(keyring_core::Error::NoEntry) => {
            wake.0.notify_one();
            Ok(())
        }
        Err(e) => Err(format!("keychain delete failed: {e}")),
    }
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned State parameters"
)]
fn get_latest_metrics(
    state: State<'_, LatestMetrics>,
) -> Result<Option<metrics::MetricsUpdate>, String> {
    let guard = state.0.lock().map_err(|e| format!("lock error: {e}"))?;
    Ok(guard.clone())
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned State parameters"
)]
fn get_latest_alerts(
    state: State<'_, LatestAlerts>,
) -> Result<Option<metrics::AlertsUpdate>, String> {
    let guard = state.0.lock().map_err(|e| format!("lock error: {e}"))?;
    Ok(guard.clone())
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned parameters"
)]
fn open_url(url: String) -> Result<(), String> {
    // http(s) only — refuse file://, custom schemes, or app launches. The
    // URL is passed to `open` as a single argv entry (no shell), so
    // query-string characters cannot be interpreted as shell syntax; scheme
    // validation is the only check needed. Scheme is case-insensitive per
    // RFC 3986, so lowercase a copy for the guard while passing the original
    // url (path/query case preserved) to `open`.
    let scheme_ok = {
        let lower = url.to_ascii_lowercase();
        lower.starts_with("https://") || lower.starts_with("http://")
    };
    if !scheme_ok {
        return Err("url must be http(s)".to_string());
    }
    std::process::Command::new("open")
        .arg(&url)
        .spawn()
        .map_err(|e| format!("failed to open url: {e}"))?;
    Ok(())
}

fn setup_tray_and_window(
    app: &App,
    is_visible: &Arc<AtomicBool>,
    wake: &Arc<Notify>,
) -> Result<(), Box<dyn std::error::Error>> {
    let icon = Image::from_bytes(include_bytes!("../icons/tray-default.png"))?;

    let quit_item = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
    let tray_menu = MenuBuilder::new(app).item(&quit_item).build()?;

    let tray_visible = Arc::clone(is_visible);
    let tray_wake = Arc::clone(wake);
    let tray = TrayIconBuilder::new()
        .icon(icon)
        .icon_as_template(true)
        .menu(&tray_menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            if event.id().as_ref() == "quit" {
                app.exit(0);
            }
        })
        .on_tray_icon_event(move |tray, event| {
            tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);

            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    if window.is_visible().unwrap_or(false) {
                        if let Err(e) = window.hide() {
                            tracing::warn!("failed to hide window: {e}");
                        }
                        tray_visible.store(false, Ordering::Release);
                        tray_wake.notify_one();
                    } else {
                        // Reset tray icon to default on window open
                        if let Ok(img) =
                            Image::from_bytes(include_bytes!("../icons/tray-default.png"))
                        {
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
                        tray_visible.store(true, Ordering::Release);
                        tray_wake.notify_one();
                    }
                }
            }
        })
        .build(app)?;

    app.manage(TrayState {
        icon: Mutex::new(tray),
        icon_reset: AtomicBool::new(false),
        last_tray_show_ms: AtomicU64::new(0),
    });

    let blur_handle = app.handle().clone();
    let blur_visible = Arc::clone(is_visible);
    let blur_wake = Arc::clone(wake);
    if let Some(window) = app.get_webview_window("main") {
        let w = window.clone();
        window.on_window_event(move |event| {
            if let tauri::WindowEvent::Focused(false) = event {
                // On macOS, clicking the tray icon causes the window to
                // lose focus immediately after being shown. Skip the
                // blur-to-hide if the window was just opened via tray
                // click (within 500 ms grace period).
                if let Some(state) = blur_handle.try_state::<TrayState>() {
                    let shown_at = state.last_tray_show_ms.load(Ordering::Acquire);
                    let now = unix_now_ms();
                    if now.saturating_sub(shown_at) < 500 {
                        return;
                    }
                }
                if let Err(e) = w.hide() {
                    tracing::warn!("failed to hide window on blur: {e}");
                }
                blur_visible.store(false, Ordering::Release);
                blur_wake.notify_one();
            }
        });
    }

    Ok(())
}

/// Run the Observer Ward application.
///
/// # Errors
///
/// Returns an error if the Tauri runtime fails to start, the tray
/// icon cannot be created, or the default window icon is missing.
#[expect(
    clippy::exit,
    reason = "tauri::generate_context! macro calls process::exit"
)]
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    // keyring v4 requires a credential store to be registered before any
    // Entry operation; register the platform-native store (macOS Keychain)
    // once at startup. A failure here only disables Grafana token storage —
    // the rest of the app still works — so log and continue.
    if let Err(e) = keyring::use_native_store(false) {
        tracing::warn!("failed to initialize keychain store: {e}");
    }

    let initial_config = config::load_config().unwrap_or_default();
    let config_arc = Arc::new(Mutex::new(initial_config));
    let is_window_visible = Arc::new(AtomicBool::new(false));
    let poll_wake = Arc::new(Notify::new());
    let latest_metrics = Arc::new(Mutex::new(None));
    let latest_alerts = Arc::new(Mutex::new(None));

    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_positioner::init())
        .manage(ConfigState(Arc::clone(&config_arc)))
        .manage(WakeState(Arc::clone(&poll_wake)))
        .manage(LatestMetrics(Arc::clone(&latest_metrics)))
        .manage(LatestAlerts(Arc::clone(&latest_alerts)))
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_config_cmd,
            add_server,
            remove_server,
            resize_window,
            open_ssh_terminal,
            open_pod_logs,
            copy_to_clipboard,
            set_grafana_token,
            has_grafana_token,
            delete_grafana_token,
            get_latest_metrics,
            get_latest_alerts,
            open_url,
        ])
        .setup(move |app| {
            setup_tray_and_window(app, &is_window_visible, &poll_wake)?;

            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let handle = app.handle().clone();
            let config_for_poller = Arc::clone(&config_arc);
            tauri::async_runtime::spawn(async move {
                let mut poller = poller::Poller::new(
                    handle,
                    config_for_poller,
                    is_window_visible,
                    poll_wake,
                    latest_metrics,
                    latest_alerts,
                );
                poller.run().await;
            });

            Ok(())
        })
        .run(tauri::generate_context!())?;

    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::{unix_now_ms, validate_shell_safe};

    #[test]
    fn unix_now_ms_is_millisecond_resolution() {
        let a = unix_now_ms();
        std::thread::sleep(std::time::Duration::from_millis(15));
        let b = unix_now_ms();
        assert!(b > a, "expected millisecond tick, got {a} then {b}");
    }

    #[test]
    fn validate_shell_safe_accepts_typical_values() {
        validate_shell_safe("prod-ctx", "context").expect("context");
        validate_shell_safe("user_name", "user").expect("user");
        validate_shell_safe("/home/user/.ssh/id_ed25519", "key").expect("key");
        validate_shell_safe("10.0.0.5", "host").expect("host");
    }

    #[test]
    fn validate_shell_safe_rejects_quotes_and_semicolons() {
        assert!(validate_shell_safe("foo;rm", "host").is_err());
        assert!(validate_shell_safe("foo'bar", "user").is_err());
        assert!(validate_shell_safe("", "host").is_err());
    }
}

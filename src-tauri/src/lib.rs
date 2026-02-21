use tauri::{
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager,
};
use tauri_plugin_positioner::{Position, WindowExt};

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

    tauri::Builder::default()
        .plugin(tauri_plugin_positioner::init())
        .setup(|app| {
            let icon = app
                .default_window_icon()
                .ok_or("no default icon configured")?
                .clone();

            let _tray = TrayIconBuilder::new()
                .icon(icon)
                .icon_as_template(true)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
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
                                let _ = window.hide();
                            } else {
                                let _ = window.move_window(Position::TrayCenter);
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                    }
                })
                .build(app)?;

            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            Ok(())
        })
        .run(tauri::generate_context!())?;

    Ok(())
}

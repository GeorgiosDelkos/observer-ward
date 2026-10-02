//! Tauri IPC commands invoked by the frontend.
//!
//! Anything that touches the disk or the Keychain is an `async` command
//! and runs the blocking part on the blocking pool: a non-async Tauri
//! command runs on the main thread, where a Keychain prompt or slow disk
//! would freeze the UI.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use serde::Serialize;
use tauri::State;
use tauri_plugin_dialog::DialogExt;
use tokio::sync::Notify;

use crate::config::{self, AppConfig, ConfigError, ConfigStore, ServerConfig, Settings};
use crate::error::error_chain;
use crate::grafana::{self, GrafanaError, TokenEpoch};
use crate::k8s::{self, K8sError};
use crate::metrics::{AlertsUpdate, MetricsUpdate};
use crate::terminal::{self, TerminalError};
use crate::tray::NativeDialogGuard;

pub(crate) struct WakeState(pub(crate) Arc<Notify>);
pub(crate) struct TokenEpochState(pub(crate) Arc<TokenEpoch>);
pub(crate) struct LatestMetrics(pub(crate) Arc<Mutex<Option<MetricsUpdate>>>);
pub(crate) struct LatestAlerts(pub(crate) Arc<Mutex<Option<AlertsUpdate>>>);

/// Every way a command can fail. Typed up to the IPC boundary, where it
/// serializes as its full cause chain, the string the frontend shows.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CommandError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    K8s(#[from] K8sError),
    #[error(transparent)]
    Grafana(#[from] GrafanaError),
    #[error(transparent)]
    Terminal(#[from] TerminalError),
    #[error("server '{0}' not found")]
    UnknownServer(String),
    #[error("server '{name}' is not {expected} server")]
    WrongServerKind {
        name: String,
        expected: &'static str,
    },
    #[error("file picker closed without a result")]
    PickerClosed,
    #[error("selected file is not a local path")]
    NotLocalPath,
    #[error("background task failed")]
    Task(#[from] tokio::task::JoinError),
    #[error("failed to {action}")]
    Window {
        action: &'static str,
        #[source]
        source: tauri::Error,
    },
    #[error("clipboard write failed")]
    Clipboard(#[source] tauri_plugin_clipboard_manager::Error),
    #[error("url must be http(s)")]
    UrlScheme,
    #[error("failed to open url")]
    OpenUrl(#[source] std::io::Error),
}

impl Serialize for CommandError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&error_chain(self))
    }
}

type CommandResult<T> = Result<T, CommandError>;

/// Run blocking work off the async runtime and the main thread.
async fn blocking<T, E>(work: impl FnOnce() -> Result<T, E> + Send + 'static) -> CommandResult<T>
where
    T: Send + 'static,
    E: Into<CommandError> + Send + 'static,
{
    tokio::task::spawn_blocking(work).await?.map_err(Into::into)
}

#[tauri::command]
pub(crate) async fn get_config(store: State<'_, Arc<ConfigStore>>) -> CommandResult<AppConfig> {
    Ok(store.snapshot().await)
}

#[tauri::command]
pub(crate) async fn save_settings(
    store: State<'_, Arc<ConfigStore>>,
    wake: State<'_, WakeState>,
    settings: Settings,
) -> CommandResult<AppConfig> {
    // A disabled connection is not polled, so its URL is not checked.
    if let Some(grafana) = settings.grafana.as_ref().filter(|g| g.enabled) {
        grafana::validate_url(&grafana.url)?;
    }
    let next = store
        .update(|config| {
            settings.apply_to(config);
            Ok::<_, CommandError>(())
        })
        .await?;
    wake.0.notify_one();
    Ok(next)
}

#[tauri::command]
pub(crate) async fn add_server(
    store: State<'_, Arc<ConfigStore>>,
    wake: State<'_, WakeState>,
    server: ServerConfig,
) -> CommandResult<AppConfig> {
    validate_new_server(&server).await?;
    let next = store
        .update(|config| {
            config.servers.push(server);
            Ok::<_, CommandError>(())
        })
        .await?;
    wake.0.notify_one();
    Ok(next)
}

/// Reject entries the poller could never connect with (a mistyped
/// kubeconfig path or context) instead of saving a server that only ever
/// shows "offline". SSH entries are checked by connecting, not here.
async fn validate_new_server(server: &ServerConfig) -> CommandResult<()> {
    match server {
        ServerConfig::K8s(target) => {
            k8s::validate_server(target.kubeconfig.clone(), target.context.clone()).await?;
            Ok(())
        }
        ServerConfig::Ssh(_) => Ok(()),
    }
}

#[tauri::command]
pub(crate) async fn remove_server(
    store: State<'_, Arc<ConfigStore>>,
    wake: State<'_, WakeState>,
    name: String,
) -> CommandResult<AppConfig> {
    let next = store
        .update(|config| {
            let before = config.servers.len();
            config.servers.retain(|s| s.name().as_str() != name);
            if config.servers.len() == before {
                return Err(CommandError::UnknownServer(name));
            }
            Ok(())
        })
        .await?;
    wake.0.notify_one();
    Ok(next)
}

/// Show a native file picker for a kubeconfig and return the chosen path,
/// or `None` if the user cancelled. `current` is the path already in the
/// form; the picker opens in its folder, else in `~/.kube`.
#[tauri::command]
pub(crate) async fn pick_kubeconfig(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    current: Option<String>,
) -> CommandResult<Option<PathBuf>> {
    let fallback = dirs::home_dir().map(|home| home.join(".kube"));
    let start_dir = config::picker_start_dir(current.as_deref(), fallback.as_deref());
    pick_file_path(&app, &window, "Select kubeconfig", start_dir).await
}

/// Like [`pick_kubeconfig`], for an SSH private key; falls back to `~/.ssh`.
#[tauri::command]
pub(crate) async fn pick_ssh_key(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    current: Option<String>,
) -> CommandResult<Option<PathBuf>> {
    let fallback = dirs::home_dir().map(|home| home.join(".ssh"));
    let start_dir = config::picker_start_dir(current.as_deref(), fallback.as_deref());
    pick_file_path(&app, &window, "Select SSH private key", start_dir).await
}

/// Run a native single-file picker while keeping the popover open, then
/// hand key focus back to it.
async fn pick_file_path(
    app: &tauri::AppHandle,
    window: &tauri::WebviewWindow,
    title: &str,
    start_dir: Option<PathBuf>,
) -> CommandResult<Option<PathBuf>> {
    let mut dialog = app.dialog().file().set_title(title);
    if let Some(dir) = start_dir {
        dialog = dialog.set_directory(dir);
    }

    let (tx, rx) = tokio::sync::oneshot::channel();
    let guard = NativeDialogGuard::open(app);
    dialog.pick_file(move |picked| {
        // Sending fails only if the command future was dropped, in which
        // case nobody is waiting for the answer.
        if tx.send(picked).is_err() {
            tracing::debug!("file picker answered after its command was dropped");
        }
    });
    let picked = rx.await.map_err(|_| CommandError::PickerClosed)?;
    drop(guard);

    if let Err(e) = window.set_focus() {
        tracing::warn!("failed to refocus window after file picker: {e}");
    }

    picked
        .map(|file| file.into_path().map_err(|_| CommandError::NotLocalPath))
        .transpose()
}

/// List the contexts in a kubeconfig (the default one when `path` is
/// `None`) so the form can offer them instead of free-typed names.
#[tauri::command]
pub(crate) async fn inspect_kubeconfig(
    path: Option<PathBuf>,
) -> CommandResult<k8s::KubeconfigSummary> {
    Ok(k8s::inspect(path).await?)
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro injects WebviewWindow by value"
)]
pub(crate) fn resize_window(
    window: tauri::WebviewWindow,
    width: f64,
    height: f64,
) -> CommandResult<()> {
    window
        .set_size(tauri::LogicalSize::new(width, height))
        .map_err(|source| CommandError::Window {
            action: "resize window",
            source,
        })
}

/// Open an interactive `ssh` session to the saved server `name`. Only the
/// name crosses the IPC boundary; host, user and key come from config.
#[tauri::command]
pub(crate) async fn open_ssh_terminal(
    store: State<'_, Arc<ConfigStore>>,
    name: String,
) -> CommandResult<()> {
    let config = store.snapshot().await;
    let target = match config.server(&name) {
        Some(ServerConfig::Ssh(target)) => target,
        Some(ServerConfig::K8s(_)) => return Err(wrong_kind(name, "an SSH")),
        None => return Err(CommandError::UnknownServer(name)),
    };
    terminal::run_in_terminal(&terminal::ssh_command(target)?)?;
    Ok(())
}

/// Tail the logs of `pod` on the saved cluster `server`.
#[tauri::command]
pub(crate) async fn open_pod_logs(
    store: State<'_, Arc<ConfigStore>>,
    server: String,
    pod: String,
) -> CommandResult<()> {
    let config = store.snapshot().await;
    let target = match config.server(&server) {
        Some(ServerConfig::K8s(target)) => target,
        Some(ServerConfig::Ssh(_)) => return Err(wrong_kind(server, "a Kubernetes")),
        None => return Err(CommandError::UnknownServer(server)),
    };
    terminal::run_in_terminal(&terminal::pod_logs_command(target, &pod)?)?;
    Ok(())
}

fn wrong_kind(name: String, expected: &'static str) -> CommandError {
    CommandError::WrongServerKind { name, expected }
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned AppHandle"
)]
pub(crate) fn copy_to_clipboard(app: tauri::AppHandle, text: String) -> CommandResult<()> {
    use tauri_plugin_clipboard_manager::ClipboardExt;

    app.clipboard()
        .write_text(&text)
        .map_err(CommandError::Clipboard)
}

#[tauri::command]
pub(crate) async fn set_grafana_token(
    wake: State<'_, WakeState>,
    epoch: State<'_, TokenEpochState>,
    name: String,
    token: String,
) -> CommandResult<()> {
    blocking(move || grafana::store_token(&name, &token)).await?;
    epoch.0.bump();
    wake.0.notify_one();
    Ok(())
}

/// Whether a token is stored. Never returns the secret itself.
#[tauri::command]
pub(crate) async fn has_grafana_token(name: String) -> CommandResult<bool> {
    blocking(move || grafana::has_token(&name)).await
}

#[tauri::command]
pub(crate) async fn delete_grafana_token(
    wake: State<'_, WakeState>,
    epoch: State<'_, TokenEpochState>,
    name: String,
) -> CommandResult<()> {
    blocking(move || grafana::delete_token(&name)).await?;
    epoch.0.bump();
    wake.0.notify_one();
    Ok(())
}

// The snapshot mutexes guard a plain value that is replaced wholesale, so
// a panic elsewhere cannot leave it half-written: recovering from poison
// is safe.

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned State parameters"
)]
pub(crate) fn get_latest_metrics(state: State<'_, LatestMetrics>) -> Option<MetricsUpdate> {
    state
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned State parameters"
)]
pub(crate) fn get_latest_alerts(state: State<'_, LatestAlerts>) -> Option<AlertsUpdate> {
    state
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned parameters"
)]
pub(crate) fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned parameters"
)]
pub(crate) fn open_url(url: String) -> CommandResult<()> {
    // http(s) only: refuse file://, custom schemes, or app launches. The
    // URL is passed to `open` as a single argv entry (no shell), so
    // query-string characters cannot be interpreted as shell syntax.
    // Scheme is case-insensitive per RFC 3986.
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        return Err(CommandError::UrlScheme);
    }
    std::process::Command::new("open")
        .arg(&url)
        .spawn()
        .map_err(CommandError::OpenUrl)?;
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use std::path::PathBuf;

    use super::{CommandError, open_url, validate_new_server};
    use crate::config::{K8sTarget, ServerConfig, SshTarget};

    #[tokio::test]
    async fn new_k8s_server_with_missing_kubeconfig_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("hippius1-oct.yaml");
        let server = ServerConfig::K8s(K8sTarget {
            name: "hippius".into(),
            kubeconfig: Some(missing.clone()),
            context: "hippius".to_string(),
            namespace: "default".to_string(),
        });

        let err = validate_new_server(&server)
            .await
            .expect_err("missing kubeconfig must be rejected");

        let rendered = serde_json::to_value(&err).expect("serialize");
        let rendered = rendered.as_str().expect("errors serialize as strings");
        assert!(
            rendered.contains(&missing.display().to_string()),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn new_ssh_server_is_not_validated_up_front() {
        let server = ServerConfig::Ssh(SshTarget {
            name: "bastion".into(),
            host: "10.0.1.50".to_string(),
            port: 22,
            user: "admin".to_string(),
            key_path: PathBuf::from("/does/not/exist"),
        });

        validate_new_server(&server)
            .await
            .expect("ssh entries are checked by connecting, not on add");
    }

    #[test]
    fn errors_serialize_with_their_cause_chain() {
        let err = CommandError::OpenUrl(std::io::Error::other("no browser"));

        let json = serde_json::to_value(&err).expect("serialize");

        assert_eq!(json, "failed to open url: no browser");
    }

    #[test]
    fn open_url_refuses_non_http_schemes() {
        for url in ["file:///etc/passwd", "javascript:alert(1)", "ssh://box", ""] {
            assert!(matches!(
                open_url(url.to_string()),
                Err(CommandError::UrlScheme)
            ));
        }
    }
}

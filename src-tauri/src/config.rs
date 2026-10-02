//! Persisted app configuration: model, validation, and JSON storage.

use std::borrow::Borrow;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Accepted foreground poll interval, matching the settings form.
const FOREGROUND_POLL_SECS: std::ops::RangeInclusive<u64> = 5..=120;

/// Accepted background poll interval, matching the settings form.
const BACKGROUND_POLL_SECS: std::ops::RangeInclusive<u64> = 30..=600;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_foreground_poll")]
    pub foreground_poll_secs: u64,
    #[serde(default = "default_background_poll")]
    pub background_poll_secs: u64,
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
    #[serde(default)]
    pub notifications_enabled: bool,
    #[serde(default)]
    pub grafana: Option<GrafanaConfig>,
}

fn default_foreground_poll() -> u64 {
    10
}

fn default_background_poll() -> u64 {
    300
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            foreground_poll_secs: default_foreground_poll(),
            background_poll_secs: default_background_poll(),
            servers: Vec::new(),
            notifications_enabled: false,
            grafana: None,
        }
    }
}

impl AppConfig {
    /// Poll interval while the popover is open. Clamped, since a file
    /// edited by hand may hold any value.
    #[must_use]
    pub fn foreground_interval(&self) -> Duration {
        Duration::from_secs(clamp_to(self.foreground_poll_secs, &FOREGROUND_POLL_SECS))
    }

    /// Poll interval while the popover is hidden. Clamped like
    /// [`Self::foreground_interval`].
    #[must_use]
    pub fn background_interval(&self) -> Duration {
        Duration::from_secs(clamp_to(self.background_poll_secs, &BACKGROUND_POLL_SECS))
    }

    /// The server named `name`, if any.
    #[must_use]
    pub fn server(&self, name: &str) -> Option<&ServerConfig> {
        self.servers.iter().find(|s| s.name().as_str() == name)
    }

    /// Check the config invariants a write must keep (poll intervals in the form's
    /// range; unique server names without `/`), but only for what differs
    /// from `previous`: changed intervals and servers that were not there
    /// before. A legacy file (a `/` in a name, a hand-edited interval) must
    /// not block unrelated edits such as removing a different server. The
    /// Grafana URL is checked separately, by `grafana::validate_url`, when
    /// settings are saved.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Invalid`] or [`ConfigError::DuplicateServer`] for the
    /// first new violation.
    pub fn validate_change_from(&self, previous: &AppConfig) -> Result<(), ConfigError> {
        let intervals_changed = self.foreground_poll_secs != previous.foreground_poll_secs
            || self.background_poll_secs != previous.background_poll_secs;
        if intervals_changed {
            check_intervals(self)?;
        }

        for server in &self.servers {
            if !previous.servers.contains(server) {
                server.name().validate()?;
            }
            // A name shared by more servers than before is a new duplicate,
            // even when the added entry is an exact copy of an existing one.
            let name = server.name();
            if count_named(self, name) > count_named(previous, name).max(1) {
                return Err(ConfigError::DuplicateServer(name.clone()));
            }
        }
        Ok(())
    }
}

fn count_named(config: &AppConfig, name: &ServerName) -> usize {
    config.servers.iter().filter(|s| s.name() == name).count()
}

fn check_intervals(config: &AppConfig) -> Result<(), ConfigError> {
    if !FOREGROUND_POLL_SECS.contains(&config.foreground_poll_secs) {
        return Err(invalid(format!(
            "foreground poll interval must be {}-{} seconds",
            FOREGROUND_POLL_SECS.start(),
            FOREGROUND_POLL_SECS.end()
        )));
    }
    if !BACKGROUND_POLL_SECS.contains(&config.background_poll_secs) {
        return Err(invalid(format!(
            "background poll interval must be {}-{} seconds",
            BACKGROUND_POLL_SECS.start(),
            BACKGROUND_POLL_SECS.end()
        )));
    }
    Ok(())
}

fn clamp_to(value: u64, range: &std::ops::RangeInclusive<u64>) -> u64 {
    value.clamp(*range.start(), *range.end())
}

fn invalid(reason: String) -> ConfigError {
    ConfigError::Invalid { reason }
}

/// A configured server's name: its identity in the config, the poller
/// and the UI.
///
/// Pod cards are keyed `cluster/pod` in the UI, so a `/` in a cluster
/// name would make those keys ambiguous; [`Self::validate`] rejects it.
/// Deserialization does not validate, so a config written before the
/// rule existed still loads; every write validates.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ServerName(String);

impl ServerName {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// # Errors
    ///
    /// [`ConfigError::Invalid`] if the name is blank or contains `/`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.0.trim().is_empty() {
            return Err(invalid("server name must not be empty".to_string()));
        }
        if self.0.contains('/') {
            return Err(invalid(format!(
                "server name '{}' must not contain '/'",
                self.0
            )));
        }
        Ok(())
    }
}

impl fmt::Display for ServerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Borrow<str> for ServerName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ServerName {
    fn from(name: &str) -> Self {
        Self(name.to_string())
    }
}

/// Connection details for a single Grafana instance whose alerts the
/// app displays. The API token is NOT stored here: it lives in the OS
/// keychain, keyed by `name` (see `grafana::read_token`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GrafanaConfig {
    /// Display label and keychain key for this connection.
    pub name: String,
    /// Base URL, e.g. `https://grafana.internal` (no trailing path).
    pub url: String,
    /// Verify TLS certificates. Defaults to true; set false only for a
    /// self-signed instance you trust.
    #[serde(default = "default_verify_tls")]
    pub verify_tls: bool,
    #[serde(default)]
    pub enabled: bool,
}

fn default_verify_tls() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerConfig {
    K8s(K8sTarget),
    Ssh(SshTarget),
}

/// A Kubernetes cluster: which kubeconfig context to use and which
/// namespace's pods to show.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct K8sTarget {
    pub name: ServerName,
    /// `None` uses `$KUBECONFIG` / `~/.kube/config`. May start with `~/`.
    pub kubeconfig: Option<PathBuf>,
    pub context: String,
    pub namespace: String,
}

/// An SSH host reached with key authentication.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SshTarget {
    pub name: ServerName,
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub user: String,
    /// Private key file. May start with `~/`.
    pub key_path: PathBuf,
}

fn default_ssh_port() -> u16 {
    22
}

impl ServerConfig {
    #[must_use]
    pub fn name(&self) -> &ServerName {
        match self {
            ServerConfig::K8s(target) => &target.name,
            ServerConfig::Ssh(target) => &target.name,
        }
    }
}

/// Failure categories for config persistence and validation. I/O and
/// serde variants keep their cause in the source chain so the command
/// boundary can render the whole chain.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not determine the user config directory")]
    NoConfigDir,
    #[error("failed to read config file {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create config directory {}", path.display())]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write config file {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file {}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize config")]
    Serialize(#[source] serde_json::Error),
    #[error("failed to rename config file into place: {}", path.display())]
    Rename {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("config save task failed")]
    SaveTask(#[source] tokio::task::JoinError),
    #[error("invalid config: {reason}")]
    Invalid { reason: String },
    #[error("server '{0}' already exists")]
    DuplicateServer(ServerName),
}

/// The settings-form fields, saved without touching the server list so
/// a settings save can never overwrite a server added in the meantime.
#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    pub foreground_poll_secs: u64,
    pub background_poll_secs: u64,
    pub notifications_enabled: bool,
    pub grafana: Option<GrafanaConfig>,
}

impl Settings {
    pub fn apply_to(self, config: &mut AppConfig) {
        config.foreground_poll_secs = self.foreground_poll_secs;
        config.background_poll_secs = self.background_poll_secs;
        config.notifications_enabled = self.notifications_enabled;
        config.grafana = self.grafana;
    }
}

/// The live config and the file it is persisted to, shared by the IPC
/// commands and the poller.
///
/// An async mutex, held across the save, serializes writers: two
/// concurrent edits cannot both start from the same snapshot and have
/// the second silently drop the first.
pub struct ConfigStore {
    path: PathBuf,
    current: tokio::sync::Mutex<AppConfig>,
}

impl ConfigStore {
    #[must_use]
    pub fn new(path: PathBuf, config: AppConfig) -> Self {
        Self {
            path,
            current: tokio::sync::Mutex::new(config),
        }
    }

    pub async fn snapshot(&self) -> AppConfig {
        self.current.lock().await.clone()
    }

    /// Apply `edit` to a copy of the config, validate it, persist it, and
    /// only then make it current. Returns the new config.
    ///
    /// # Errors
    ///
    /// Whatever `edit` returns, or a [`ConfigError`] from validation or
    /// the save; on error the current config is unchanged.
    pub async fn update<E>(
        &self,
        edit: impl FnOnce(&mut AppConfig) -> Result<(), E>,
    ) -> Result<AppConfig, E>
    where
        E: From<ConfigError>,
    {
        let mut current = self.current.lock().await;
        let mut next = current.clone();
        edit(&mut next)?;
        next.validate_change_from(&current)?;

        let path = self.path.clone();
        let to_save = next.clone();
        tokio::task::spawn_blocking(move || save_config_to(&path, &to_save))
            .await
            .map_err(ConfigError::SaveTask)??;

        current.clone_from(&next);
        Ok(next)
    }
}

/// Expand a leading `~/` (or a lone `~`) to the user's home directory.
/// Other paths are returned unchanged.
#[must_use]
pub fn expand_tilde(path: &Path) -> PathBuf {
    let Ok(rest) = path.strip_prefix("~") else {
        return path.to_path_buf();
    };
    match dirs::home_dir() {
        Some(home) => home.join(rest),
        None => path.to_path_buf(),
    }
}

/// Directory a file picker opens in: the folder of the path already in the
/// form when that folder exists, else `fallback` (e.g. `~/.kube`) when it
/// exists, else `None` for the OS default.
#[must_use]
pub fn picker_start_dir(current: Option<&str>, fallback: Option<&Path>) -> Option<PathBuf> {
    let from_current = current
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| expand_tilde(Path::new(p)))
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .filter(|dir| dir.is_dir());
    if from_current.is_some() {
        return from_current;
    }

    fallback.filter(|dir| dir.is_dir()).map(Path::to_path_buf)
}

/// Returns the config file path: `<config dir>/observer-ward/config.json`,
/// i.e. `~/Library/Application Support/...` on macOS and
/// `~/.config/...` on Linux (`dirs::config_dir`).
///
/// # Errors
///
/// [`ConfigError::NoConfigDir`] if the platform config dir is unknown.
pub fn config_path() -> Result<PathBuf, ConfigError> {
    let config_dir = dirs::config_dir().ok_or(ConfigError::NoConfigDir)?;
    Ok(config_dir.join("observer-ward").join("config.json"))
}

/// Load the config at `path`, or the default when no file exists yet.
///
/// # Errors
///
/// Returns [`ConfigError`] if the file cannot be read or its contents
/// are not valid config JSON.
pub fn load_config_from(path: &Path) -> Result<AppConfig, ConfigError> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(AppConfig::default()),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    serde_json::from_str(&contents).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

/// Load the config, falling back to the default on any failure. A file
/// that exists but cannot be read or parsed is first copied aside to
/// `config.json.invalid`, so the next save cannot silently replace the
/// user's servers with the empty default.
#[must_use]
pub fn load_config_or_default(path: &Path) -> AppConfig {
    match load_config_from(path) {
        Ok(config) => config,
        Err(e) => {
            tracing::error!(
                "failed to load config, using defaults: {}",
                crate::error::error_chain(&e)
            );
            // NotFound already returned the default above, so any error means a
            // file exists that the next save would replace.
            preserve_invalid(path);
            AppConfig::default()
        }
    }
}

fn preserve_invalid(path: &Path) {
    let backup = path.with_extension("json.invalid");
    match std::fs::copy(path, &backup) {
        Ok(_) => tracing::warn!("kept the unreadable config as {}", backup.display()),
        Err(e) => tracing::error!(
            "could not back up unreadable config to {}: {e}",
            backup.display()
        ),
    }
}

/// Persist `config` to `path` atomically (write a temp file, then rename).
///
/// Blocking file IO: async callers should use `spawn_blocking`.
///
/// # Errors
///
/// Returns [`ConfigError`] if the directory cannot be created, the config
/// cannot be serialized, or the temp file cannot be written or renamed.
pub fn save_config_to(path: &Path, config: &AppConfig) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let json = serde_json::to_string_pretty(config).map_err(ConfigError::Serialize)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).map_err(|source| ConfigError::Write {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, path).map_err(|source| ConfigError::Rename {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::*;
    use std::fs;

    /// Validate `config` as a brand-new file: every server and both
    /// intervals count as changed.
    fn validate_fresh(config: &AppConfig) -> Result<(), ConfigError> {
        config.validate_change_from(&AppConfig {
            foreground_poll_secs: 0,
            background_poll_secs: 0,
            servers: Vec::new(),
            ..AppConfig::default()
        })
    }

    fn ssh(name: &str) -> ServerConfig {
        ServerConfig::Ssh(SshTarget {
            name: name.into(),
            host: "10.0.0.5".to_string(),
            port: 22,
            user: "deploy".to_string(),
            key_path: PathBuf::from("/home/user/.ssh/id_ed25519"),
        })
    }

    fn k8s(name: &str) -> ServerConfig {
        ServerConfig::K8s(K8sTarget {
            name: name.into(),
            kubeconfig: None,
            context: "ctx".to_string(),
            namespace: "default".to_string(),
        })
    }

    #[test]
    fn picker_starts_in_folder_of_current_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let current = dir.path().join("does-not-need-to-exist.yaml");

        let start = picker_start_dir(Some(&current.display().to_string()), None);

        assert_eq!(start.as_deref(), Some(dir.path()));
    }

    #[test]
    fn picker_falls_back_only_to_an_existing_dir() {
        let home = tempfile::tempdir().expect("tempdir");
        let fallback = home.path().join(".ssh");

        assert_eq!(picker_start_dir(Some("  "), Some(&fallback)), None);

        fs::create_dir(&fallback).expect("create fallback dir");
        let missing_parent = home.path().join("gone").join("id_ed25519");

        let start = picker_start_dir(Some(&missing_parent.display().to_string()), Some(&fallback));

        assert_eq!(start, Some(fallback));
    }

    #[test]
    fn expand_tilde_leaves_absolute_and_relative_paths() {
        for path in ["/etc/ssh/id", "keys/id_ed25519", "", "~foo/.ssh/id"] {
            assert_eq!(expand_tilde(Path::new(path)), PathBuf::from(path));
        }
    }

    #[test]
    fn expand_tilde_prefixes_home() {
        let home = dirs::home_dir().expect("home dir");
        assert_eq!(
            expand_tilde(Path::new("~/.ssh/id_ed25519")),
            home.join(".ssh/id_ed25519")
        );
        assert_eq!(expand_tilde(Path::new("~")), home);
        assert_eq!(expand_tilde(Path::new("~/")), home);
    }

    #[test]
    fn server_json_shape_is_stable() {
        let json = serde_json::to_value(ssh("web-box")).expect("serialize");
        assert_eq!(json["type"], "ssh");
        assert_eq!(json["name"], "web-box");
        assert_eq!(json["host"], "10.0.0.5");
        assert_eq!(json["port"], 22);
        assert_eq!(json["user"], "deploy");
        assert_eq!(json["key_path"], "/home/user/.ssh/id_ed25519");

        let json = serde_json::to_value(k8s("prod")).expect("serialize");
        assert_eq!(json["type"], "k8s");
        assert_eq!(json["name"], "prod");
        assert!(json["kubeconfig"].is_null());
        assert_eq!(json["context"], "ctx");
        assert_eq!(json["namespace"], "default");
    }

    #[test]
    fn deserialize_k8s_server() {
        let json = r#"{
            "type": "k8s",
            "name": "staging",
            "kubeconfig": "~/.kube/staging",
            "context": "staging-ctx",
            "namespace": "kube-system"
        }"#;
        let server: ServerConfig = serde_json::from_str(json).expect("deserialize");

        let ServerConfig::K8s(target) = server else {
            panic!("expected a k8s server, got {server:?}");
        };
        assert_eq!(target.name.as_str(), "staging");
        assert_eq!(target.kubeconfig, Some(PathBuf::from("~/.kube/staging")));
        assert_eq!(target.context, "staging-ctx");
        assert_eq!(target.namespace, "kube-system");
    }

    #[test]
    fn deserialize_ssh_server_default_port() {
        let json = r#"{
            "type": "ssh",
            "name": "bastion",
            "host": "bastion.example.com",
            "user": "admin",
            "key_path": "/root/.ssh/id_rsa"
        }"#;
        let server: ServerConfig = serde_json::from_str(json).expect("deserialize");

        let ServerConfig::Ssh(target) = server else {
            panic!("expected an ssh server, got {server:?}");
        };
        assert_eq!(target.port, 22);
    }

    #[test]
    fn unknown_server_type_returns_error() {
        let json = r#"{ "type": "docker", "name": "container-host" }"#;
        assert!(serde_json::from_str::<ServerConfig>(json).is_err());
    }

    #[test]
    fn empty_json_uses_defaults() {
        let config: AppConfig = serde_json::from_str("{}").expect("deserialize");

        assert_eq!(config.foreground_poll_secs, 10);
        assert_eq!(config.background_poll_secs, 300);
        assert!(config.servers.is_empty());
        assert!(config.grafana.is_none());
        validate_fresh(&config).expect("defaults are valid");
    }

    #[test]
    fn grafana_config_defaults_verify_tls_on() {
        let json =
            r#"{ "grafana": { "name": "home", "url": "https://g.internal", "enabled": true } }"#;
        let config: AppConfig = serde_json::from_str(json).expect("deserialize");

        let grafana = config.grafana.expect("grafana present");
        assert_eq!(grafana.name, "home");
        assert!(grafana.enabled);
        assert!(grafana.verify_tls);
    }

    #[test]
    fn validate_rejects_bad_names_and_duplicates() {
        let mut config = AppConfig {
            servers: vec![ssh("a"), k8s("b")],
            ..AppConfig::default()
        };
        validate_fresh(&config).expect("valid");

        config.servers.push(ssh("a"));
        assert!(matches!(
            validate_fresh(&config),
            Err(ConfigError::DuplicateServer(name)) if name.as_str() == "a"
        ));

        for bad in ["", "   ", "prod/eu"] {
            config.servers = vec![k8s(bad)];
            assert!(
                matches!(validate_fresh(&config), Err(ConfigError::Invalid { .. })),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn validate_rejects_out_of_range_intervals_and_accessors_clamp() {
        let config = AppConfig {
            foreground_poll_secs: 1,
            background_poll_secs: 100_000,
            ..AppConfig::default()
        };

        assert!(matches!(
            validate_fresh(&config),
            Err(ConfigError::Invalid { .. })
        ));
        assert_eq!(config.foreground_interval(), Duration::from_secs(5));
        assert_eq!(config.background_interval(), Duration::from_secs(600));
    }

    #[test]
    fn server_lookup_by_name() {
        let config = AppConfig {
            servers: vec![ssh("a"), k8s("b")],
            ..AppConfig::default()
        };

        assert!(matches!(config.server("b"), Some(ServerConfig::K8s(_))));
        assert!(config.server("c").is_none());
    }

    #[test]
    fn save_then_load_round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("config.json");
        let config = AppConfig {
            foreground_poll_secs: 15,
            background_poll_secs: 60,
            servers: vec![ssh("local"), k8s("cluster")],
            ..AppConfig::default()
        };

        save_config_to(&path, &config).expect("save");
        let loaded = load_config_from(&path).expect("load");

        assert_eq!(loaded.foreground_poll_secs, 15);
        assert_eq!(loaded.background_poll_secs, 60);
        assert_eq!(loaded.servers, config.servers);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().expect("tempdir");

        let config = load_config_from(&dir.path().join("config.json")).expect("load");

        assert!(config.servers.is_empty());
    }

    #[tokio::test]
    async fn store_update_persists_before_publishing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.json");
        let store = ConfigStore::new(path.clone(), AppConfig::default());

        let next = store
            .update(|c| {
                c.servers.push(ssh("a"));
                Ok::<_, ConfigError>(())
            })
            .await
            .expect("update");

        assert_eq!(next.servers.len(), 1);
        assert_eq!(store.snapshot().await.servers, next.servers);
        assert_eq!(load_config_from(&path).expect("load").servers, next.servers);
    }

    #[tokio::test]
    async fn store_update_rejected_by_validation_changes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.json");
        let store = ConfigStore::new(path.clone(), AppConfig::default());

        let result = store
            .update(|c| {
                c.servers.push(k8s("prod/eu"));
                Ok::<_, ConfigError>(())
            })
            .await;

        assert!(matches!(result, Err(ConfigError::Invalid { .. })));
        assert!(store.snapshot().await.servers.is_empty());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn failed_save_leaves_the_current_config_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blocker = dir.path().join("not-a-dir");
        fs::write(&blocker, "file where the config dir should be").expect("write");
        let store = ConfigStore::new(blocker.join("config.json"), AppConfig::default());

        let result = store
            .update(|c| {
                c.servers.push(ssh("a"));
                Ok::<_, ConfigError>(())
            })
            .await;

        assert!(matches!(result, Err(ConfigError::CreateDir { .. })));
        assert!(store.snapshot().await.servers.is_empty());
    }

    #[tokio::test]
    async fn concurrent_updates_do_not_lose_each_other() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.json");
        let store = std::sync::Arc::new(ConfigStore::new(path.clone(), AppConfig::default()));

        let edits = (0..8).map(|i| {
            let store = std::sync::Arc::clone(&store);
            tokio::spawn(async move {
                store
                    .update(|c| {
                        c.servers.push(ssh(&format!("s{i}")));
                        Ok::<_, ConfigError>(())
                    })
                    .await
            })
        });
        for edit in edits.collect::<Vec<_>>() {
            edit.await.expect("join").expect("update");
        }

        assert_eq!(store.snapshot().await.servers.len(), 8);
        assert_eq!(load_config_from(&path).expect("load").servers.len(), 8);
    }

    #[tokio::test]
    async fn legacy_entries_do_not_block_unrelated_edits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let legacy = AppConfig {
            foreground_poll_secs: 1,
            servers: vec![k8s("prod/eu"), ssh("other")],
            ..AppConfig::default()
        };
        let store = ConfigStore::new(dir.path().join("config.json"), legacy);

        let next = store
            .update(|c| {
                c.servers.retain(|s| s.name().as_str() != "other");
                c.servers.push(ssh("new"));
                Ok::<_, ConfigError>(())
            })
            .await
            .expect("unrelated edit succeeds despite legacy entries");

        assert_eq!(next.servers.len(), 2);

        let rejected = store
            .update(|c| {
                c.servers.push(ssh("new"));
                Ok::<_, ConfigError>(())
            })
            .await;
        assert!(matches!(rejected, Err(ConfigError::DuplicateServer(_))));

        let rejected = store
            .update(|c| {
                c.background_poll_secs = 5;
                Ok::<_, ConfigError>(())
            })
            .await;
        assert!(matches!(rejected, Err(ConfigError::Invalid { .. })));
    }

    #[test]
    fn unreadable_config_is_preserved_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.json");
        fs::write(&path, [0xff, 0xfe, b'{']).expect("write");

        assert!(matches!(
            load_config_from(&path),
            Err(ConfigError::Read { .. })
        ));
        let _ = load_config_or_default(&path);

        let backup = fs::read(path.with_extension("json.invalid")).expect("backup");
        assert_eq!(backup, [0xff, 0xfe, b'{']);
    }

    #[test]
    fn settings_leave_servers_alone() {
        let mut config = AppConfig {
            servers: vec![ssh("a")],
            ..AppConfig::default()
        };
        let settings = Settings {
            foreground_poll_secs: 20,
            background_poll_secs: 120,
            notifications_enabled: true,
            grafana: None,
        };

        settings.apply_to(&mut config);

        assert_eq!(config.foreground_poll_secs, 20);
        assert!(config.notifications_enabled);
        assert_eq!(config.servers.len(), 1);
    }

    #[test]
    fn unparseable_config_is_reported_and_preserved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.json");
        fs::write(&path, "{ not valid json }").expect("write");

        assert!(matches!(
            load_config_from(&path),
            Err(ConfigError::Parse { .. })
        ));

        let config = load_config_or_default(&path);

        assert!(config.servers.is_empty());
        let backup = fs::read_to_string(path.with_extension("json.invalid")).expect("backup");
        assert_eq!(backup, "{ not valid json }");
    }
}

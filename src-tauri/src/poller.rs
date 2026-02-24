use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::image::Image;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio::time::sleep;

use crate::config::{AppConfig, ServerConfig};
use crate::k8s_backend::K8sBackend;
use crate::metrics::{
    classify_level, has_restarts, worst_level, MetricLevel, MetricsUpdate, ServerMetrics,
    ServerStatus,
};
use crate::ssh_backend::SshBackend;
use crate::TrayState;

const COLLECT_TIMEOUT: Duration = Duration::from_secs(30);
const BACKOFF_THRESHOLD: u32 = 3;
const BACKOFF_DURATION: Duration = Duration::from_secs(120);

struct FailureState {
    count: u32,
    last_attempt: Instant,
}

enum BackendEntry {
    Ssh(SshBackend),
    K8s(K8sBackend),
}

pub struct Poller {
    app_handle: AppHandle,
    config_state: Arc<Mutex<AppConfig>>,
    is_visible: Arc<AtomicBool>,
    wake: Arc<Notify>,
    ssh_backends: HashMap<String, SshBackend>,
    k8s_backends: HashMap<String, K8sBackend>,
    failures: HashMap<String, FailureState>,
    prev_levels: HashMap<String, [MetricLevel; 3]>,
}

impl Poller {
    pub fn new(
        app_handle: AppHandle,
        config_state: Arc<Mutex<AppConfig>>,
        is_visible: Arc<AtomicBool>,
        wake: Arc<Notify>,
    ) -> Self {
        Self {
            app_handle,
            config_state,
            is_visible,
            wake,
            ssh_backends: HashMap::new(),
            k8s_backends: HashMap::new(),
            failures: HashMap::new(),
            prev_levels: HashMap::new(),
        }
    }

    pub async fn run(&mut self) {
        loop {
            let snapshot = self.config_state.lock().ok().map(|c| c.clone());

            let Some(config) = snapshot else {
                tracing::error!("config lock poisoned");
                sleep(Duration::from_secs(5)).await;
                continue;
            };

            let foreground_interval = config.foreground_poll_secs.max(5);
            let background_interval = config.background_poll_secs.max(30);
            let servers = config.servers;
            let notifications_enabled = config.notifications_enabled;

            self.cleanup_removed_backends(&servers);

            if let Err(e) = self.app_handle.emit("poll-start", ()) {
                tracing::warn!("failed to emit poll-start: {e}");
            }

            let all_metrics = self.poll_all_servers(&servers).await;

            let update = MetricsUpdate {
                servers: all_metrics,
            };
            if let Err(e) = self.app_handle.emit("metrics-update", &update) {
                tracing::warn!("failed to emit metrics-update: {e}");
            }

            self.check_and_notify(notifications_enabled, &update.servers);
            self.update_tray_icon(&update.servers);

            let interval = if self.is_visible.load(Ordering::Relaxed) {
                foreground_interval
            } else {
                background_interval
            };
            tokio::select! {
                () = sleep(Duration::from_secs(interval)) => {}
                () = self.wake.notified() => {}
            }
        }
    }

    /// Poll all servers concurrently, returning aggregated
    /// metrics. Servers in backoff are skipped with offline
    /// status.
    async fn poll_all_servers(&mut self, servers: &[ServerConfig]) -> Vec<ServerMetrics> {
        let mut skipped = Vec::new();
        let mut tasks = JoinSet::new();

        for server in servers {
            let name = server.name().to_string();
            let stype = server.server_type().to_string();

            if self.in_backoff(&name) {
                skipped.push(offline_metrics(&name, &stype));
                continue;
            }

            let entry = self.take_or_create_backend(server);
            let server = server.clone();

            tasks.spawn(async move {
                let result =
                    tokio::time::timeout(COLLECT_TIMEOUT, collect_with_entry(entry, &server)).await;

                if let Ok((entry, inner)) = result {
                    (name, stype, Some(entry), inner)
                } else {
                    // Timeout — drop the stale backend
                    let msg = format!(
                        "timed out collecting metrics \
                         for {name}"
                    );
                    (name, stype, None, Err(msg))
                }
            });
        }

        let mut all_metrics = skipped;
        while let Some(join_result) = tasks.join_next().await {
            let Ok((name, stype, entry, result)) = join_result else {
                tracing::warn!("poll task panicked");
                continue;
            };

            if let Some(entry) = entry {
                self.put_backend_back(&name, entry);
            }

            match result {
                Ok(metrics) => {
                    self.failures.remove(&name);
                    all_metrics.extend(metrics);
                }
                Err(e) => {
                    tracing::warn!("failed to collect metrics for {name}: {e}");
                    self.record_failure(&name);
                    all_metrics.push(offline_metrics(&name, &stype));
                }
            }
        }

        all_metrics
    }

    /// Extract an existing backend from the cache, or create a
    /// new one. Stale backends (config changed) are dropped and
    /// recreated.
    fn take_or_create_backend(&mut self, server: &ServerConfig) -> BackendEntry {
        match server {
            ServerConfig::Ssh {
                name,
                host,
                port,
                user,
                key_path,
            } => {
                let existing = self.ssh_backends.remove(name);
                let backend = match existing {
                    Some(b) if b.matches_config(host, *port, user, key_path) => b,
                    _ => SshBackend::new(host.clone(), *port, user.clone(), key_path.clone()),
                };
                BackendEntry::Ssh(backend)
            }
            ServerConfig::K8s {
                name,
                kubeconfig,
                context,
                ..
            } => {
                let existing = self.k8s_backends.remove(name);
                let backend = match existing {
                    Some(b) if b.matches_config(kubeconfig.as_ref(), context) => b,
                    _ => K8sBackend::new(kubeconfig.clone(), context.clone()),
                };
                BackendEntry::K8s(backend)
            }
        }
    }

    fn put_backend_back(&mut self, name: &str, entry: BackendEntry) {
        match entry {
            BackendEntry::Ssh(b) => {
                self.ssh_backends.insert(name.to_string(), b);
            }
            BackendEntry::K8s(b) => {
                self.k8s_backends.insert(name.to_string(), b);
            }
        }
    }

    /// Check whether a server is in backoff. When the backoff
    /// period expires, the failure counter is reset to give a
    /// fresh set of `BACKOFF_THRESHOLD` attempts.
    fn in_backoff(&mut self, name: &str) -> bool {
        let Some(state) = self.failures.get_mut(name) else {
            return false;
        };
        if state.count >= BACKOFF_THRESHOLD {
            if state.last_attempt.elapsed() < BACKOFF_DURATION {
                return true;
            }
            // Backoff expired — reset counter for fresh attempts
            state.count = 0;
        }
        false
    }

    fn record_failure(&mut self, name: &str) {
        let state = self
            .failures
            .entry(name.to_string())
            .or_insert_with(|| FailureState {
                count: 0,
                last_attempt: Instant::now(),
            });
        state.count = state.count.saturating_add(1);
        state.last_attempt = Instant::now();
    }

    fn update_tray_icon(&self, metrics: &[ServerMetrics]) {
        let level = worst_level(metrics);
        let restarts = has_restarts(metrics);
        let Some(state) = self.app_handle.try_state::<TrayState>() else {
            return;
        };
        let Ok(tray) = state.0.lock() else {
            return;
        };

        let tooltip = if restarts {
            "Observer Ward — restart detected"
        } else {
            match level {
                MetricLevel::Ok => "Observer Ward — all clear",
                MetricLevel::Warn => "Observer Ward — warning",
                MetricLevel::Crit => "Observer Ward — critical",
            }
        };

        if let Err(e) = tray.set_tooltip(Some(tooltip)) {
            tracing::warn!("failed to set tray tooltip: {e}");
        }

        if restarts {
            let icon = Image::from_bytes(include_bytes!("../icons/tray-restart.png"));
            if let Ok(img) = icon {
                if let Err(e) = tray.set_icon(Some(img)) {
                    tracing::warn!("failed to set restart tray icon: {e}");
                }
                if let Err(e) = tray.set_icon_as_template(false) {
                    tracing::warn!("failed to unset icon template: {e}");
                }
            }
        } else {
            match level {
                MetricLevel::Ok => {
                    if let Some(icon) = self.app_handle.default_window_icon() {
                        if let Err(e) = tray.set_icon(Some(icon.clone())) {
                            tracing::warn!("failed to set tray icon: {e}");
                        }
                        if let Err(e) = tray.set_icon_as_template(true) {
                            tracing::warn!("failed to set icon as template: {e}");
                        }
                    }
                }
                MetricLevel::Warn => {
                    let icon = Image::from_bytes(include_bytes!("../icons/tray-warn.png"));
                    if let Ok(img) = icon {
                        if let Err(e) = tray.set_icon(Some(img)) {
                            tracing::warn!("failed to set warn tray icon: {e}");
                        }
                        if let Err(e) = tray.set_icon_as_template(false) {
                            tracing::warn!("failed to unset icon template: {e}");
                        }
                    }
                }
                MetricLevel::Crit => {
                    let icon = Image::from_bytes(include_bytes!("../icons/tray-crit.png"));
                    if let Ok(img) = icon {
                        if let Err(e) = tray.set_icon(Some(img)) {
                            tracing::warn!("failed to set crit tray icon: {e}");
                        }
                        if let Err(e) = tray.set_icon_as_template(false) {
                            tracing::warn!("failed to unset icon template: {e}");
                        }
                    }
                }
            }
        }
    }

    fn check_and_notify(&mut self, enabled: bool, metrics: &[ServerMetrics]) {
        if !enabled {
            return;
        }
        let metric_names = ["CPU", "MEM", "DISK"];
        for m in metrics {
            if m.status != ServerStatus::Online {
                // Reset so notifications re-fire on recovery
                self.prev_levels.remove(&m.server_name);
                continue;
            }
            let levels = [
                classify_level(m.cpu_percent),
                classify_level(m.memory_percent),
                classify_level(m.disk_percent),
            ];
            let percents = [m.cpu_percent, m.memory_percent, m.disk_percent];
            let prev = self
                .prev_levels
                .get(&m.server_name)
                .copied()
                .unwrap_or([MetricLevel::Ok; 3]);

            for i in 0..3 {
                if levels[i] > prev[i] {
                    self.send_notification(&m.server_name, metric_names[i], levels[i], percents[i]);
                }
            }
            self.prev_levels.insert(m.server_name.clone(), levels);
        }
    }

    fn send_notification(&self, server_name: &str, metric: &str, level: MetricLevel, value: f64) {
        use tauri_plugin_notification::NotificationExt;

        let level_str = match level {
            MetricLevel::Ok => "OK",
            MetricLevel::Warn => "WARNING",
            MetricLevel::Crit => "CRITICAL",
        };

        let title = format!("{server_name}: {metric} {level_str}");
        let body = format!("{metric} at {value:.0}%");

        if let Err(e) = self
            .app_handle
            .notification()
            .builder()
            .title(&title)
            .body(&body)
            .show()
        {
            tracing::warn!("failed to send notification for {server_name}: {e}");
        }
    }

    fn cleanup_removed_backends(&mut self, servers: &[ServerConfig]) {
        let active: HashSet<&str> = servers.iter().map(ServerConfig::name).collect();

        self.ssh_backends
            .retain(|name, _| active.contains(name.as_str()));
        self.k8s_backends
            .retain(|name, _| active.contains(name.as_str()));
        self.failures
            .retain(|name, _| active.contains(name.as_str()));
        self.prev_levels
            .retain(|name, _| active.contains(name.as_str()));
    }
}

/// Collect metrics using an extracted backend. Returns the
/// backend alongside the result so it can be put back.
async fn collect_with_entry(
    mut entry: BackendEntry,
    server: &ServerConfig,
) -> (BackendEntry, Result<Vec<ServerMetrics>, String>) {
    let result = match (&mut entry, server) {
        (BackendEntry::Ssh(backend), ServerConfig::Ssh { name, .. }) => {
            if !backend.is_connected() {
                if let Err(e) = backend.connect().await {
                    backend.disconnect().await;
                    return (entry, Err(e));
                }
            }
            let result = backend.collect_metrics(name).await;
            if result.is_err() {
                backend.disconnect().await;
            }
            result.map(|m| vec![m])
        }
        (
            BackendEntry::K8s(backend),
            ServerConfig::K8s {
                name, namespace, ..
            },
        ) => backend.collect_all(name, namespace).await,
        _ => Err("backend type mismatch".to_string()),
    };
    (entry, result)
}

fn offline_metrics(name: &str, server_type: &str) -> ServerMetrics {
    ServerMetrics {
        server_name: name.to_string(),
        server_type: server_type.to_string(),
        status: ServerStatus::Offline,
        ..ServerMetrics::default()
    }
}

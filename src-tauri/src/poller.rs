use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio::time::sleep;

use crate::config::{AppConfig, ServerConfig};
use crate::grafana::{GrafanaBackend, read_token};
use crate::k8s::K8sBackend;
use crate::metrics::{
    Alert, AlertSeverity, AlertState, AlertsUpdate, MetricLevel, MetricsUpdate, ServerMetrics,
    ServerStatus, classify_level, has_restarts, newly_firing, worst_alert_level, worst_level,
};
use crate::ssh::SshBackend;
use crate::tray::{TrayIconKind, TrayState};

const COLLECT_TIMEOUT: Duration = Duration::from_secs(30);
const BACKOFF_THRESHOLD: u32 = 3;
const BACKOFF_DURATION: Duration = Duration::from_mins(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackoffDecision {
    Ready,
    Hold,
    Expired,
}

fn backoff_decision(count: u32, elapsed: Duration) -> BackoffDecision {
    if count < BACKOFF_THRESHOLD {
        return BackoffDecision::Ready;
    }
    if elapsed < BACKOFF_DURATION {
        return BackoffDecision::Hold;
    }
    BackoffDecision::Expired
}

/// Crit always wins over a restart badge so a hot cluster is not
/// painted as "just a restart" because of a lifetime restartCount.
fn tray_icon_kind(level: MetricLevel, restarts: bool) -> TrayIconKind {
    match (level, restarts) {
        (MetricLevel::Crit, _) => TrayIconKind::Crit,
        (_, true) => TrayIconKind::Restart,
        (MetricLevel::Warn, false) => TrayIconKind::Warn,
        (MetricLevel::Ok, false) => TrayIconKind::Default,
    }
}

/// Tray input after a Grafana poll. A fetch error is unknown, not
/// all-clear: keep the last successful list across consecutive failures.
fn tray_alerts_after_grafana(fetch: Option<&AlertsUpdate>, last_good: &[Alert]) -> Vec<Alert> {
    match fetch {
        None => Vec::new(),
        Some(update) if update.source_error.is_none() => update.alerts.clone(),
        Some(_) => last_good.to_vec(),
    }
}

struct FailureState {
    count: u32,
    last_attempt: Instant,
    /// Kept so servers held in backoff still report why they are offline.
    last_error: String,
}

impl FailureState {
    fn new() -> Self {
        Self {
            count: 0,
            last_attempt: Instant::now(),
            last_error: String::new(),
        }
    }

    fn record(&mut self, error: String) {
        self.count = self.count.saturating_add(1);
        self.last_attempt = Instant::now();
        self.last_error = error;
    }
}

enum BackendEntry {
    Ssh(SshBackend),
    K8s(K8sBackend),
}

/// Inputs the poll loop needs from Tauri setup. Bundled so `Poller::new`
/// stays within the positional-argument limit.
pub(crate) struct PollerHandles {
    pub(crate) app_handle: AppHandle,
    pub(crate) config_state: Arc<Mutex<AppConfig>>,
    pub(crate) is_visible: Arc<AtomicBool>,
    pub(crate) wake: Arc<Notify>,
    pub(crate) latest_metrics: Arc<Mutex<Option<MetricsUpdate>>>,
    pub(crate) latest_alerts: Arc<Mutex<Option<AlertsUpdate>>>,
}

pub(crate) struct Poller {
    app_handle: AppHandle,
    config_state: Arc<Mutex<AppConfig>>,
    is_visible: Arc<AtomicBool>,
    wake: Arc<Notify>,
    ssh_backends: HashMap<String, SshBackend>,
    k8s_backends: HashMap<String, K8sBackend>,
    failures: HashMap<String, FailureState>,
    prev_levels: HashMap<String, [MetricLevel; 3]>,
    grafana_backend: Option<GrafanaBackend>,
    prev_alert_fingerprints: HashSet<String>,
    last_good_grafana_alerts: Vec<Alert>,
    prev_tray_state: Option<(MetricLevel, bool)>,
    latest_metrics: Arc<Mutex<Option<MetricsUpdate>>>,
    latest_alerts: Arc<Mutex<Option<AlertsUpdate>>>,
}

impl Poller {
    pub(crate) fn new(handles: PollerHandles) -> Self {
        Self {
            app_handle: handles.app_handle,
            config_state: handles.config_state,
            is_visible: handles.is_visible,
            wake: handles.wake,
            ssh_backends: HashMap::new(),
            k8s_backends: HashMap::new(),
            failures: HashMap::new(),
            prev_levels: HashMap::new(),
            grafana_backend: None,
            prev_alert_fingerprints: HashSet::new(),
            last_good_grafana_alerts: Vec::new(),
            prev_tray_state: None,
            latest_metrics: handles.latest_metrics,
            latest_alerts: handles.latest_alerts,
        }
    }

    pub(crate) async fn run(&mut self) {
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
            let grafana_cfg = config.grafana.clone();

            self.cleanup_removed_backends(&servers);

            if let Err(e) = self.app_handle.emit("poll-start", ()) {
                tracing::warn!("failed to emit poll-start: {e}");
            }

            let mut grafana_backend = self.grafana_backend.take();
            let (all_metrics, grafana_result) = tokio::join!(
                self.poll_all_servers(&servers),
                Self::poll_grafana(&mut grafana_backend, grafana_cfg.as_ref()),
            );
            self.grafana_backend = grafana_backend;

            let update = MetricsUpdate {
                servers: all_metrics,
            };
            if let Ok(mut guard) = self.latest_metrics.lock() {
                *guard = Some(update.clone());
            }
            if let Err(e) = self.app_handle.emit("metrics-update", &update) {
                tracing::warn!("failed to emit metrics-update: {e}");
            }

            self.check_and_notify(notifications_enabled, &update.servers);

            let alerts_for_tray =
                tray_alerts_after_grafana(grafana_result.as_ref(), &self.last_good_grafana_alerts);

            if let Some(alerts_update) = grafana_result {
                if let Ok(mut guard) = self.latest_alerts.lock() {
                    *guard = Some(alerts_update.clone());
                }
                if let Err(e) = self.app_handle.emit("alerts-update", &alerts_update) {
                    tracing::warn!("failed to emit alerts-update: {e}");
                }
                // On a transient fetch error the alert list is empty but
                // *unknown*, not "all clear" — skip the notify/dedup update so
                // recovery does not replay every still-firing alert as new.
                if alerts_update.source_error.is_none() {
                    self.last_good_grafana_alerts
                        .clone_from(&alerts_update.alerts);
                    self.notify_new_alerts(notifications_enabled, &alerts_update.alerts);
                }
            } else {
                if let Ok(mut guard) = self.latest_alerts.lock() {
                    *guard = None;
                }
                self.prev_alert_fingerprints.clear();
                self.last_good_grafana_alerts.clear();
            }

            self.update_tray_icon(&update.servers, &alerts_for_tray);

            let interval = if self.is_visible.load(Ordering::Acquire) {
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
                let last_error = self.failures.get(&name).map(|f| f.last_error.clone());
                skipped.push(offline_metrics(&name, &stype, last_error));
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
                    all_metrics.push(offline_metrics(&name, &stype, Some(e.clone())));
                    self.record_failure(&name, e);
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
                    Some(_) | None => {
                        SshBackend::new(host.clone(), *port, user.clone(), key_path.clone())
                    }
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
                    Some(_) | None => K8sBackend::new(kubeconfig.clone(), context.clone()),
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
        match backoff_decision(state.count, state.last_attempt.elapsed()) {
            BackoffDecision::Ready => false,
            BackoffDecision::Hold => true,
            BackoffDecision::Expired => {
                state.count = 0;
                false
            }
        }
    }

    fn record_failure(&mut self, name: &str, error: String) {
        self.failures
            .entry(name.to_string())
            .or_insert_with(FailureState::new)
            .record(error);
    }

    fn update_tray_icon(&mut self, metrics: &[ServerMetrics], alerts: &[Alert]) {
        let Some(state) = self.app_handle.try_state::<TrayState>() else {
            return;
        };
        if state.take_icon_reset() {
            self.prev_tray_state = None;
        }

        let level = worst_level(metrics).max(worst_alert_level(alerts));
        let new_state = (level, has_restarts(metrics));
        if self.prev_tray_state == Some(new_state) {
            return;
        }
        self.prev_tray_state = Some(new_state);

        state.show_kind(tray_icon_kind(new_state.0, new_state.1));
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

    /// Poll Grafana for active alerts. Returns `None` when no Grafana
    /// connection is configured or it is disabled (in which case any
    /// cached backend and notification state are cleared). Network and
    /// auth failures are returned as an `AlertsUpdate` carrying a
    /// `source_error`, never as a panic.
    async fn poll_grafana(
        grafana_backend: &mut Option<GrafanaBackend>,
        grafana: Option<&crate::config::GrafanaConfig>,
    ) -> Option<AlertsUpdate> {
        let Some(cfg) = grafana.filter(|c| c.enabled) else {
            *grafana_backend = None;
            return None;
        };

        let token = match read_token(&cfg.name) {
            Ok(token) => token,
            Err(e) => {
                *grafana_backend = None;
                return Some(AlertsUpdate {
                    alerts: Vec::new(),
                    source_error: Some(crate::error::error_chain(&e)),
                });
            }
        };
        let needs_rebuild = grafana_backend
            .as_ref()
            .is_none_or(|b| !b.matches_config(cfg) || !b.uses_token(&token));
        if needs_rebuild {
            match GrafanaBackend::new(cfg, token) {
                Ok(backend) => *grafana_backend = Some(backend),
                Err(e) => {
                    *grafana_backend = None;
                    return Some(AlertsUpdate {
                        alerts: Vec::new(),
                        source_error: Some(crate::error::error_chain(&e)),
                    });
                }
            }
        }

        let backend = grafana_backend.as_ref()?;
        match tokio::time::timeout(COLLECT_TIMEOUT, backend.fetch_alerts()).await {
            Ok(Ok(alerts)) => Some(AlertsUpdate {
                alerts,
                source_error: None,
            }),
            Ok(Err(e)) => Some(AlertsUpdate {
                alerts: Vec::new(),
                source_error: Some(crate::error::error_chain(&e)),
            }),
            Err(_) => Some(AlertsUpdate {
                alerts: Vec::new(),
                source_error: Some("timed out fetching Grafana alerts".to_string()),
            }),
        }
    }

    fn notify_new_alerts(&mut self, enabled: bool, alerts: &[Alert]) {
        if enabled {
            for alert in newly_firing(&self.prev_alert_fingerprints, alerts) {
                self.send_alert_notification(alert);
            }
        }
        // Track the current active set even when notifications are
        // disabled, so re-enabling them does not replay the whole backlog
        // as "new". (Deliberately stronger than check_and_notify, which
        // resets on recovery.)
        self.prev_alert_fingerprints = alerts
            .iter()
            .filter(|a| a.state == AlertState::Active)
            .map(|a| a.fingerprint.clone())
            .collect();
    }

    fn send_alert_notification(&self, alert: &Alert) {
        use tauri_plugin_notification::NotificationExt;

        let severity = match alert.severity {
            AlertSeverity::Critical => "CRITICAL",
            AlertSeverity::Warning => "WARNING",
            AlertSeverity::Info => "INFO",
            AlertSeverity::Unknown => "ALERT",
        };
        let title = format!("Grafana {severity}: {}", alert.name);
        let body = if alert.summary.is_empty() {
            alert.name.clone()
        } else {
            alert.summary.clone()
        };

        if let Err(e) = self
            .app_handle
            .notification()
            .builder()
            .title(&title)
            .body(&body)
            .show()
        {
            tracing::warn!("failed to send alert notification: {e}");
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
    // The two backends return distinct typed errors; this internal
    // boundary flattens each to a chain-rendered String (the only thing
    // the poll loop does with it is log it / mark the server offline).
    let result: Result<Vec<ServerMetrics>, String> = match (&mut entry, server) {
        (BackendEntry::Ssh(backend), ServerConfig::Ssh { name, .. }) => {
            if !backend.is_connected()
                && let Err(e) = backend.connect().await
            {
                backend.disconnect().await;
                return (entry, Err(crate::error::error_chain(&e)));
            }
            let result = backend.collect_metrics(name).await;
            if result.is_err() {
                backend.disconnect().await;
            }
            result
                .map(|m| vec![m])
                .map_err(|e| crate::error::error_chain(&e))
        }
        (
            BackendEntry::K8s(backend),
            ServerConfig::K8s {
                name, namespace, ..
            },
        ) => backend
            .collect_all(name, namespace)
            .await
            .map_err(|e| crate::error::error_chain(&e)),
        (BackendEntry::Ssh(_), ServerConfig::K8s { .. })
        | (BackendEntry::K8s(_), ServerConfig::Ssh { .. }) => {
            Err("backend type mismatch".to_string())
        }
    };
    (entry, result)
}

fn offline_metrics(name: &str, server_type: &str, error: Option<String>) -> ServerMetrics {
    ServerMetrics {
        server_name: name.to_string(),
        server_type: server_type.to_string(),
        status: ServerStatus::Offline,
        error,
        ..ServerMetrics::default()
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::{
        BACKOFF_DURATION, BACKOFF_THRESHOLD, BackoffDecision, FailureState, TrayIconKind,
        backoff_decision, offline_metrics, tray_alerts_after_grafana, tray_icon_kind,
    };
    use crate::metrics::{Alert, AlertSeverity, AlertState, AlertsUpdate, MetricLevel};
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn sample_alert(name: &str) -> Alert {
        Alert {
            fingerprint: format!("fp-{name}"),
            name: name.to_string(),
            severity: AlertSeverity::Critical,
            state: AlertState::Active,
            summary: String::new(),
            description: String::new(),
            starts_at: String::new(),
            labels: BTreeMap::new(),
            generator_url: None,
        }
    }

    #[test]
    fn backoff_ready_below_threshold() {
        assert_eq!(
            backoff_decision(BACKOFF_THRESHOLD - 1, Duration::from_secs(0)),
            BackoffDecision::Ready
        );
    }

    #[test]
    fn backoff_holds_inside_window() {
        assert_eq!(
            backoff_decision(BACKOFF_THRESHOLD, Duration::from_secs(1)),
            BackoffDecision::Hold
        );
    }

    #[test]
    fn backoff_expires_at_window() {
        assert_eq!(
            backoff_decision(BACKOFF_THRESHOLD, BACKOFF_DURATION),
            BackoffDecision::Expired
        );
    }

    #[test]
    fn tray_icon_crit_beats_restart() {
        assert_eq!(tray_icon_kind(MetricLevel::Crit, true), TrayIconKind::Crit);
    }

    #[test]
    fn tray_icon_restart_when_not_crit() {
        assert_eq!(tray_icon_kind(MetricLevel::Ok, true), TrayIconKind::Restart);
        assert_eq!(
            tray_icon_kind(MetricLevel::Warn, true),
            TrayIconKind::Restart
        );
        assert_eq!(tray_icon_kind(MetricLevel::Warn, false), TrayIconKind::Warn);
        assert_eq!(
            tray_icon_kind(MetricLevel::Ok, false),
            TrayIconKind::Default
        );
    }

    #[test]
    fn tray_alerts_success_replaces_last_good() {
        let fresh = vec![sample_alert("new")];
        let update = AlertsUpdate {
            alerts: fresh.clone(),
            source_error: None,
        };
        let last_good = vec![sample_alert("old")];
        assert_eq!(tray_alerts_after_grafana(Some(&update), &last_good), fresh);
    }

    #[test]
    fn tray_alerts_error_keeps_last_good_across_failures() {
        let last_good = vec![sample_alert("firing")];
        let failed = AlertsUpdate {
            alerts: Vec::new(),
            source_error: Some("timeout".to_string()),
        };
        assert_eq!(
            tray_alerts_after_grafana(Some(&failed), &last_good),
            last_good
        );
        assert_eq!(
            tray_alerts_after_grafana(Some(&failed), &last_good),
            last_good
        );
    }

    #[test]
    fn tray_alerts_error_with_no_history_is_empty() {
        let failed = AlertsUpdate {
            alerts: Vec::new(),
            source_error: Some("timeout".to_string()),
        };
        assert!(tray_alerts_after_grafana(Some(&failed), &[]).is_empty());
    }

    #[test]
    fn tray_alerts_disabled_grafana_is_empty() {
        let last_good = vec![sample_alert("stale")];
        assert!(tray_alerts_after_grafana(None, &last_good).is_empty());
    }

    #[test]
    fn offline_metrics_carries_the_failure_reason_to_the_ui() {
        let m = offline_metrics(
            "hippius",
            "k8s",
            Some("failed to read kubeconfig /x.yaml: No such file".to_string()),
        );

        let json = serde_json::to_value(&m).expect("serialize offline metrics");

        assert_eq!(json["status"], "offline");
        assert_eq!(
            json["error"],
            "failed to read kubeconfig /x.yaml: No such file"
        );
    }

    #[test]
    fn online_metrics_omit_the_error_field() {
        let m = crate::metrics::ServerMetrics::default();

        let json = serde_json::to_value(&m).expect("serialize metrics");

        assert!(json.get("error").is_none(), "{json}");
    }

    #[test]
    fn failure_state_keeps_the_latest_error_for_backoff_rows() {
        let mut state = FailureState::new();

        state.record("connection refused".to_string());
        state.record("timed out collecting metrics for bastion".to_string());

        assert_eq!(state.count, 2);
        assert_eq!(state.last_error, "timed out collecting metrics for bastion");
    }
}

//! The poll loop: collect every server and Grafana concurrently, publish
//! the results to the frontend, and update notifications and the tray.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Notify;
use tokio::task::{Id, JoinSet};

use crate::config::{ConfigStore, ServerConfig, ServerName};
use crate::error::error_chain;
use crate::grafana::{AlertSource, TokenEpoch};
use crate::k8s::K8sBackend;
use crate::metrics::{
    Alert, AlertsUpdate, Health, MetricLevel, MetricsUpdate, ServerReport, USAGE_METRICS,
    has_restarts, newly_firing, online_usage, worst_alert_level, worst_level,
};
use crate::ssh::SshBackend;
use crate::tray::{TrayIconKind, TrayState};

/// Upper bound on one server's collection, connect included.
const COLLECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Consecutive failures after which a server is skipped for a while.
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

/// A server's live connection, holding the config it was built from.
enum Backend {
    Ssh(SshBackend),
    K8s(K8sBackend),
}

impl Backend {
    fn new(server: &ServerConfig) -> Self {
        match server {
            ServerConfig::Ssh(target) => Backend::Ssh(SshBackend::new(target.clone())),
            ServerConfig::K8s(target) => Backend::K8s(K8sBackend::new(target.clone())),
        }
    }

    /// Whether this backend was built from exactly `server`. Any edit
    /// means a fresh connection.
    fn serves(&self, server: &ServerConfig) -> bool {
        match (self, server) {
            (Backend::Ssh(backend), ServerConfig::Ssh(target)) => backend.target() == target,
            (Backend::K8s(backend), ServerConfig::K8s(target)) => backend.target() == target,
            (Backend::Ssh(_), ServerConfig::K8s(_)) | (Backend::K8s(_), ServerConfig::Ssh(_)) => {
                false
            }
        }
    }

    /// Collect one report. Errors are rendered with their cause chain here,
    /// the last point where they are typed.
    async fn collect(&mut self, name: ServerName) -> ServerReport {
        match self {
            Backend::Ssh(backend) => ServerReport::Ssh {
                name,
                health: health(backend.collect().await),
            },
            Backend::K8s(backend) => ServerReport::K8s {
                name,
                health: health(backend.collect().await),
            },
        }
    }
}

fn health<T, E: std::error::Error>(result: Result<T, E>) -> Health<T> {
    match result {
        Ok(metrics) => Health::Online { metrics },
        Err(e) => Health::Offline {
            error: error_chain(&e),
        },
    }
}

/// An offline report of the right kind for `server`.
fn offline_report(server: &ServerConfig, error: String) -> ServerReport {
    let name = server.name().clone();
    match server {
        ServerConfig::Ssh(_) => ServerReport::Ssh {
            name,
            health: Health::Offline { error },
        },
        ServerConfig::K8s(_) => ServerReport::K8s {
            name,
            health: Health::Offline { error },
        },
    }
}

fn offline_error(report: &ServerReport) -> Option<&str> {
    match report {
        ServerReport::Ssh {
            health: Health::Offline { error },
            ..
        }
        | ServerReport::K8s {
            health: Health::Offline { error },
            ..
        } => Some(error),
        ServerReport::Ssh {
            health: Health::Online { .. },
            ..
        }
        | ServerReport::K8s {
            health: Health::Online { .. },
            ..
        } => None,
    }
}

/// Backends and failure tracking for the configured servers.
#[derive(Default)]
struct ServerPool {
    backends: HashMap<ServerName, Backend>,
    failures: HashMap<ServerName, FailureState>,
}

/// What a collection task hands back: the backend to reuse (dropped on
/// timeout, as its connection state is unknown) and the report.
type TaskOutput = (ServerName, Option<Backend>, ServerReport);

impl ServerPool {
    /// Forget servers that are no longer configured.
    fn retain(&mut self, servers: &[ServerConfig]) {
        let active: HashSet<&str> = servers.iter().map(|s| s.name().as_str()).collect();
        self.backends
            .retain(|name, _| active.contains(name.as_str()));
        self.failures
            .retain(|name, _| active.contains(name.as_str()));
    }

    /// Poll all servers concurrently. Servers in backoff are reported
    /// offline with their last error, without being contacted.
    async fn poll(&mut self, servers: &[ServerConfig]) -> Vec<ServerReport> {
        let mut reports = Vec::with_capacity(servers.len());
        let mut tasks: JoinSet<TaskOutput> = JoinSet::new();
        let mut task_servers: HashMap<Id, &ServerConfig> = HashMap::new();

        for server in servers {
            if let Some(last_error) = self.backoff_error(server.name()) {
                reports.push(offline_report(server, last_error));
                continue;
            }
            let backend = self.take_backend(server);
            let id = tasks
                .spawn(collect_task(server.name().clone(), backend))
                .id();
            task_servers.insert(id, server);
        }

        while let Some(joined) = tasks.join_next_with_id().await {
            let report = match joined {
                Ok((_, (name, backend, report))) => {
                    if let Some(backend) = backend {
                        self.backends.insert(name, backend);
                    }
                    report
                }
                Err(e) => {
                    let Some(server) = task_servers.get(&e.id()) else {
                        tracing::error!("poll task {} failed: {e}", e.id());
                        continue;
                    };
                    tracing::error!("poll task for {} failed: {e}", server.name());
                    offline_report(server, format!("metrics collection failed: {e}"))
                }
            };
            self.record(&report);
            reports.push(report);
        }

        // Tasks finish in any order; keep the payload and the notification
        // order stable by reporting in config order.
        let position: HashMap<&str, usize> = servers
            .iter()
            .enumerate()
            .map(|(i, s)| (s.name().as_str(), i))
            .collect();
        reports.sort_by_key(|r| position.get(r.name().as_str()).copied());
        reports
    }

    /// The last error if `name` is in backoff. An expired backoff resets
    /// the counter for a fresh set of attempts.
    fn backoff_error(&mut self, name: &ServerName) -> Option<String> {
        let state = self.failures.get_mut(name)?;
        match backoff_decision(state.count, state.last_attempt.elapsed()) {
            BackoffDecision::Ready => None,
            BackoffDecision::Hold => Some(state.last_error.clone()),
            BackoffDecision::Expired => {
                state.count = 0;
                None
            }
        }
    }

    fn take_backend(&mut self, server: &ServerConfig) -> Backend {
        match self.backends.remove(server.name()) {
            Some(backend) if backend.serves(server) => backend,
            Some(_) | None => Backend::new(server),
        }
    }

    fn record(&mut self, report: &ServerReport) {
        match offline_error(report) {
            None => {
                self.failures.remove(report.name());
            }
            Some(error) => {
                tracing::warn!("failed to collect metrics for {}: {error}", report.name());
                self.failures
                    .entry(report.name().clone())
                    .or_insert_with(FailureState::new)
                    .record(error.to_string());
            }
        }
    }
}

async fn collect_task(name: ServerName, mut backend: Backend) -> TaskOutput {
    let collected = tokio::time::timeout(COLLECT_TIMEOUT, backend.collect(name.clone())).await;
    let Ok(report) = collected else {
        let error = format!("timed out collecting metrics for {name}");
        let report = match backend {
            Backend::Ssh(_) => ServerReport::Ssh {
                name: name.clone(),
                health: Health::Offline { error },
            },
            Backend::K8s(_) => ServerReport::K8s {
                name: name.clone(),
                health: Health::Offline { error },
            },
        };
        return (name, None, report);
    };
    (name, Some(backend), report)
}

/// Replace the cached value. The slot holds a value replaced wholesale, so
/// a poisoned lock cannot hide a half-written state.
fn store<T>(slot: &Mutex<Option<T>>, value: Option<T>) {
    *slot.lock().unwrap_or_else(PoisonError::into_inner) = value;
}

/// A desktop notification to show.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Notice {
    title: String,
    body: String,
}

/// Decides which notifications a cycle raises. State is tracked even
/// while notifications are off, so enabling them does not replay every
/// warning and alert already showing.
#[derive(Default)]
struct Notifier {
    /// Last levels of each online card. A card that goes offline or
    /// disappears is dropped, so it notifies again when it returns.
    levels: HashMap<String, [MetricLevel; 3]>,
    /// Active alert fingerprints from the last successful Grafana fetch.
    alert_fingerprints: HashSet<String>,
}

impl Notifier {
    /// A notice for every metric whose level rose since the last cycle.
    fn metric_notices(&mut self, reports: &[ServerReport]) -> Vec<Notice> {
        let mut notices = Vec::new();
        let mut levels = HashMap::new();

        for (label, usage) in online_usage(reports) {
            let now = usage.levels();
            let prev = self
                .levels
                .get(&label)
                .copied()
                .unwrap_or([MetricLevel::Ok; 3]);
            for (((metric, level), prev), value) in USAGE_METRICS
                .iter()
                .zip(now)
                .zip(prev)
                .zip(usage.percents())
            {
                if level > prev {
                    notices.push(Notice {
                        title: format!("{label}: {metric} {}", level.label()),
                        body: format!("{metric} at {value:.0}%"),
                    });
                }
            }
            levels.insert(label, now);
        }

        self.levels = levels;
        notices
    }

    /// A notice for every active alert not seen in the last fetch.
    fn alert_notices(&mut self, alerts: &[Alert]) -> Vec<Notice> {
        let notices = newly_firing(&self.alert_fingerprints, alerts)
            .into_iter()
            .map(|alert| Notice {
                title: format!("Grafana {}: {}", alert.severity.label(), alert.name),
                body: if alert.summary.is_empty() {
                    alert.name.clone()
                } else {
                    alert.summary.clone()
                },
            })
            .collect();
        self.alert_fingerprints = alerts
            .iter()
            .filter(|a| a.state == crate::metrics::AlertState::Active)
            .map(|a| a.fingerprint.clone())
            .collect();
        notices
    }

    fn forget_alerts(&mut self) {
        self.alert_fingerprints.clear();
    }
}

/// Inputs the poll loop needs from Tauri setup. Bundled so `Poller::new`
/// stays within the positional-argument limit.
pub(crate) struct PollerHandles {
    pub(crate) app_handle: AppHandle,
    pub(crate) config: Arc<ConfigStore>,
    pub(crate) is_visible: Arc<AtomicBool>,
    pub(crate) wake: Arc<Notify>,
    pub(crate) token_epoch: Arc<TokenEpoch>,
    pub(crate) latest_metrics: Arc<Mutex<Option<MetricsUpdate>>>,
    pub(crate) latest_alerts: Arc<Mutex<Option<AlertsUpdate>>>,
}

pub(crate) struct Poller {
    app: AppHandle,
    config: Arc<ConfigStore>,
    is_visible: Arc<AtomicBool>,
    wake: Arc<Notify>,
    servers: ServerPool,
    alerts: AlertSource,
    notifier: Notifier,
    last_good_alerts: Vec<Alert>,
    prev_tray: Option<TrayIconKind>,
    latest_metrics: Arc<Mutex<Option<MetricsUpdate>>>,
    latest_alerts: Arc<Mutex<Option<AlertsUpdate>>>,
}

impl Poller {
    pub(crate) fn new(handles: PollerHandles) -> Self {
        Self {
            app: handles.app_handle,
            config: handles.config,
            is_visible: handles.is_visible,
            wake: handles.wake,
            servers: ServerPool::default(),
            alerts: AlertSource::new(handles.token_epoch),
            notifier: Notifier::default(),
            last_good_alerts: Vec::new(),
            prev_tray: None,
            latest_metrics: handles.latest_metrics,
            latest_alerts: handles.latest_alerts,
        }
    }

    pub(crate) async fn run(&mut self) {
        loop {
            let interval = self.cycle().await;
            tokio::select! {
                () = tokio::time::sleep(interval) => {}
                () = self.wake.notified() => {}
            }
        }
    }

    /// One poll cycle. Returns how long to wait before the next.
    async fn cycle(&mut self) -> Duration {
        let config = self.config.snapshot().await;
        self.servers.retain(&config.servers);
        self.emit("poll-start", ());

        let (reports, alerts) = tokio::join!(
            self.servers.poll(&config.servers),
            self.alerts.poll(config.grafana.as_ref()),
        );

        let metric_notices = self.notifier.metric_notices(&reports);
        let update = MetricsUpdate { servers: reports };
        self.publish(&self.latest_metrics, "metrics-update", &update);

        let alert_notices = self.handle_alerts(alerts.as_ref());
        if config.notifications_enabled {
            for notice in metric_notices.iter().chain(&alert_notices) {
                self.notify(notice);
            }
        }

        let tray_alerts = tray_alerts_after_grafana(alerts.as_ref(), &self.last_good_alerts);
        self.update_tray(&update.servers, &tray_alerts);

        if self.is_visible.load(Ordering::Acquire) {
            config.foreground_interval()
        } else {
            config.background_interval()
        }
    }

    /// Publish the Grafana result and return the notices it raises. A
    /// failed fetch is unknown, not "all clear", so it leaves the dedup
    /// state alone: recovery must not replay still-firing alerts as new.
    fn handle_alerts(&mut self, alerts: Option<&AlertsUpdate>) -> Vec<Notice> {
        match alerts {
            None => {
                store(&self.latest_alerts, None);
                self.notifier.forget_alerts();
                self.last_good_alerts.clear();
                Vec::new()
            }
            Some(update) => {
                self.publish(&self.latest_alerts, "alerts-update", update);
                if update.source_error.is_some() {
                    return Vec::new();
                }
                self.last_good_alerts.clone_from(&update.alerts);
                self.notifier.alert_notices(&update.alerts)
            }
        }
    }

    fn update_tray(&mut self, reports: &[ServerReport], alerts: &[Alert]) {
        let Some(tray) = self.app.try_state::<TrayState>() else {
            return;
        };
        if tray.take_icon_reset() {
            self.prev_tray = None;
        }

        let level = worst_level(reports).max(worst_alert_level(alerts));
        let kind = tray_icon_kind(level, has_restarts(reports));
        if self.prev_tray != Some(kind) {
            tray.show_kind(kind);
            self.prev_tray = Some(kind);
        }
    }

    /// Cache `value` for the frontend's initial fetch, then emit it.
    fn publish<T: Serialize + Clone>(&self, slot: &Mutex<Option<T>>, event: &str, value: &T) {
        store(slot, Some(value.clone()));
        self.emit(event, value);
    }

    fn emit<T: Serialize + Clone>(&self, event: &str, payload: T) {
        if let Err(e) = self.app.emit(event, payload) {
            tracing::warn!("failed to emit {event}: {e}");
        }
    }

    fn notify(&self, notice: &Notice) {
        use tauri_plugin_notification::NotificationExt;

        if let Err(e) = self
            .app
            .notification()
            .builder()
            .title(&notice.title)
            .body(&notice.body)
            .show()
        {
            tracing::warn!("failed to show notification '{}': {e}", notice.title);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::tests::{cluster, host, make_alert, offline, pod, usage};
    use crate::metrics::{AlertSeverity, AlertState};

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

    fn sample_alert(name: &str) -> Alert {
        make_alert(
            &format!("fp-{name}"),
            AlertSeverity::Critical,
            AlertState::Active,
        )
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
    fn tray_alerts_error_keeps_last_good() {
        let last_good = vec![sample_alert("firing")];
        let failed = AlertsUpdate {
            alerts: Vec::new(),
            source_error: Some("timeout".to_string()),
        };
        assert_eq!(
            tray_alerts_after_grafana(Some(&failed), &last_good),
            last_good
        );
        assert!(tray_alerts_after_grafana(Some(&failed), &[]).is_empty());
    }

    #[test]
    fn tray_alerts_disabled_grafana_is_empty() {
        let last_good = vec![sample_alert("stale")];
        assert!(tray_alerts_after_grafana(None, &last_good).is_empty());
    }

    #[test]
    fn failure_state_keeps_the_latest_error_for_backoff_rows() {
        let mut state = FailureState::new();

        state.record("connection refused".to_string());
        state.record("timed out collecting metrics for bastion".to_string());

        assert_eq!(state.count, 2);
        assert_eq!(state.last_error, "timed out collecting metrics for bastion");
    }

    #[test]
    fn pool_records_failures_and_clears_on_success() {
        let mut pool = ServerPool::default();
        let name = ServerName::from("bastion");

        for _ in 0..BACKOFF_THRESHOLD {
            pool.record(&offline("bastion"));
        }
        assert_eq!(
            pool.backoff_error(&name).as_deref(),
            Some("connection refused")
        );

        pool.record(&host("bastion", usage(1.0, 1.0, 1.0)));
        assert!(pool.backoff_error(&name).is_none());
    }

    #[test]
    fn metric_notices_fire_once_per_rise() {
        let mut notifier = Notifier::default();

        let hot = [host("web", usage(90.0, 10.0, 10.0))];
        let first = notifier.metric_notices(&hot);
        assert_eq!(
            first,
            [Notice {
                title: "web: CPU CRITICAL".to_string(),
                body: "CPU at 90%".to_string(),
            }]
        );
        assert!(notifier.metric_notices(&hot).is_empty(), "no repeat");

        notifier.metric_notices(&[offline("web")]);
        assert_eq!(
            notifier.metric_notices(&hot).len(),
            1,
            "re-fires after recovery"
        );
    }

    #[test]
    fn pod_levels_persist_between_cycles() {
        // Pod entries used to be pruned every cycle by a cleanup that only
        // knew server names, so a hot pod notified on every poll.
        let mut notifier = Notifier::default();
        let reports = [cluster(
            "prod",
            usage(1.0, 1.0, 1.0),
            Some(vec![pod("web", 0, Some(usage(1.0, 95.0, 1.0)))]),
        )];

        let first = notifier.metric_notices(&reports);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].title, "prod/web: MEM CRITICAL");
        assert!(notifier.metric_notices(&reports).is_empty());
    }

    #[test]
    fn alert_notices_only_for_new_active_alerts() {
        let mut notifier = Notifier::default();
        let mut muted = sample_alert("muted");
        muted.state = AlertState::Suppressed;
        let alerts = [sample_alert("a"), muted];

        let first = notifier.alert_notices(&alerts);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].title, "Grafana CRITICAL: Test");
        assert!(notifier.alert_notices(&alerts).is_empty());

        notifier.forget_alerts();
        assert_eq!(notifier.alert_notices(&alerts).len(), 1);
    }
}

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

use crate::config::{AppConfig, ConfigStore, ServerConfig, ServerName};
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

/// How long a cluster that needs a login is left alone unless a login
/// lifts it. Arbitrary: long enough that an ignored expired session starts
/// a browser sign-in only every so often, short enough that a transient
/// failure (VPN down, a slow API server timing out a plugin cluster)
/// recovers by itself.
const LOGIN_HOLD: Duration = Duration::from_mins(10);

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
    /// The config the failures were seen with; an edit starts afresh.
    server: ServerConfig,
    count: u32,
    last_attempt: Instant,
    /// The last offline report, so servers held in backoff still report
    /// why they are offline and whether a login may help.
    last_report: ServerReport,
}

impl FailureState {
    fn new(server: ServerConfig, report: ServerReport) -> Self {
        Self {
            server,
            count: 1,
            last_attempt: Instant::now(),
            last_report: report,
        }
    }

    fn record(&mut self, report: ServerReport) {
        self.count = self.count.saturating_add(1);
        self.last_attempt = Instant::now();
        self.last_report = report;
    }
}

/// Servers to contact on the next poll even when held in backoff, because
/// a login just renewed their credentials. Shared by the login command,
/// which adds, and the poller, which drains.
#[derive(Debug, Default)]
pub(crate) struct RetryNow(Mutex<HashSet<ServerName>>);

impl RetryNow {
    pub(crate) fn request(&self, name: ServerName) {
        // The set stays valid whatever panicked while it was held.
        let mut names = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        names.insert(name);
    }

    fn take(&self) -> HashSet<ServerName> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
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

    /// Take `server` if this backend connects where it does, adopting its
    /// watched namespaces and hidden workloads. Any other edit means a
    /// fresh connection.
    fn adopt(&mut self, server: &ServerConfig) -> bool {
        match (self, server) {
            (Backend::Ssh(backend), ServerConfig::Ssh(target)) => backend.target() == target,
            (Backend::K8s(backend), ServerConfig::K8s(target)) => {
                let same = backend.target().same_connection(target);
                if same {
                    backend.set_target(target.clone());
                }
                same
            }
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
                health: match backend.collect().await {
                    Ok(metrics) => Health::Online { metrics },
                    Err(e) => Health::Offline {
                        error: error_chain(&e),
                        needs_login: false,
                    },
                },
            },
            Backend::K8s(backend) => ServerReport::K8s {
                name,
                health: match backend.collect().await {
                    Ok(metrics) => Health::Online { metrics },
                    Err(e) => Health::Offline {
                        error: error_chain(&e),
                        needs_login: e.needs_login(),
                    },
                },
            },
        }
    }
}

/// An offline report of the right kind for `server`, for a failure no
/// login can fix (a timeout, a panicked task).
fn offline_report(server: &ServerConfig, error: String) -> ServerReport {
    let name = server.name().clone();
    match server {
        ServerConfig::Ssh(_) => ServerReport::Ssh {
            name,
            health: Health::Offline {
                error,
                needs_login: false,
            },
        },
        ServerConfig::K8s(_) => ServerReport::K8s {
            name,
            health: Health::Offline {
                error,
                needs_login: false,
            },
        },
    }
}

fn offline_error(report: &ServerReport) -> Option<&str> {
    match report {
        ServerReport::Ssh {
            health: Health::Offline { error, .. },
            ..
        }
        | ServerReport::K8s {
            health: Health::Offline { error, .. },
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
type TaskOutput = (Option<Backend>, ServerReport);

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
            if let Some(last_report) = self.backoff_report(server) {
                reports.push(last_report);
                continue;
            }
            let backend = self.take_backend(server);
            let id = tasks.spawn(collect_task(server.clone(), backend)).id();
            task_servers.insert(id, server);
        }

        while let Some(joined) = tasks.join_next_with_id().await {
            let (id, outcome) = match joined {
                Ok((id, output)) => (id, Ok(output)),
                Err(e) => (e.id(), Err(e)),
            };
            let Some(&server) = task_servers.get(&id) else {
                tracing::error!("poll task {id} finished but was never spawned");
                continue;
            };
            let report = match outcome {
                Ok((backend, report)) => {
                    if let Some(backend) = backend {
                        self.backends.insert(server.name().clone(), backend);
                    }
                    report
                }
                Err(e) => {
                    tracing::error!("poll task for {} failed: {e}", server.name());
                    offline_report(server, format!("metrics collection failed: {e}"))
                }
            };
            self.record(server, &report);
            reports.push(report);
        }

        order_by_config(&mut reports, servers);
        reports
    }

    /// Contact every server `retry` names on this poll, past any backoff.
    fn retry_now(&mut self, retry: &RetryNow) {
        for name in retry.take() {
            self.failures.remove(&name);
        }
    }

    /// The last offline report if `server` is in backoff. An expired
    /// backoff resets the counter for a fresh set of attempts, and so does
    /// any change to the server's connection: a corrected entry (removed
    /// and re-added under the same name) must be contacted right away.
    fn backoff_report(&mut self, server: &ServerConfig) -> Option<ServerReport> {
        let state = self.failures.get_mut(server.name())?;
        if !state.server.same_connection(server) {
            self.failures.remove(server.name());
            return None;
        }
        // A login is the likely fix, and polling again would run the
        // credential plugin again, which may start another browser sign-in.
        // `k8s_login` lifts this early through `retry_now`.
        if needs_login(&state.last_report) && state.last_attempt.elapsed() < LOGIN_HOLD {
            return Some(state.last_report.clone());
        }
        match backoff_decision(state.count, state.last_attempt.elapsed()) {
            BackoffDecision::Ready => None,
            BackoffDecision::Hold => Some(state.last_report.clone()),
            BackoffDecision::Expired => {
                state.count = 0;
                None
            }
        }
    }

    fn take_backend(&mut self, server: &ServerConfig) -> Backend {
        match self.backends.remove(server.name()) {
            Some(mut backend) => {
                if backend.adopt(server) {
                    backend
                } else {
                    Backend::new(server)
                }
            }
            None => Backend::new(server),
        }
    }

    fn record(&mut self, server: &ServerConfig, report: &ServerReport) {
        match offline_error(report) {
            None => {
                self.failures.remove(server.name());
            }
            Some(error) => {
                tracing::warn!("failed to collect metrics for {}: {error}", server.name());
                if let Some(state) = self.failures.get_mut(server.name()) {
                    state.record(report.clone());
                } else {
                    let state = FailureState::new(server.clone(), report.clone());
                    self.failures.insert(server.name().clone(), state);
                }
            }
        }
    }
}

/// Sort `reports` into the order of `servers`. Tasks finish in any order;
/// this keeps the payload and the notification order stable.
fn order_by_config(reports: &mut [ServerReport], servers: &[ServerConfig]) {
    let position: HashMap<&str, usize> = servers
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name().as_str(), i))
        .collect();
    reports.sort_by_key(|r| position.get(r.name().as_str()).copied());
}

async fn collect_task(server: ServerConfig, mut backend: Backend) -> TaskOutput {
    let name = server.name().clone();
    let Ok(report) = tokio::time::timeout(COLLECT_TIMEOUT, backend.collect(name)).await else {
        return (None, timeout_report(&server, &backend));
    };
    (Some(backend), report)
}

/// The report for a poll that ran out of time. A credential plugin waiting
/// for a browser sign-in holds the poll this way: kubectl-bao (see
/// `k8s::exec_auth`) does not read `spec.interactive`, so it signs in even
/// when polled. Such a cluster offers a login, which also stops it being
/// polled into another sign-in; for an unreachable cluster the login is a
/// quick no-op followed by a retry.
fn timeout_report(server: &ServerConfig, backend: &Backend) -> ServerReport {
    let error = format!("timed out collecting metrics for {}", server.name());
    match backend {
        Backend::K8s(k8s) if k8s.uses_credential_plugin() => ServerReport::K8s {
            name: server.name().clone(),
            health: Health::Offline {
                error: format!("{error}; its credential plugin may be waiting for a sign-in"),
                needs_login: true,
            },
        },
        Backend::K8s(_) | Backend::Ssh(_) => offline_report(server, error),
    }
}

/// Whether `report` is offline until a login.
fn needs_login(report: &ServerReport) -> bool {
    matches!(
        report,
        ServerReport::K8s {
            health: Health::Offline {
                needs_login: true,
                ..
            },
            ..
        }
    )
}

/// Drop the pods each cluster in `config` hides, before the reports reach
/// the tray, notifier, or UI, so a hidden pod affects none of them.
fn hide_workloads(reports: &mut [ServerReport], config: &AppConfig) {
    for report in reports {
        let ServerReport::K8s {
            name,
            health: Health::Online { metrics },
        } = report
        else {
            continue;
        };
        if let Some(ServerConfig::K8s(target)) = config.server(name.as_str()) {
            metrics.hide(target);
        }
    }
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
    levels: HashMap<String, [MetricLevel; USAGE_METRICS.len()]>,
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
                .unwrap_or([MetricLevel::Ok; USAGE_METRICS.len()]);
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

        // A namespace whose pods could not be listed says nothing about
        // them: keep their levels so the next good listing does not
        // re-announce every pod that is still hot.
        for report in reports {
            let ServerReport::K8s {
                name,
                health: Health::Online { metrics },
            } = report
            else {
                continue;
            };
            for namespace in metrics.namespaces.iter().filter(|ns| ns.pods.is_none()) {
                let prefix = format!("{name}/{}/", namespace.name);
                for (label, level) in &self.levels {
                    if label.starts_with(&prefix) {
                        levels.insert(label.clone(), *level);
                    }
                }
            }
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
    pub(crate) retry: Arc<RetryNow>,
    pub(crate) latest_metrics: Arc<Mutex<Option<MetricsUpdate>>>,
    pub(crate) latest_alerts: Arc<Mutex<Option<AlertsUpdate>>>,
}

pub(crate) struct Poller {
    app: AppHandle,
    config: Arc<ConfigStore>,
    is_visible: Arc<AtomicBool>,
    wake: Arc<Notify>,
    retry: Arc<RetryNow>,
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
            retry: handles.retry,
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
        self.servers.retry_now(&self.retry);
        self.emit("poll-start", ());

        let (mut reports, alerts) = tokio::join!(
            self.servers.poll(&config.servers),
            self.alerts.poll(config.grafana.as_ref()),
        );
        // Against the config as it is now, not as the poll started: a
        // workload hidden mid-poll must not notify or reach the tray. This
        // only removes pods; one shown mid-poll returns with the next poll,
        // which `show_workload` wakes at once.
        hide_workloads(&mut reports, &self.config.snapshot().await);

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
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::*;
    use crate::config::{K8sTarget, SshTarget, WatchedNamespace, WorkloadRef};
    use crate::metrics::tests::{cluster, host, make_alert, metrics_mut, offline, pod, usage};
    use crate::metrics::{AlertSeverity, AlertState, NamespaceMetrics, PodMetrics};
    use std::collections::BTreeSet;
    use std::path::PathBuf;

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
        assert_eq!(tray_alerts_after_grafana(Some(&failed), &[]), []);
    }

    #[test]
    fn tray_alerts_disabled_grafana_is_empty() {
        let last_good = vec![sample_alert("stale")];
        assert_eq!(tray_alerts_after_grafana(None, &last_good), []);
    }

    #[test]
    fn failure_state_keeps_the_latest_report_for_backoff_rows() {
        let server = ssh_server("bastion", "h");
        let mut state = FailureState::new(server.clone(), offline("bastion"));

        let timeout = "timed out collecting metrics for bastion".to_string();
        state.record(offline_report(&server, timeout));

        assert_eq!(state.count, 2);
        assert_eq!(
            offline_error(&state.last_report),
            Some("timed out collecting metrics for bastion")
        );
    }

    #[test]
    fn a_cluster_needing_login_is_held_until_a_retry_or_the_hold_ends() {
        let mut pool = ServerPool::default();
        let server = k8s_server("prod", []);
        let expired = ServerReport::K8s {
            name: "prod".into(),
            health: Health::Offline {
                error: "credential plugin failed: session expired".to_string(),
                needs_login: true,
            },
        };
        pool.record(&server, &expired);
        // Long past any backoff: another poll would run the plugin again.
        let state = pool.failures.get_mut("prod").expect("recorded");
        state.last_attempt = Instant::now()
            .checked_sub(BACKOFF_DURATION * 2)
            .expect("the clock is past two backoffs");

        assert!(matches!(
            pool.backoff_report(&server),
            Some(ServerReport::K8s {
                health: Health::Offline {
                    needs_login: true,
                    ..
                },
                ..
            })
        ));

        let retry = RetryNow::default();
        retry.request("other".into());
        pool.retry_now(&retry);
        assert!(
            pool.backoff_report(&server).is_some(),
            "only the named server"
        );

        retry.request("prod".into());
        pool.retry_now(&retry);
        assert!(pool.backoff_report(&server).is_none());

        // Without a login, the hold ends by itself.
        pool.record(&server, &expired);
        let state = pool.failures.get_mut("prod").expect("recorded");
        state.last_attempt = Instant::now()
            .checked_sub(LOGIN_HOLD)
            .expect("the clock is past one hold");
        assert!(pool.backoff_report(&server).is_none(), "the hold expired");
    }

    fn ssh_server(name: &str, host: &str) -> ServerConfig {
        ServerConfig::Ssh(SshTarget {
            name: name.into(),
            host: host.to_string(),
            port: 22,
            user: "ops".to_string(),
            key_path: PathBuf::from("/k"),
        })
    }

    #[test]
    fn pool_records_failures_and_clears_on_success() {
        let mut pool = ServerPool::default();
        let server = ssh_server("bastion", "10.0.0.1");

        for _ in 0..BACKOFF_THRESHOLD {
            pool.record(&server, &offline("bastion"));
        }
        let held = pool.backoff_report(&server);
        assert_eq!(
            held.as_ref().and_then(offline_error),
            Some("connection refused")
        );

        pool.record(&server, &host("bastion", usage(1.0, 1.0, 1.0)));
        assert!(pool.backoff_report(&server).is_none());
    }

    #[test]
    fn edited_server_is_not_held_in_the_old_backoff() {
        let mut pool = ServerPool::default();
        let broken = ssh_server("bastion", "10.0.0.1");
        for _ in 0..BACKOFF_THRESHOLD {
            pool.record(&broken, &offline("bastion"));
        }

        let fixed = ssh_server("bastion", "10.0.0.2");

        assert!(pool.backoff_report(&fixed).is_none());
        assert!(pool.backoff_report(&broken).is_none(), "state was reset");
    }

    /// A cluster watching `default`, with `hidden` hidden there.
    fn k8s_server(name: &str, hidden: impl IntoIterator<Item = WorkloadRef>) -> ServerConfig {
        ServerConfig::K8s(K8sTarget {
            name: name.into(),
            kubeconfig: None,
            context: "ctx".to_string(),
            namespaces: vec![WatchedNamespace {
                name: "default".to_string(),
                hidden: hidden.into_iter().collect(),
            }],
        })
    }

    #[tokio::test]
    async fn a_timed_out_cluster_offers_login_only_when_a_plugin_signs_it_in() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let plugin = dir.path().join("plugin");
        std::fs::write(&plugin, "#!/bin/sh\nexit 1\n").expect("write plugin");
        std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755))
            .expect("make plugin executable");
        let kubeconfig = dir.path().join("kubeconfig.yaml");
        let yaml = format!(
            "clusters:\n- name: c\n  cluster: {{server: \"https://127.0.0.1:1\"}}\n\
             users:\n- name: u\n  user:\n    exec:\n      \
             apiVersion: client.authentication.k8s.io/v1\n      command: \"{}\"\n\
             contexts:\n- name: ctx\n  context: {{cluster: c, user: u}}\n",
            plugin.display()
        );
        std::fs::write(&kubeconfig, yaml).expect("write kubeconfig");
        let ServerConfig::K8s(mut target) = k8s_server("prod", []) else {
            unreachable!("k8s_server builds a cluster");
        };
        target.kubeconfig = Some(kubeconfig);
        let server = ServerConfig::K8s(target);

        let mut backend = Backend::new(&server);
        // Fails at once; what matters is that the backend saw the plugin.
        let _ = backend.collect("prod".into()).await;

        assert!(needs_login(&timeout_report(&server, &backend)));
        let fresh = Backend::new(&server);
        assert!(!needs_login(&timeout_report(&server, &fresh)));
        let host = ssh_server("bastion", "h");
        assert!(!needs_login(&timeout_report(&host, &Backend::new(&host))));
    }

    /// The workload of the `pod` fixture named `name`.
    fn pod_workload(name: &str) -> WorkloadRef {
        WorkloadRef {
            kind: "Pod".to_string(),
            name: name.to_string(),
        }
    }

    #[test]
    fn hiding_a_workload_keeps_the_backoff_and_the_backend() {
        let mut pool = ServerPool::default();
        let server = k8s_server("prod", []);
        for _ in 0..BACKOFF_THRESHOLD {
            pool.record(&server, &offline("prod"));
        }

        let hiding = k8s_server("prod", [pod_workload("api-1")]);

        assert!(pool.backoff_report(&hiding).is_some());

        let hidden_of = |backend: &Backend| match backend {
            Backend::K8s(k8s) => k8s.target().namespaces[0].hidden.clone(),
            Backend::Ssh(_) => BTreeSet::from([pod_workload("not a k8s backend")]),
        };
        let mut backend = Backend::new(&server);
        assert!(backend.adopt(&hiding));
        assert_eq!(hidden_of(&backend), BTreeSet::from([pod_workload("api-1")]));
        assert!(
            backend.adopt(&server),
            "showing it again keeps the backend too"
        );
        assert_eq!(hidden_of(&backend), BTreeSet::new());
    }

    #[test]
    fn hidden_pods_reach_neither_tray_nor_notifier() {
        let mut reports = [cluster(
            "prod",
            usage(1.0, 1.0, 1.0),
            Some(vec![
                pod("api-1", 7, Some(usage(99.0, 99.0, 0.0))),
                pod("db-0", 0, Some(usage(1.0, 1.0, 0.0))),
            ]),
        )];
        assert!(has_restarts(&reports));
        let config = AppConfig {
            servers: vec![k8s_server("prod", [pod_workload("api-1")])],
            ..AppConfig::default()
        };

        hide_workloads(&mut reports, &config);

        assert!(!has_restarts(&reports));
        assert_eq!(worst_level(&reports), MetricLevel::Ok);
        let labels: Vec<String> = online_usage(&reports).map(|(label, _)| label).collect();
        assert_eq!(labels, ["prod", "prod/default/db-0"]);
    }

    #[test]
    fn reports_follow_config_order() {
        let servers = [
            ssh_server("a", "h"),
            ssh_server("b", "h"),
            ssh_server("c", "h"),
        ];
        let mut reports = vec![offline("c"), offline("a"), offline("b")];

        order_by_config(&mut reports, &servers);

        let names: Vec<&str> = reports.iter().map(|r| r.name().as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
    }

    #[test]
    fn panicked_or_timed_out_tasks_report_the_right_kind() {
        let report = offline_report(&ssh_server("a", "h"), "boom".to_string());
        assert!(matches!(
            report,
            ServerReport::Ssh {
                health: Health::Offline { ref error, needs_login: false },
                ..
            } if error == "boom"
        ));
    }

    #[test]
    fn pod_levels_survive_a_failed_pod_listing() {
        let mut notifier = Notifier::default();
        let hot = [cluster(
            "prod",
            usage(1.0, 1.0, 1.0),
            Some(vec![pod("web", 0, Some(usage(95.0, 1.0, 1.0)))]),
        )];
        assert_eq!(notifier.metric_notices(&hot).len(), 1);

        notifier.metric_notices(&[cluster("prod", usage(1.0, 1.0, 1.0), None)]);

        assert!(
            notifier.metric_notices(&hot).is_empty(),
            "no repeat after a listing gap"
        );
    }

    #[test]
    fn only_the_namespace_that_failed_to_list_keeps_its_pod_levels() {
        let hot = || Some(usage(95.0, 1.0, 1.0));
        let two_namespaces = |default: Option<Vec<PodMetrics>>, s3: Option<Vec<PodMetrics>>| {
            let mut report = cluster("prod", usage(1.0, 1.0, 1.0), default);
            metrics_mut(&mut report).namespaces.push(NamespaceMetrics {
                name: "s3".to_string(),
                pods: s3,
            });
            [report]
        };
        let mut notifier = Notifier::default();
        let both_hot = two_namespaces(
            Some(vec![pod("web", 0, hot())]),
            Some(vec![pod("db", 0, hot())]),
        );
        assert_eq!(notifier.metric_notices(&both_hot).len(), 2);

        // `default` fails to list; `s3` lists with its pod cooled down.
        notifier.metric_notices(&two_namespaces(None, Some(vec![pod("db", 0, None)])));

        let notices = notifier.metric_notices(&both_hot);
        let titles: Vec<&str> = notices.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(
            titles,
            ["prod/s3/db: CPU CRITICAL"],
            "default's level was kept, s3's was forgotten"
        );
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
        assert_eq!(first[0].title, "prod/default/web: MEM CRITICAL");
        assert_eq!(notifier.metric_notices(&reports), []);
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
        assert_eq!(notifier.alert_notices(&alerts), []);

        notifier.forget_alerts();
        assert_eq!(notifier.alert_notices(&alerts).len(), 1);
    }
}

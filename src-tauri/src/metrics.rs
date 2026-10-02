//! Data the poller sends to the frontend, and the level logic that drives
//! the tray icon and notifications.
//!
//! Each server kind gets its own metrics type, so a field that only means
//! something for pods cannot appear, zeroed, on an SSH host; and an
//! offline server carries its error instead of a row of zeros.

use std::collections::{BTreeMap, HashSet};
use std::hash::BuildHasher;
use std::time::Instant;

use serde::Serialize;

use crate::config::ServerName;

/// Event payload sent to the frontend as `metrics-update`.
#[derive(Debug, Clone, Serialize)]
pub struct MetricsUpdate {
    pub servers: Vec<ServerReport>,
}

/// One configured server's result for a poll cycle.
///
/// JSON: `{"kind": "ssh"|"k8s", "name": ..., "status": "online",
/// "metrics": {...}}` or `{..., "status": "offline", "error": "..."}`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ServerReport {
    Ssh {
        name: ServerName,
        #[serde(flatten)]
        health: Health<HostMetrics>,
    },
    K8s {
        name: ServerName,
        #[serde(flatten)]
        health: Health<ClusterMetrics>,
    },
}

impl ServerReport {
    #[must_use]
    pub fn name(&self) -> &ServerName {
        match self {
            ServerReport::Ssh { name, .. } | ServerReport::K8s { name, .. } => name,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Health<T> {
    Online {
        metrics: T,
    },
    /// Why the last poll failed, rendered with its cause chain.
    Offline {
        error: String,
    },
}

/// The three percentages every card shows as bars.
#[expect(
    clippy::struct_field_names,
    reason = "the field names are the JSON the frontend reads"
)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Usage {
    pub cpu_percent: f64,
    pub memory_percent: f64,
    pub disk_percent: f64,
}

/// Network throughput between two polls. `None` in a metrics struct
/// until there have been two samples to compare.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct NetRate {
    pub rx_bytes_per_sec: u64,
    pub tx_bytes_per_sec: u64,
}

/// Cumulative byte counters at one instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetSample {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub at: Instant,
}

impl NetSample {
    /// Throughput from `prev` to `self`. A counter that went backwards
    /// (reboot, interface reset) reads as zero rather than wrapping.
    #[must_use]
    pub fn rate_since(&self, prev: &NetSample) -> Option<NetRate> {
        let elapsed = self.at.checked_duration_since(prev.at)?.as_secs_f64();
        if elapsed <= 0.0 {
            return None;
        }
        Some(NetRate {
            rx_bytes_per_sec: per_second(self.rx_bytes.saturating_sub(prev.rx_bytes), elapsed),
            tx_bytes_per_sec: per_second(self.tx_bytes.saturating_sub(prev.tx_bytes), elapsed),
        })
    }
}

#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a byte delta over positive seconds is finite and non-negative; \
              f64 -> u64 saturates and sub-byte precision is irrelevant"
)]
fn per_second(bytes: u64, seconds: f64) -> u64 {
    (bytes as f64 / seconds) as u64
}

/// `part / whole` as a percentage, 0 when `whole` is 0.
#[must_use]
pub fn percent(part: f64, whole: f64) -> f64 {
    if whole > 0.0 {
        part / whole * 100.0
    } else {
        0.0
    }
}

/// Used and total bytes of a filesystem or volume.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Capacity {
    pub used_bytes: u64,
    pub capacity_bytes: u64,
}

impl Capacity {
    #[expect(
        clippy::cast_precision_loss,
        reason = "byte counts fit comfortably in f64 for a percentage"
    )]
    #[must_use]
    pub fn percent(&self) -> f64 {
        percent(self.used_bytes as f64, self.capacity_bytes as f64)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HostMetrics {
    #[serde(flatten)]
    pub usage: Usage,
    pub net: Option<NetRate>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClusterMetrics {
    #[serde(flatten)]
    pub usage: Usage,
    pub net: Option<NetRate>,
    pub cpu_millicores: f64,
    pub memory_bytes: u64,
    pub disk: Capacity,
    pub node_count: usize,
    /// Pods in the configured namespace. `None` when listing them failed:
    /// the UI must then keep the previous cards, not treat the cluster as
    /// having no pods.
    pub pods: Option<Vec<PodMetrics>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PodMetrics {
    pub name: String,
    /// `None` when the pod spec was not in the listing (a pod created or
    /// deleted between the two list calls).
    pub status: Option<PodStatus>,
    pub restart_count: u32,
    /// RFC 3339, second precision.
    pub start_time: Option<String>,
    /// Newest event, as `"{type}: {reason}"`.
    pub last_event: Option<String>,
    /// `None` when metrics-server has no sample for the pod yet.
    pub usage: Option<PodUsage>,
    pub net: Option<NetRate>,
    /// Summed over the pod's PVC-backed volumes; `None` without any.
    pub pvc: Option<Capacity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PodUsage {
    #[serde(flatten)]
    pub usage: Usage,
    pub cpu_millicores: f64,
    pub memory_bytes: u64,
}

/// A pod's phase, or a container state specific enough to override it.
/// Serialized with Kubernetes' own spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PodStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Unknown,
    CrashLoopBackOff,
    ImagePullBackOff,
    ErrImagePull,
    #[serde(rename = "OOMKilled")]
    OomKilled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MetricLevel {
    Ok,
    Warn,
    Crit,
}

impl MetricLevel {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            MetricLevel::Ok => "OK",
            MetricLevel::Warn => "WARNING",
            MetricLevel::Crit => "CRITICAL",
        }
    }
}

/// Thresholds shared with the frontend bars: green < 60, amber < 85.
#[must_use]
pub fn classify_level(percent: f64) -> MetricLevel {
    if percent >= 85.0 {
        MetricLevel::Crit
    } else if percent >= 60.0 {
        MetricLevel::Warn
    } else {
        MetricLevel::Ok
    }
}

/// The metric names, in the order of [`Usage::levels`].
pub const USAGE_METRICS: [&str; 3] = ["CPU", "MEM", "DISK"];

impl Usage {
    #[must_use]
    pub fn percents(&self) -> [f64; USAGE_METRICS.len()] {
        [self.cpu_percent, self.memory_percent, self.disk_percent]
    }

    #[must_use]
    pub fn levels(&self) -> [MetricLevel; USAGE_METRICS.len()] {
        self.percents().map(classify_level)
    }
}

/// Every online card's usage with the label used in notifications:
/// server names for hosts and clusters, `cluster/pod` for pods.
pub fn online_usage(reports: &[ServerReport]) -> impl Iterator<Item = (String, Usage)> + '_ {
    reports.iter().flat_map(|report| {
        let mut entries = Vec::new();
        match report {
            ServerReport::Ssh {
                name,
                health: Health::Online { metrics },
            } => entries.push((name.to_string(), metrics.usage)),
            ServerReport::K8s {
                name,
                health: Health::Online { metrics },
            } => {
                entries.push((name.to_string(), metrics.usage));
                for pod in metrics.pods.iter().flatten() {
                    if let Some(usage) = pod.usage {
                        entries.push((format!("{name}/{}", pod.name), usage.usage));
                    }
                }
            }
            ServerReport::Ssh {
                health: Health::Offline { .. },
                ..
            }
            | ServerReport::K8s {
                health: Health::Offline { .. },
                ..
            } => {}
        }
        entries
    })
}

#[must_use]
pub fn worst_level(reports: &[ServerReport]) -> MetricLevel {
    online_usage(reports)
        .flat_map(|(_, usage)| usage.levels())
        .max()
        .unwrap_or(MetricLevel::Ok)
}

/// Whether any pod of an online cluster has restarted.
#[must_use]
pub fn has_restarts(reports: &[ServerReport]) -> bool {
    reports.iter().any(|report| match report {
        ServerReport::K8s {
            health: Health::Online { metrics },
            ..
        } => metrics
            .pods
            .iter()
            .flatten()
            .any(|pod| pod.restart_count > 0),
        ServerReport::K8s {
            health: Health::Offline { .. },
            ..
        }
        | ServerReport::Ssh { .. } => false,
    })
}

/// Severity of a Grafana alert, derived from the conventional
/// `severity` label. A missing or unrecognized label maps to
/// `Unknown` so an alert is never silently dropped for lacking the
/// label.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AlertSeverity {
    Critical,
    Warning,
    Info,
    Unknown,
}

impl AlertSeverity {
    /// Parse the `severity` label value, case-insensitively.
    #[must_use]
    pub fn from_label(value: Option<&str>) -> Self {
        match value.map(str::to_ascii_lowercase).as_deref() {
            Some("critical") => AlertSeverity::Critical,
            Some("warning") => AlertSeverity::Warning,
            Some("info") => AlertSeverity::Info,
            Some(_) | None => AlertSeverity::Unknown,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            AlertSeverity::Critical => "CRITICAL",
            AlertSeverity::Warning => "WARNING",
            AlertSeverity::Info => "INFO",
            AlertSeverity::Unknown => "ALERT",
        }
    }
}

/// Whether an alert is actively firing or suppressed (silenced or
/// inhibited in Grafana). Suppressed alerts are shown but do not raise
/// the tray level or fire notifications.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AlertState {
    Active,
    Suppressed,
}

/// A single Grafana alert as displayed by the app. `fingerprint` is
/// Grafana's stable per-alert identity, used as the notification dedup
/// key across poll cycles.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Alert {
    pub fingerprint: String,
    pub name: String,
    pub severity: AlertSeverity,
    pub state: AlertState,
    pub summary: String,
    pub description: String,
    pub starts_at: String,
    pub labels: BTreeMap<String, String>,
    pub generator_url: Option<String>,
}

/// Event payload sent to the frontend as `alerts-update`.
/// `source_error` is `Some` when the fetch failed, so the UI can show an
/// "unreachable" state instead of a stale list.
#[derive(Debug, Clone, Serialize)]
pub struct AlertsUpdate {
    pub alerts: Vec<Alert>,
    pub source_error: Option<String>,
}

/// Map an alert severity onto the tray-icon metric level so Grafana
/// alerts and self-collected metrics share one visual scale.
#[must_use]
pub fn severity_to_level(severity: AlertSeverity) -> MetricLevel {
    match severity {
        AlertSeverity::Critical => MetricLevel::Crit,
        AlertSeverity::Warning => MetricLevel::Warn,
        AlertSeverity::Info | AlertSeverity::Unknown => MetricLevel::Ok,
    }
}

/// Worst tray level across the active (non-suppressed) alerts.
#[must_use]
pub fn worst_alert_level(alerts: &[Alert]) -> MetricLevel {
    alerts
        .iter()
        .filter(|a| a.state == AlertState::Active)
        .map(|a| severity_to_level(a.severity))
        .max()
        .unwrap_or(MetricLevel::Ok)
}

/// Active alerts whose fingerprint is absent from `prev`: the set that
/// should raise a fresh notification this cycle.
#[must_use]
pub fn newly_firing<'a, S: BuildHasher>(
    prev: &HashSet<String, S>,
    alerts: &'a [Alert],
) -> Vec<&'a Alert> {
    alerts
        .iter()
        .filter(|a| a.state == AlertState::Active && !prev.contains(&a.fingerprint))
        .collect()
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
pub(crate) mod tests {
    use std::time::Duration;

    use super::*;

    pub(crate) fn usage(cpu: f64, memory: f64, disk: f64) -> Usage {
        Usage {
            cpu_percent: cpu,
            memory_percent: memory,
            disk_percent: disk,
        }
    }

    pub(crate) fn host(name: &str, usage: Usage) -> ServerReport {
        ServerReport::Ssh {
            name: name.into(),
            health: Health::Online {
                metrics: HostMetrics { usage, net: None },
            },
        }
    }

    pub(crate) fn offline(name: &str) -> ServerReport {
        ServerReport::Ssh {
            name: name.into(),
            health: Health::Offline {
                error: "connection refused".to_string(),
            },
        }
    }

    pub(crate) fn pod(name: &str, restarts: u32, usage: Option<Usage>) -> PodMetrics {
        PodMetrics {
            name: name.to_string(),
            status: Some(PodStatus::Running),
            restart_count: restarts,
            start_time: None,
            last_event: None,
            usage: usage.map(|usage| PodUsage {
                usage,
                cpu_millicores: 0.0,
                memory_bytes: 0,
            }),
            net: None,
            pvc: None,
        }
    }

    pub(crate) fn cluster(name: &str, usage: Usage, pods: Option<Vec<PodMetrics>>) -> ServerReport {
        ServerReport::K8s {
            name: name.into(),
            health: Health::Online {
                metrics: ClusterMetrics {
                    usage,
                    net: None,
                    cpu_millicores: 0.0,
                    memory_bytes: 0,
                    disk: Capacity::default(),
                    node_count: 1,
                    pods,
                },
            },
        }
    }

    #[test]
    fn online_host_serializes_flat_status_and_metrics() {
        let json = serde_json::to_value(host("web-1", usage(42.5, 61.3, 78.0))).expect("json");

        assert_eq!(json["kind"], "ssh");
        assert_eq!(json["name"], "web-1");
        assert_eq!(json["status"], "online");
        assert_eq!(json["metrics"]["cpu_percent"], 42.5);
        assert_eq!(json["metrics"]["memory_percent"], 61.3);
        assert_eq!(json["metrics"]["disk_percent"], 78.0);
        assert!(json["metrics"]["net"].is_null());
        assert!(json.get("error").is_none());
    }

    #[test]
    fn offline_server_carries_only_its_error() {
        let json = serde_json::to_value(offline("bastion")).expect("json");

        assert_eq!(json["kind"], "ssh");
        assert_eq!(json["status"], "offline");
        assert_eq!(json["error"], "connection refused");
        assert!(json.get("metrics").is_none());
    }

    #[test]
    fn cluster_serializes_pods_or_null() {
        let listed = cluster("prod", Usage::default(), Some(vec![pod("web", 2, None)]));
        let json = serde_json::to_value(listed).expect("json");
        assert_eq!(json["kind"], "k8s");
        assert_eq!(json["metrics"]["pods"][0]["name"], "web");
        assert_eq!(json["metrics"]["pods"][0]["status"], "Running");
        assert_eq!(json["metrics"]["pods"][0]["restart_count"], 2);
        assert!(json["metrics"]["pods"][0]["usage"].is_null());

        let failed = cluster("prod", Usage::default(), None);
        let json = serde_json::to_value(failed).expect("json");
        assert!(json["metrics"]["pods"].is_null());
    }

    #[test]
    fn pod_status_uses_kubernetes_spelling() {
        let json = serde_json::to_value([PodStatus::OomKilled, PodStatus::CrashLoopBackOff])
            .expect("json");
        assert_eq!(json, serde_json::json!(["OOMKilled", "CrashLoopBackOff"]));
    }

    #[test]
    fn classify_level_thresholds() {
        assert_eq!(classify_level(59.9), MetricLevel::Ok);
        assert_eq!(classify_level(60.0), MetricLevel::Warn);
        assert_eq!(classify_level(84.9), MetricLevel::Warn);
        assert_eq!(classify_level(85.0), MetricLevel::Crit);
    }

    #[test]
    fn worst_level_covers_hosts_clusters_and_pods_but_not_offline() {
        assert_eq!(worst_level(&[]), MetricLevel::Ok);
        assert_eq!(
            worst_level(&[host("a", usage(10.0, 70.0, 0.0)), offline("b")]),
            MetricLevel::Warn
        );

        let hot_pod = pod("web", 0, Some(usage(95.0, 0.0, 0.0)));
        let reports = [cluster(
            "prod",
            usage(10.0, 10.0, 10.0),
            Some(vec![hot_pod]),
        )];
        assert_eq!(worst_level(&reports), MetricLevel::Crit);
    }

    #[test]
    fn online_usage_labels_pods_with_their_cluster() {
        let reports = [cluster(
            "prod",
            Usage::default(),
            Some(vec![
                pod("web", 0, Some(Usage::default())),
                pod("idle", 0, None),
            ]),
        )];

        let labels: Vec<String> = online_usage(&reports).map(|(label, _)| label).collect();

        assert_eq!(labels, ["prod", "prod/web"]);
    }

    #[test]
    fn has_restarts_only_counts_online_cluster_pods() {
        assert!(!has_restarts(&[]));
        assert!(!has_restarts(&[cluster(
            "p",
            Usage::default(),
            Some(vec![pod("a", 0, None)])
        )]));
        assert!(has_restarts(&[cluster(
            "p",
            Usage::default(),
            Some(vec![pod("a", 3, None)])
        )]));
        assert!(!has_restarts(&[cluster("p", Usage::default(), None)]));
    }

    #[test]
    fn net_rate_from_two_samples() {
        let start = Instant::now();
        let prev = NetSample {
            rx_bytes: 1_000,
            tx_bytes: 5_000,
            at: start,
        };
        let now = NetSample {
            rx_bytes: 3_000,
            tx_bytes: 4_000,
            at: start + Duration::from_secs(2),
        };

        assert_eq!(
            now.rate_since(&prev),
            Some(NetRate {
                rx_bytes_per_sec: 1_000,
                tx_bytes_per_sec: 0,
            })
        );
        assert_eq!(prev.rate_since(&prev), None, "zero elapsed time");
        assert_eq!(prev.rate_since(&now), None, "clock order reversed");
    }

    #[test]
    fn capacity_percent_handles_zero() {
        let full = Capacity {
            used_bytes: 50,
            capacity_bytes: 200,
        };
        assert!((full.percent() - 25.0).abs() < f64::EPSILON);
        assert!(Capacity::default().percent().abs() < f64::EPSILON);
    }

    #[test]
    fn severity_from_label_parses_known_values() {
        assert_eq!(
            AlertSeverity::from_label(Some("critical")),
            AlertSeverity::Critical
        );
        assert_eq!(
            AlertSeverity::from_label(Some("warning")),
            AlertSeverity::Warning
        );
        assert_eq!(AlertSeverity::from_label(Some("info")), AlertSeverity::Info);
        assert_eq!(
            AlertSeverity::from_label(Some("CRITICAL")),
            AlertSeverity::Critical
        );
        assert_eq!(
            AlertSeverity::from_label(Some("page")),
            AlertSeverity::Unknown
        );
        assert_eq!(AlertSeverity::from_label(None), AlertSeverity::Unknown);
    }

    #[test]
    fn severity_to_level_maps_to_tray_levels() {
        assert_eq!(
            severity_to_level(AlertSeverity::Critical),
            MetricLevel::Crit
        );
        assert_eq!(severity_to_level(AlertSeverity::Warning), MetricLevel::Warn);
        assert_eq!(severity_to_level(AlertSeverity::Info), MetricLevel::Ok);
        assert_eq!(severity_to_level(AlertSeverity::Unknown), MetricLevel::Ok);
    }

    pub(crate) fn make_alert(
        fingerprint: &str,
        severity: AlertSeverity,
        state: AlertState,
    ) -> Alert {
        Alert {
            fingerprint: fingerprint.to_string(),
            name: "Test".to_string(),
            severity,
            state,
            summary: String::new(),
            description: String::new(),
            starts_at: String::new(),
            labels: BTreeMap::new(),
            generator_url: None,
        }
    }

    #[test]
    fn worst_alert_level_ignores_suppressed() {
        assert_eq!(worst_alert_level(&[]), MetricLevel::Ok);
        let alerts = [
            make_alert("a", AlertSeverity::Warning, AlertState::Active),
            make_alert("b", AlertSeverity::Critical, AlertState::Suppressed),
        ];
        assert_eq!(worst_alert_level(&alerts), MetricLevel::Warn);
    }

    #[test]
    fn newly_firing_returns_only_new_active_alerts() {
        let prev: HashSet<String> = HashSet::from(["known".to_string()]);
        let alerts = vec![
            make_alert("known", AlertSeverity::Critical, AlertState::Active),
            make_alert("fresh", AlertSeverity::Warning, AlertState::Active),
            make_alert("muted", AlertSeverity::Critical, AlertState::Suppressed),
        ];
        let fired: Vec<&str> = newly_firing(&prev, &alerts)
            .iter()
            .map(|a| a.fingerprint.as_str())
            .collect();
        assert_eq!(fired, vec!["fresh"]);
    }

    #[test]
    fn alert_serializes_enums_lowercase() {
        let alert = make_alert("fp", AlertSeverity::Critical, AlertState::Active);
        let json = serde_json::to_value(&alert).expect("serialize");
        assert_eq!(json["severity"], "critical");
        assert_eq!(json["state"], "active");
    }
}

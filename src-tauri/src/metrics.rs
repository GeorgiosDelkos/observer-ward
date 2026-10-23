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

use crate::config::{K8sTarget, ServerName, WorkloadRef};

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

/// One cluster: totals over all its nodes, each node, and the pods of each
/// watched namespace.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClusterMetrics {
    // Totals over the nodes with a metrics-server sample, against their
    // allocatable capacity.
    #[serde(flatten)]
    pub usage: ResourceUsage,
    pub net: Option<NetRate>,
    pub disk: Capacity,
    /// What the cluster's pods reserve, as a share of allocatable. `None`
    /// when pods cannot be listed across all namespaces.
    pub requested: Option<Requested>,

    /// Sorted by name.
    pub nodes: Vec<NodeMetrics>,
    /// In the order the cluster's config lists them.
    pub namespaces: Vec<NamespaceMetrics>,
}

impl ClusterMetrics {
    /// Every listed pod with its namespace.
    pub fn pods(&self) -> impl Iterator<Item = (&str, &PodMetrics)> {
        self.namespaces.iter().flat_map(|namespace| {
            let pods = namespace.pods.iter().flatten();
            pods.map(|pod| (namespace.name.as_str(), pod))
        })
    }

    /// Drop every namespace `target` no longer watches, and the pods of
    /// every workload it hides in the rest. A namespace unwatched while a
    /// poll was running must not notify or reach the tray.
    pub fn hide(&mut self, target: &K8sTarget) {
        self.namespaces
            .retain(|ns| target.namespace(&ns.name).is_some());
        for namespace in &mut self.namespaces {
            let (Some(watched), Some(pods)) =
                (target.namespace(&namespace.name), &mut namespace.pods)
            else {
                continue;
            };
            pods.retain(|pod| !watched.hidden.contains(&pod.workload));
        }
    }
}

/// CPU and memory reserved by the requests of every pod placed on a node
/// and not finished, against the allocatable capacity of the nodes that
/// report it. The scheduler
/// places pods by requests, so a cluster can idle at low usage and still
/// have no room for a new pod.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Requested {
    pub cpu_percent: f64,
    pub memory_percent: f64,
}

/// CPU in millicores and memory in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Resources {
    pub cpu_millicores: f64,
    pub memory_bytes: u64,
}

impl Resources {
    pub fn add(&mut self, other: Resources) {
        self.cpu_millicores += other.cpu_millicores;
        self.memory_bytes = self.memory_bytes.saturating_add(other.memory_bytes);
    }

    /// CPU and memory percentages of `self` against `whole`; disk is left
    /// at 0 for the caller to fill in.
    #[must_use]
    pub fn share_of(&self, whole: &Resources) -> Usage {
        #[expect(
            clippy::cast_precision_loss,
            reason = "byte counts fit comfortably in f64 for a percentage"
        )]
        let memory_percent = percent(self.memory_bytes as f64, whole.memory_bytes as f64);
        Usage {
            cpu_percent: percent(self.cpu_millicores, whole.cpu_millicores),
            memory_percent,
            disk_percent: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NodeMetrics {
    pub name: String,
    /// From `node-role.kubernetes.io/<role>` labels, e.g. `control-plane`.
    pub roles: Vec<String>,
    /// The `Ready` condition is `True`.
    pub ready: bool,
    /// Cordoned: no new pods are scheduled here.
    pub unschedulable: bool,
    /// Conditions that are `True` and mean trouble.
    pub problems: Vec<NodeProblem>,
    pub kubelet_version: Option<String>,
    /// `None` when the node reports no allocatable CPU and memory.
    pub allocatable: Option<Resources>,
    /// Against allocatable, with the root filesystem as disk. `None` when
    /// metrics-server has no sample for the node.
    pub usage: Option<ResourceUsage>,
    /// The node's root filesystem. `None` when its kubelet stats failed.
    pub disk: Option<Capacity>,
}

/// A node condition that is `True` when something is wrong. Serialized
/// with Kubernetes' own spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum NodeProblem {
    MemoryPressure,
    DiskPressure,
    #[serde(rename = "PIDPressure")]
    PidPressure,
    NetworkUnavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NamespaceMetrics {
    pub name: String,
    /// `None` when listing them failed: the UI must then keep the previous
    /// cards, not treat the namespace as empty.
    pub pods: Option<Vec<PodMetrics>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PodMetrics {
    pub name: String,
    /// What hiding this pod hides.
    pub workload: WorkloadRef,
    /// `None` when the pod reports no status yet.
    pub status: Option<PodStatus>,
    pub restart_count: u32,
    /// RFC 3339, second precision.
    pub start_time: Option<String>,
    /// Newest event, as `"{type}: {reason}"`.
    pub last_event: Option<String>,
    /// The node the pod is scheduled on; `None` while it is unscheduled.
    pub node: Option<String>,
    /// `None` when metrics-server has no sample for the pod yet.
    pub usage: Option<PodUsage>,
    pub net: Option<NetRate>,
    /// Summed over the pod's PVC-backed volumes; `None` without any.
    pub pvc: Option<Capacity>,
}

/// Usage from a metrics-server sample, as percentages and raw values.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ResourceUsage {
    #[serde(flatten)]
    pub usage: Usage,
    #[serde(flatten)]
    pub used: Resources,
}

/// A pod's usage from its metrics-server sample: percentages of
/// `ceiling`, and raw values.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PodUsage {
    #[serde(flatten)]
    pub usage: Usage,
    #[serde(flatten)]
    pub used: Resources,
    pub ceiling: Ceiling,
}

/// What a pod's CPU and memory percentages are shares of. The two may come
/// from different places: a pod can limit memory and not CPU.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Ceiling {
    /// In millicores.
    pub cpu: Bound<f64>,
    /// In bytes.
    pub memory: Bound<u64>,
}

impl Ceiling {
    #[must_use]
    pub fn resources(&self) -> Resources {
        Resources {
            cpu_millicores: self.cpu.amount,
            memory_bytes: self.memory.amount,
        }
    }
}

/// One resource's ceiling and where it comes from.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Bound<T> {
    pub amount: T,
    pub source: CeilingSource,
}

impl<T> Bound<T> {
    /// A ceiling set by the pod's own limit.
    pub fn limit(amount: T) -> Self {
        Self {
            amount,
            source: CeilingSource::Limit,
        }
    }
}

/// Where a pod's ceiling for one resource comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CeilingSource {
    /// The pod's limit, pod-level or summed over its containers.
    Limit,
    /// Without a limit, the pod may use whatever its node has free.
    Node,
    /// The pod's node is not known this poll; the cluster's allocatable
    /// stands in, which understates the pod.
    Cluster,
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
/// server names for hosts and clusters, `cluster node <name>` for nodes,
/// and `cluster/namespace/pod` for pods. A node label never looks like a
/// pod label, even under a legacy cluster name with `/`.
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
                entries.push((name.to_string(), metrics.usage.usage));
                // A one-node cluster's total is that node: one problem, one
                // notification.
                let nodes = if metrics.nodes.len() > 1 {
                    metrics.nodes.as_slice()
                } else {
                    &[]
                };
                for node in nodes {
                    if let Some(usage) = node.usage {
                        entries.push((format!("{name} node {}", node.name), usage.usage));
                    }
                }
                for (namespace, pod) in metrics.pods() {
                    if let Some(usage) = pod.usage {
                        entries.push((format!("{name}/{namespace}/{}", pod.name), usage.usage));
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
        } => metrics.pods().any(|(_, pod)| pod.restart_count > 0),
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
    clippy::panic,
    reason = "panicking on failure is standard in tests"
)]
pub(crate) mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use super::*;
    use crate::config::WatchedNamespace;

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
            workload: WorkloadRef {
                kind: "Pod".to_string(),
                name: name.to_string(),
            },
            status: Some(PodStatus::Running),
            restart_count: restarts,
            start_time: None,
            last_event: None,
            node: None,
            usage: usage.map(|usage| PodUsage {
                usage,
                used: Resources::default(),
                ceiling: Ceiling {
                    cpu: Bound {
                        amount: 0.0,
                        source: CeilingSource::Limit,
                    },
                    memory: Bound {
                        amount: 0,
                        source: CeilingSource::Limit,
                    },
                },
            }),
            net: None,
            pvc: None,
        }
    }

    fn sampled(usage: Usage) -> ResourceUsage {
        ResourceUsage {
            usage,
            used: Resources::default(),
        }
    }

    /// A ready node with a metrics sample.
    pub(crate) fn node(name: &str, usage: Usage) -> NodeMetrics {
        NodeMetrics {
            name: name.to_string(),
            roles: Vec::new(),
            ready: true,
            unschedulable: false,
            problems: Vec::new(),
            kubelet_version: None,
            allocatable: None,
            usage: Some(sampled(usage)),
            disk: None,
        }
    }

    /// An online cluster without nodes, watching one namespace, `default`.
    pub(crate) fn cluster(name: &str, usage: Usage, pods: Option<Vec<PodMetrics>>) -> ServerReport {
        ServerReport::K8s {
            name: name.into(),
            health: Health::Online {
                metrics: ClusterMetrics {
                    usage: sampled(usage),
                    net: None,
                    disk: Capacity::default(),
                    requested: None,
                    nodes: Vec::new(),
                    namespaces: vec![NamespaceMetrics {
                        name: "default".to_string(),
                        pods,
                    }],
                },
            },
        }
    }

    pub(crate) fn metrics_mut(report: &mut ServerReport) -> &mut ClusterMetrics {
        match report {
            ServerReport::K8s {
                health: Health::Online { metrics },
                ..
            } => metrics,
            ServerReport::K8s { .. } | ServerReport::Ssh { .. } => {
                panic!("not an online cluster")
            }
        }
    }

    #[test]
    fn hide_drops_only_pods_of_hidden_workloads_in_their_namespace() {
        let mut report = cluster(
            "prod",
            usage(0.0, 0.0, 0.0),
            Some(vec![pod("api-1", 0, None), pod("db-0", 0, None)]),
        );
        let metrics = metrics_mut(&mut report);
        metrics.namespaces.push(NamespaceMetrics {
            name: "s3".to_string(),
            pods: Some(vec![pod("api-1", 0, None)]),
        });
        let api = metrics.namespaces[0].pods.as_ref().expect("pods")[0]
            .workload
            .clone();
        let target = K8sTarget {
            name: "prod".into(),
            kubeconfig: None,
            context: "ctx".to_string(),
            namespaces: vec![
                WatchedNamespace {
                    name: "default".to_string(),
                    hidden: [api].into(),
                },
                WatchedNamespace {
                    name: "s3".to_string(),
                    hidden: BTreeSet::new(),
                },
            ],
        };

        metrics.hide(&target);

        let names: Vec<_> = metrics
            .pods()
            .map(|(ns, p)| format!("{ns}/{}", p.name))
            .collect();
        assert_eq!(names, ["default/db-0", "s3/api-1"]);

        metrics.namespaces[0].pods = None;
        metrics.hide(&target);
        assert!(
            metrics.namespaces[0].pods.is_none(),
            "a failed listing stays failed"
        );

        let mut unwatched = target;
        unwatched.namespaces.retain(|ns| ns.name != "s3");
        metrics.hide(&unwatched);
        let names: Vec<&str> = metrics.namespaces.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["default"], "unwatched mid-poll");
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
    fn cluster_serializes_namespaces_with_pods_or_null() {
        let listed = cluster("prod", Usage::default(), Some(vec![pod("web", 2, None)]));
        let json = serde_json::to_value(listed).expect("json");
        assert_eq!(json["kind"], "k8s");
        let namespace = &json["metrics"]["namespaces"][0];
        assert_eq!(namespace["name"], "default");
        assert_eq!(namespace["pods"][0]["name"], "web");
        assert_eq!(namespace["pods"][0]["status"], "Running");
        assert_eq!(namespace["pods"][0]["restart_count"], 2);
        assert!(namespace["pods"][0]["usage"].is_null());
        assert!(json["metrics"]["requested"].is_null());

        let failed = cluster("prod", Usage::default(), None);
        let json = serde_json::to_value(failed).expect("json");
        assert!(json["metrics"]["namespaces"][0]["pods"].is_null());
    }

    #[test]
    fn node_serializes_flat_usage_and_kubernetes_problem_names() {
        let mut node = node("hp-07", usage(12.0, 39.0, 88.0));
        node.problems = vec![NodeProblem::DiskPressure, NodeProblem::PidPressure];
        node.usage = node.usage.map(|u| ResourceUsage {
            used: Resources {
                cpu_millicores: 6720.0,
                memory_bytes: 64,
            },
            ..u
        });

        let json = serde_json::to_value(&node).expect("json");

        assert_eq!(json["usage"]["disk_percent"], 88.0);
        assert_eq!(json["usage"]["cpu_millicores"], 6720.0);
        assert_eq!(json["usage"]["memory_bytes"], 64);
        assert_eq!(
            json["problems"],
            serde_json::json!(["DiskPressure", "PIDPressure"])
        );
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
    fn online_usage_labels_nodes_and_pods_with_their_cluster() {
        let mut report = cluster(
            "prod",
            Usage::default(),
            Some(vec![
                pod("web", 0, Some(Usage::default())),
                pod("idle", 0, None),
            ]),
        );
        let metrics = metrics_mut(&mut report);
        metrics.nodes = vec![node("n1", usage(1.0, 1.0, 95.0))];
        let mut unsampled = node("n2", Usage::default());
        unsampled.usage = None;
        metrics.nodes.push(unsampled);
        let reports = [report];

        let labels: Vec<String> = online_usage(&reports).map(|(label, _)| label).collect();

        assert_eq!(labels, ["prod", "prod node n1", "prod/default/web"]);
        assert_eq!(worst_level(&reports), MetricLevel::Crit, "a full node disk");

        let [mut single] = reports;
        metrics_mut(&mut single).nodes.truncate(1);
        let labels: Vec<String> = online_usage(std::slice::from_ref(&single))
            .map(|(label, _)| label)
            .collect();
        assert_eq!(
            labels,
            ["prod", "prod/default/web"],
            "a lone node is the cluster total"
        );
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

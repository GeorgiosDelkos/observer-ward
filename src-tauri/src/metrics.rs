use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::hash::BuildHasher;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerMetrics {
    pub server_name: String,
    pub server_type: String,
    pub status: ServerStatus,
    pub cpu_percent: f64,
    pub memory_percent: f64,
    pub disk_percent: f64,
    pub net_rx_bytes_per_sec: u64,
    pub net_tx_bytes_per_sec: u64,
    #[serde(default)]
    pub cpu_millicores: f64,
    #[serde(default)]
    pub memory_bytes: u64,
    #[serde(default)]
    pub restart_count: u32,
    #[serde(default)]
    pub start_time: String,
    #[serde(default)]
    pub pod_status: String,
    #[serde(default)]
    pub pvc_used_bytes: u64,
    #[serde(default)]
    pub pvc_capacity_bytes: u64,
    #[serde(default)]
    pub last_event: String,
    #[serde(default)]
    pub disk_used_bytes: u64,
    #[serde(default)]
    pub disk_capacity_bytes: u64,
    #[serde(default)]
    pub node_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ServerStatus {
    #[default]
    Pending,
    Online,
    Offline,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MetricLevel {
    Ok,
    Warn,
    Crit,
}

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

#[must_use]
pub fn worst_level(metrics: &[ServerMetrics]) -> MetricLevel {
    metrics
        .iter()
        .filter(|m| m.status == ServerStatus::Online)
        .flat_map(|m| [m.cpu_percent, m.memory_percent, m.disk_percent])
        .map(classify_level)
        .max()
        .unwrap_or(MetricLevel::Ok)
}

#[must_use]
pub fn has_restarts(metrics: &[ServerMetrics]) -> bool {
    metrics
        .iter()
        .filter(|m| m.status == ServerStatus::Online)
        .any(|m| m.restart_count > 0)
}

/// Event payload sent to the frontend via `app.emit("metrics-update", ...)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsUpdate {
    pub servers: Vec<ServerMetrics>,
}

/// Severity of a Grafana alert, derived from the conventional
/// `severity` label. A missing or unrecognized label maps to
/// `Unknown` so an alert is never silently dropped for lacking the
/// label.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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
            _ => AlertSeverity::Unknown,
        }
    }
}

/// Whether an alert is actively firing or suppressed (silenced or
/// inhibited in Grafana). Suppressed alerts are shown but do not raise
/// the tray level or fire notifications.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AlertState {
    Active,
    Suppressed,
}

/// A single Grafana alert as displayed by the app. `fingerprint` is
/// Grafana's stable per-alert identity, used as the notification dedup
/// key across poll cycles.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

/// Event payload sent to the frontend via `app.emit("alerts-update", ...)`.
/// `source_error` is `Some` when the fetch failed, so the UI can show an
/// "unreachable" state instead of a stale list.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Active alert fingerprints present now but absent from `prev` — the
/// set that should raise a fresh notification this cycle. Suppressed
/// alerts never appear here.
#[must_use]
pub fn newly_firing<'a, S: BuildHasher>(
    prev: &std::collections::HashSet<String, S>,
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
mod tests {
    use super::*;

    fn assert_f64_eq(left: f64, right: f64) {
        assert!(
            (left - right).abs() < f64::EPSILON,
            "expected {right}, got {left}"
        );
    }

    #[test]
    fn default_server_metrics_has_zeroed_values() {
        let m = ServerMetrics::default();

        assert_eq!(m.server_name, "");
        assert_eq!(m.server_type, "");
        assert_eq!(m.status, ServerStatus::Pending);
        assert_f64_eq(m.cpu_percent, 0.0);
        assert_f64_eq(m.memory_percent, 0.0);
        assert_f64_eq(m.disk_percent, 0.0);
        assert_eq!(m.net_rx_bytes_per_sec, 0);
        assert_eq!(m.net_tx_bytes_per_sec, 0);
        assert_f64_eq(m.cpu_millicores, 0.0);
        assert_eq!(m.memory_bytes, 0);
        assert_eq!(m.restart_count, 0);
        assert_eq!(m.start_time, "");
        assert_eq!(m.pod_status, "");
        assert_eq!(m.pvc_used_bytes, 0);
        assert_eq!(m.pvc_capacity_bytes, 0);
        assert_eq!(m.last_event, "");
        assert_eq!(m.disk_used_bytes, 0);
        assert_eq!(m.disk_capacity_bytes, 0);
        assert_eq!(m.node_count, 0);
    }

    #[test]
    fn default_status_is_pending() {
        assert_eq!(ServerStatus::default(), ServerStatus::Pending);
    }

    #[test]
    fn serialize_server_metrics_matches_frontend_schema() {
        let m = ServerMetrics {
            server_name: "web-1".to_string(),
            server_type: "ssh".to_string(),
            status: ServerStatus::Online,
            cpu_percent: 42.5,
            memory_percent: 61.3,
            disk_percent: 78.0,
            net_rx_bytes_per_sec: 1_048_576,
            net_tx_bytes_per_sec: 524_288,
            ..ServerMetrics::default()
        };
        let json = serde_json::to_string(&m).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");

        assert_eq!(v["server_name"], "web-1");
        assert_eq!(v["server_type"], "ssh");
        assert_eq!(v["status"], "online");
        assert_eq!(v["cpu_percent"], 42.5);
        assert_eq!(v["memory_percent"], 61.3);
        assert_eq!(v["disk_percent"], 78.0);
        assert_eq!(v["net_rx_bytes_per_sec"], 1_048_576);
        assert_eq!(v["net_tx_bytes_per_sec"], 524_288);
    }

    #[test]
    fn status_serializes_to_lowercase() {
        let cases = [
            (ServerStatus::Pending, "pending"),
            (ServerStatus::Online, "online"),
            (ServerStatus::Offline, "offline"),
            (ServerStatus::Error, "error"),
        ];
        for (status, expected) in cases {
            let json = serde_json::to_string(&status).expect("serialize status");
            assert_eq!(json, format!("\"{expected}\""));
        }
    }

    #[test]
    fn deserialize_status_from_lowercase() {
        let cases = [
            ("\"pending\"", ServerStatus::Pending),
            ("\"online\"", ServerStatus::Online),
            ("\"offline\"", ServerStatus::Offline),
            ("\"error\"", ServerStatus::Error),
        ];
        for (json, expected) in cases {
            let status: ServerStatus = serde_json::from_str(json).expect("deserialize status");
            assert_eq!(status, expected);
        }
    }

    #[test]
    fn deserialize_unknown_status_fails() {
        let result: Result<ServerStatus, _> = serde_json::from_str("\"unknown\"");
        assert!(result.is_err());
    }

    #[test]
    fn serialize_metrics_update_wraps_servers() {
        let update = MetricsUpdate {
            servers: vec![
                ServerMetrics {
                    server_name: "node-a".to_string(),
                    server_type: "k8s".to_string(),
                    status: ServerStatus::Online,
                    cpu_percent: 10.0,
                    memory_percent: 20.0,
                    disk_percent: 30.0,
                    net_rx_bytes_per_sec: 100,
                    net_tx_bytes_per_sec: 200,
                    ..ServerMetrics::default()
                },
                ServerMetrics {
                    server_name: "node-b".to_string(),
                    server_type: "ssh".to_string(),
                    status: ServerStatus::Offline,
                    ..ServerMetrics::default()
                },
            ],
        };
        let json = serde_json::to_string(&update).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");

        let servers = v["servers"].as_array().expect("servers is array");
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0]["server_name"], "node-a");
        assert_eq!(servers[0]["status"], "online");
        assert_eq!(servers[1]["server_name"], "node-b");
        assert_eq!(servers[1]["status"], "offline");
    }

    #[test]
    fn roundtrip_metrics_update() {
        let original = MetricsUpdate {
            servers: vec![ServerMetrics {
                server_name: "roundtrip".to_string(),
                server_type: "ssh".to_string(),
                status: ServerStatus::Error,
                cpu_percent: 99.9,
                memory_percent: 88.8,
                disk_percent: 77.7,
                net_rx_bytes_per_sec: 999_999,
                net_tx_bytes_per_sec: 111_111,
                ..ServerMetrics::default()
            }],
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let restored: MetricsUpdate = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.servers.len(), 1);
        let s = &restored.servers[0];
        assert_eq!(s.server_name, "roundtrip");
        assert_eq!(s.server_type, "ssh");
        assert_eq!(s.status, ServerStatus::Error);
        assert_f64_eq(s.cpu_percent, 99.9);
        assert_f64_eq(s.memory_percent, 88.8);
        assert_f64_eq(s.disk_percent, 77.7);
        assert_eq!(s.net_rx_bytes_per_sec, 999_999);
        assert_eq!(s.net_tx_bytes_per_sec, 111_111);
    }

    #[test]
    fn deserialize_metrics_from_frontend_json() {
        let json = r#"{
            "servers": [
                {
                    "server_name": "prod-web",
                    "server_type": "ssh",
                    "status": "online",
                    "cpu_percent": 55.2,
                    "memory_percent": 72.1,
                    "disk_percent": 45.0,
                    "net_rx_bytes_per_sec": 2048,
                    "net_tx_bytes_per_sec": 4096
                }
            ]
        }"#;
        let update: MetricsUpdate = serde_json::from_str(json).expect("deserialize");

        assert_eq!(update.servers.len(), 1);
        let s = &update.servers[0];
        assert_eq!(s.server_name, "prod-web");
        assert_eq!(s.status, ServerStatus::Online);
        assert_f64_eq(s.cpu_percent, 55.2);
    }

    #[test]
    fn deserialize_old_json_without_new_fields() {
        let json = r#"{
            "server_name": "legacy-node",
            "server_type": "ssh",
            "status": "online",
            "cpu_percent": 42.0,
            "memory_percent": 60.0,
            "disk_percent": 30.0,
            "net_rx_bytes_per_sec": 100,
            "net_tx_bytes_per_sec": 200
        }"#;
        let m: ServerMetrics = serde_json::from_str(json).expect("deserialize");

        assert_eq!(m.server_name, "legacy-node");
        assert_eq!(m.restart_count, 0);
        assert_eq!(m.start_time, "");
        assert_eq!(m.pod_status, "");
        assert_eq!(m.pvc_used_bytes, 0);
        assert_eq!(m.pvc_capacity_bytes, 0);
        assert_eq!(m.last_event, "");
        assert_eq!(m.disk_used_bytes, 0);
        assert_eq!(m.disk_capacity_bytes, 0);
        assert_eq!(m.node_count, 0);
    }

    #[test]
    fn empty_metrics_update_serializes() {
        let update = MetricsUpdate {
            servers: Vec::new(),
        };
        let json = serde_json::to_string(&update).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");

        let servers = v["servers"].as_array().expect("servers is array");
        assert!(servers.is_empty());
    }

    #[test]
    fn classify_level_thresholds() {
        assert_eq!(classify_level(0.0), MetricLevel::Ok);
        assert_eq!(classify_level(59.9), MetricLevel::Ok);
        assert_eq!(classify_level(60.0), MetricLevel::Warn);
        assert_eq!(classify_level(84.9), MetricLevel::Warn);
        assert_eq!(classify_level(85.0), MetricLevel::Crit);
        assert_eq!(classify_level(100.0), MetricLevel::Crit);
    }

    #[test]
    fn worst_level_picks_highest() {
        let metrics = vec![
            ServerMetrics {
                status: ServerStatus::Online,
                cpu_percent: 50.0,
                memory_percent: 30.0,
                disk_percent: 20.0,
                ..ServerMetrics::default()
            },
            ServerMetrics {
                status: ServerStatus::Online,
                cpu_percent: 70.0,
                memory_percent: 40.0,
                disk_percent: 10.0,
                ..ServerMetrics::default()
            },
        ];
        assert_eq!(worst_level(&metrics), MetricLevel::Warn);
    }

    #[test]
    fn worst_level_skips_offline() {
        let metrics = vec![
            ServerMetrics {
                status: ServerStatus::Offline,
                cpu_percent: 95.0,
                memory_percent: 95.0,
                disk_percent: 95.0,
                ..ServerMetrics::default()
            },
            ServerMetrics {
                status: ServerStatus::Online,
                cpu_percent: 10.0,
                memory_percent: 10.0,
                disk_percent: 10.0,
                ..ServerMetrics::default()
            },
        ];
        assert_eq!(worst_level(&metrics), MetricLevel::Ok);
    }

    #[test]
    fn worst_level_empty_is_ok() {
        assert_eq!(worst_level(&[]), MetricLevel::Ok);
    }

    #[test]
    fn has_restarts_true_when_restart_count_nonzero() {
        let metrics = vec![
            ServerMetrics {
                status: ServerStatus::Online,
                restart_count: 0,
                ..ServerMetrics::default()
            },
            ServerMetrics {
                status: ServerStatus::Online,
                restart_count: 2,
                ..ServerMetrics::default()
            },
        ];
        assert!(has_restarts(&metrics));
    }

    #[test]
    fn has_restarts_false_when_all_zero() {
        let metrics = vec![
            ServerMetrics {
                status: ServerStatus::Online,
                restart_count: 0,
                ..ServerMetrics::default()
            },
            ServerMetrics {
                status: ServerStatus::Online,
                restart_count: 0,
                ..ServerMetrics::default()
            },
        ];
        assert!(!has_restarts(&metrics));
    }

    #[test]
    fn has_restarts_skips_offline() {
        let metrics = vec![ServerMetrics {
            status: ServerStatus::Offline,
            restart_count: 5,
            ..ServerMetrics::default()
        }];
        assert!(!has_restarts(&metrics));
    }

    #[test]
    fn has_restarts_empty_is_false() {
        assert!(!has_restarts(&[]));
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

    fn make_alert(fingerprint: &str, severity: AlertSeverity, state: AlertState) -> Alert {
        Alert {
            fingerprint: fingerprint.to_string(),
            name: "Test".to_string(),
            severity,
            state,
            summary: String::new(),
            description: String::new(),
            starts_at: String::new(),
            labels: std::collections::BTreeMap::new(),
            generator_url: None,
        }
    }

    #[test]
    fn worst_alert_level_picks_highest_active() {
        let alerts = vec![
            make_alert("a", AlertSeverity::Warning, AlertState::Active),
            make_alert("b", AlertSeverity::Critical, AlertState::Active),
        ];
        assert_eq!(worst_alert_level(&alerts), MetricLevel::Crit);
    }

    #[test]
    fn worst_alert_level_skips_suppressed() {
        let alerts = vec![make_alert(
            "a",
            AlertSeverity::Critical,
            AlertState::Suppressed,
        )];
        assert_eq!(worst_alert_level(&alerts), MetricLevel::Ok);
    }

    #[test]
    fn worst_alert_level_empty_is_ok() {
        assert_eq!(worst_alert_level(&[]), MetricLevel::Ok);
    }

    #[test]
    fn newly_firing_returns_only_new_active_alerts() {
        let mut prev = std::collections::HashSet::new();
        prev.insert("known".to_string());
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
        let json = serde_json::to_string(&alert).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(v["severity"], "critical");
        assert_eq!(v["state"], "active");
    }
}

use serde::{Deserialize, Serialize};

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

/// Event payload sent to the frontend via `app.emit("metrics-update", ...)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsUpdate {
    pub servers: Vec<ServerMetrics>,
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
                },
                ServerMetrics {
                    server_name: "node-b".to_string(),
                    server_type: "ssh".to_string(),
                    status: ServerStatus::Offline,
                    cpu_percent: 0.0,
                    memory_percent: 0.0,
                    disk_percent: 0.0,
                    net_rx_bytes_per_sec: 0,
                    net_tx_bytes_per_sec: 0,
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
    fn empty_metrics_update_serializes() {
        let update = MetricsUpdate {
            servers: Vec::new(),
        };
        let json = serde_json::to_string(&update).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");

        let servers = v["servers"].as_array().expect("servers is array");
        assert!(servers.is_empty());
    }
}

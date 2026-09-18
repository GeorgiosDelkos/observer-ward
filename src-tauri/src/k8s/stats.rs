//! Kubelet `/stats/summary` types and node-stats collection.

use std::collections::HashMap;
use std::time::Duration;

use k8s_openapi::api::core::v1::Node;
use kube::Client;
use serde::Deserialize;
use tokio::task::JoinSet;

use super::error::K8sError;

// -- Kubelet stats summary types --

#[derive(Debug, Deserialize)]
pub(super) struct StatsSummary {
    node: NodeStats,
    #[serde(default)]
    pods: Vec<PodStatsSummary>,
}

#[derive(Debug, Deserialize)]
pub(super) struct NodeStats {
    fs: Option<FsStats>,
    network: Option<NetworkStats>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FsStats {
    used_bytes: Option<u64>,
    capacity_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NetworkStats {
    rx_bytes: Option<u64>,
    tx_bytes: Option<u64>,
    #[serde(default)]
    interfaces: Vec<InterfaceStats>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct InterfaceStats {
    name: String,
    rx_bytes: Option<u64>,
    tx_bytes: Option<u64>,
}

impl NetworkStats {
    /// Return rx/tx bytes, falling back to summing physical
    /// interfaces (eth*, enp*, eno*, ens*) when top-level
    /// counters are absent (host-networked pods/nodes).
    fn effective_bytes(&self) -> (u64, u64) {
        if self.rx_bytes.is_some() || self.tx_bytes.is_some() {
            return (self.rx_bytes.unwrap_or(0), self.tx_bytes.unwrap_or(0));
        }
        let mut rx = 0_u64;
        let mut tx = 0_u64;
        for iface in &self.interfaces {
            let n = &iface.name;
            if n.starts_with("eth")
                || n.starts_with("enp")
                || n.starts_with("eno")
                || n.starts_with("ens")
            {
                rx = rx.saturating_add(iface.rx_bytes.unwrap_or(0));
                tx = tx.saturating_add(iface.tx_bytes.unwrap_or(0));
            }
        }
        (rx, tx)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PodStatsSummary {
    pod_ref: PodRef,
    #[serde(default)]
    volume: Vec<VolumeStats>,
    network: Option<NetworkStats>,
}

#[derive(Debug, Deserialize)]
pub(super) struct PodRef {
    name: String,
    namespace: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct VolumeStats {
    used_bytes: Option<u64>,
    capacity_bytes: Option<u64>,
    pvc_ref: Option<PvcRef>,
}

#[derive(Debug, Deserialize)]
pub(super) struct PvcRef {}

const NODE_STATS_TIMEOUT: Duration = Duration::from_secs(8);

/// Percent-encode a URL path segment so a node name cannot alter the
/// kubelet proxy path. Kubernetes node names are DNS-1123, but the
/// proxy URL is still interpolated.
pub(super) fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

#[expect(
    clippy::cast_precision_loss,
    reason = "disk byte totals fit comfortably in f64"
)]
pub(super) fn compute_cluster_disk_net(summaries: &[StatsSummary]) -> (f64, u64, u64, u64, u64) {
    let mut total_disk_used = 0_u64;
    let mut total_disk_capacity = 0_u64;
    let mut total_rx = 0_u64;
    let mut total_tx = 0_u64;

    for summary in summaries {
        if let Some(fs) = &summary.node.fs {
            total_disk_used = total_disk_used.saturating_add(fs.used_bytes.unwrap_or(0));
            total_disk_capacity =
                total_disk_capacity.saturating_add(fs.capacity_bytes.unwrap_or(0));
        }
        if let Some(net) = &summary.node.network {
            let (rx, tx) = net.effective_bytes();
            total_rx = total_rx.saturating_add(rx);
            total_tx = total_tx.saturating_add(tx);
        }
    }

    let disk_pct = if total_disk_capacity > 0 {
        total_disk_used as f64 / total_disk_capacity as f64 * 100.0
    } else {
        0.0
    };

    (
        disk_pct,
        total_rx,
        total_tx,
        total_disk_used,
        total_disk_capacity,
    )
}

/// Extract PVC used/capacity bytes per pod from kubelet stats
/// summaries, filtered to a specific namespace. Only volumes
/// with a `pvcRef` are included.
pub(super) fn extract_pod_pvc(
    summaries: &[StatsSummary],
    namespace: &str,
) -> HashMap<String, (u64, u64)> {
    let mut result: HashMap<String, (u64, u64)> = HashMap::new();

    for summary in summaries {
        for pod in &summary.pods {
            if pod.pod_ref.namespace != namespace {
                continue;
            }
            for vol in &pod.volume {
                if vol.pvc_ref.is_none() {
                    continue;
                }
                let entry = result.entry(pod.pod_ref.name.clone()).or_insert((0, 0));
                entry.0 = entry.0.saturating_add(vol.used_bytes.unwrap_or(0));
                entry.1 = entry.1.saturating_add(vol.capacity_bytes.unwrap_or(0));
            }
        }
    }

    result
}

/// Extract cumulative network rx/tx bytes per pod from kubelet
/// stats summaries, filtered to a specific namespace.
/// Host-networked pods lack top-level `rxBytes`/`txBytes`;
/// `effective_bytes()` falls back to summing physical interfaces.
pub(super) fn extract_pod_network(
    summaries: &[StatsSummary],
    namespace: &str,
) -> HashMap<String, (u64, u64)> {
    let mut result: HashMap<String, (u64, u64)> = HashMap::new();

    for summary in summaries {
        for pod in &summary.pods {
            if pod.pod_ref.namespace != namespace {
                continue;
            }
            let Some(net) = &pod.network else {
                continue;
            };
            let (rx, tx) = net.effective_bytes();
            let entry = result.entry(pod.pod_ref.name.clone()).or_insert((0, 0));
            entry.0 = entry.0.saturating_add(rx);
            entry.1 = entry.1.saturating_add(tx);
        }
    }

    result
}

/// Fetch the kubelet stats summary for a single node via the
/// node proxy API.
pub(super) async fn fetch_node_stats(
    client: &Client,
    node_name: &str,
) -> Result<StatsSummary, K8sError> {
    let encoded = encode_path_segment(node_name);
    let url = format!("/api/v1/nodes/{encoded}/proxy/stats/summary");

    let request = http::Request::get(&url)
        .body(Vec::new())
        .map_err(|source| K8sError::BuildStatsRequest {
            node: node_name.to_string(),
            source,
        })?;

    client
        .request::<StatsSummary>(request)
        .await
        .map_err(|source| K8sError::FetchNodeStats {
            node: node_name.to_string(),
            source: Box::new(source),
        })
}

/// Fetch kubelet stats summaries from the given cluster nodes.
///
/// Nodes are fetched concurrently. Individual node failures
/// are logged and skipped rather than aborting the entire
/// operation.
pub(super) async fn fetch_all_node_stats(client: &Client, nodes: &[Node]) -> Vec<StatsSummary> {
    let mut tasks = JoinSet::new();

    for node in nodes {
        let name = node.metadata.name.as_deref().unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let client = client.clone();
        let name = name.to_string();
        tasks.spawn(async move {
            match tokio::time::timeout(NODE_STATS_TIMEOUT, fetch_node_stats(&client, &name)).await {
                Ok(result) => result,
                Err(_) => Err(super::error::K8sError::NodeStatsTimeout { node: name }),
            }
        });
    }

    let mut summaries = Vec::with_capacity(nodes.len());
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok(summary)) => summaries.push(summary),
            Ok(Err(e)) => {
                tracing::warn!("skipping node stats: {}", crate::error::error_chain(&e));
            }
            Err(e) => {
                tracing::warn!("node stats task panicked: {e}");
            }
        }
    }

    summaries
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::*;

    #[test]
    fn encode_path_segment_leaves_dns_names() {
        assert_eq!(encode_path_segment("worker-1.prod"), "worker-1.prod");
    }

    #[test]
    fn encode_path_segment_escapes_slash() {
        assert_eq!(encode_path_segment("a/b"), "a%2Fb");
    }

    // -- Kubelet stats JSON parsing --

    #[test]
    fn parse_stats_summary_full() {
        let json = r#"{
            "node": {
                "fs": {
                    "usedBytes": 50000000000,
                    "capacityBytes": 100000000000
                },
                "network": {
                    "rxBytes": 123456789,
                    "txBytes": 987654321
                }
            }
        }"#;
        let summary: StatsSummary = serde_json::from_str(json).expect("parse stats");

        let fs = summary.node.fs.expect("fs present");
        assert_eq!(fs.used_bytes, Some(50_000_000_000));
        assert_eq!(fs.capacity_bytes, Some(100_000_000_000));

        let net = summary.node.network.expect("network present");
        assert_eq!(net.rx_bytes, Some(123_456_789));
        assert_eq!(net.tx_bytes, Some(987_654_321));
    }

    #[test]
    fn parse_stats_summary_missing_optional_fields() {
        let json = r#"{
            "node": {
                "fs": {
                    "usedBytes": null,
                    "capacityBytes": null
                },
                "network": null
            }
        }"#;
        let summary: StatsSummary = serde_json::from_str(json).expect("parse stats");

        let fs = summary.node.fs.expect("fs present");
        assert_eq!(fs.used_bytes, None);
        assert_eq!(fs.capacity_bytes, None);
        assert!(summary.node.network.is_none());
    }

    #[test]
    fn parse_stats_summary_no_fs_no_network() {
        let json = r#"{"node": {}}"#;
        let summary: StatsSummary = serde_json::from_str(json).expect("parse stats");

        assert!(summary.node.fs.is_none());
        assert!(summary.node.network.is_none());
    }

    #[test]
    fn parse_stats_summary_extra_fields_ignored() {
        let json = r#"{
            "node": {
                "nodeName": "worker-1",
                "cpu": {"usageNanoCores": 500000000},
                "memory": {"usageBytes": 4294967296},
                "fs": {
                    "usedBytes": 10000000000,
                    "capacityBytes": 50000000000,
                    "availableBytes": 40000000000,
                    "inodes": 3276800,
                    "inodesFree": 3000000
                },
                "network": {
                    "name": "eth0",
                    "rxBytes": 1000000,
                    "txBytes": 2000000,
                    "rxErrors": 0,
                    "txErrors": 0
                },
                "systemContainers": []
            }
        }"#;
        let summary: StatsSummary = serde_json::from_str(json).expect("parse stats");

        let fs = summary.node.fs.expect("fs present");
        assert_eq!(fs.used_bytes, Some(10_000_000_000));
        assert_eq!(fs.capacity_bytes, Some(50_000_000_000));

        let net = summary.node.network.expect("network present");
        assert_eq!(net.rx_bytes, Some(1_000_000));
        assert_eq!(net.tx_bytes, Some(2_000_000));
    }

    #[test]
    fn parse_stats_summary_partial_fs() {
        let json = r#"{
            "node": {
                "fs": {
                    "usedBytes": 5000000000
                },
                "network": {
                    "txBytes": 100
                }
            }
        }"#;
        let summary: StatsSummary = serde_json::from_str(json).expect("parse stats");

        let fs = summary.node.fs.expect("fs present");
        assert_eq!(fs.used_bytes, Some(5_000_000_000));
        assert_eq!(fs.capacity_bytes, None);

        let net = summary.node.network.expect("network present");
        assert_eq!(net.rx_bytes, None);
        assert_eq!(net.tx_bytes, Some(100));
    }

    #[test]
    fn parse_stats_summary_with_pod_volumes() {
        let json = r#"{
            "node": {
                "fs": { "usedBytes": 100, "capacityBytes": 200 }
            },
            "pods": [
                {
                    "podRef": {
                        "name": "web-0",
                        "namespace": "default"
                    },
                    "volume": [
                        {
                            "usedBytes": 5000000000,
                            "capacityBytes": 10000000000,
                            "pvcRef": { "name": "data-web-0" }
                        },
                        {
                            "usedBytes": 100,
                            "capacityBytes": 200
                        }
                    ]
                },
                {
                    "podRef": {
                        "name": "worker-1",
                        "namespace": "default"
                    },
                    "volume": []
                }
            ]
        }"#;
        let summary: StatsSummary = serde_json::from_str(json).expect("parse stats");

        assert_eq!(summary.pods.len(), 2);
        assert_eq!(summary.pods[0].pod_ref.name, "web-0");
        assert_eq!(summary.pods[0].volume.len(), 2);
        assert_eq!(summary.pods[0].volume[0].used_bytes, Some(5_000_000_000));
        assert!(summary.pods[0].volume[0].pvc_ref.is_some());
        assert!(summary.pods[0].volume[1].pvc_ref.is_none());
        assert!(summary.pods[1].volume.is_empty());
    }

    #[test]
    fn parse_stats_summary_without_pods_field() {
        let json = r#"{"node": {}}"#;
        let summary: StatsSummary = serde_json::from_str(json).expect("parse stats");
        assert!(summary.pods.is_empty());
    }

    // -- Pod network deserialization --

    #[test]
    fn parse_pod_stats_with_network() {
        let json = r#"{
            "node": {},
            "pods": [
                {
                    "podRef": {
                        "name": "web-0",
                        "namespace": "default"
                    },
                    "volume": [],
                    "network": {
                        "rxBytes": 1000000,
                        "txBytes": 2000000
                    }
                },
                {
                    "podRef": {
                        "name": "worker-1",
                        "namespace": "default"
                    },
                    "volume": []
                }
            ]
        }"#;
        let summary: StatsSummary = serde_json::from_str(json).expect("parse stats");

        assert_eq!(summary.pods.len(), 2);
        let net = summary.pods[0].network.as_ref().expect("network present");
        assert_eq!(net.rx_bytes, Some(1_000_000));
        assert_eq!(net.tx_bytes, Some(2_000_000));
        assert!(summary.pods[1].network.is_none());
    }

    #[test]
    fn extract_pod_network_filters_namespace() {
        let json = r#"{
            "node": {},
            "pods": [
                {
                    "podRef": {
                        "name": "app-0",
                        "namespace": "prod"
                    },
                    "volume": [],
                    "network": {
                        "rxBytes": 100,
                        "txBytes": 200
                    }
                },
                {
                    "podRef": {
                        "name": "app-1",
                        "namespace": "staging"
                    },
                    "volume": [],
                    "network": {
                        "rxBytes": 300,
                        "txBytes": 400
                    }
                }
            ]
        }"#;
        let summary: StatsSummary = serde_json::from_str(json).expect("parse");
        let result = extract_pod_network(&[summary], "prod");

        assert_eq!(result.len(), 1);
        assert_eq!(result.get("app-0"), Some(&(100, 200)));
        assert!(!result.contains_key("app-1"));
    }

    #[test]
    fn extract_pod_network_host_networked_sums_physical_interfaces() {
        let json = r#"{
            "node": {},
            "pods": [
                {
                    "podRef": {
                        "name": "gateway-0",
                        "namespace": "default"
                    },
                    "volume": [],
                    "network": {
                        "time": "2026-01-01T00:00:00Z",
                        "name": "",
                        "interfaces": [
                            {
                                "name": "enp97s0f0np0",
                                "rxBytes": 1000000,
                                "txBytes": 2000000
                            },
                            {
                                "name": "enp129s0f0np0",
                                "rxBytes": 500000,
                                "txBytes": 300000
                            },
                            {
                                "name": "bond0",
                                "rxBytes": 9999999,
                                "txBytes": 9999999
                            },
                            {
                                "name": "vxlan.calico",
                                "rxBytes": 8888888,
                                "txBytes": 8888888
                            },
                            {
                                "name": "cali09618ca5ab3",
                                "rxBytes": 7777777,
                                "txBytes": 7777777
                            }
                        ]
                    }
                }
            ]
        }"#;
        let summary: StatsSummary = serde_json::from_str(json).expect("parse");
        let result = extract_pod_network(&[summary], "default");

        assert_eq!(result.len(), 1);
        // Only enp* interfaces are summed (physical NICs)
        assert_eq!(result.get("gateway-0"), Some(&(1_500_000, 2_300_000)));
    }

    #[test]
    fn extract_pod_network_missing_network() {
        let json = r#"{
            "node": {},
            "pods": [
                {
                    "podRef": {
                        "name": "no-net",
                        "namespace": "default"
                    },
                    "volume": []
                }
            ]
        }"#;
        let summary: StatsSummary = serde_json::from_str(json).expect("parse");
        let result = extract_pod_network(&[summary], "default");

        assert!(result.is_empty());
    }
}

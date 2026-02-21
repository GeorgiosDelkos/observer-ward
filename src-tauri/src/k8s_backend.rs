use std::borrow::Cow;
use std::time::Instant;

use k8s_openapi::api::core::v1::Node;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::api::{Api, ListParams, ObjectMeta};
use kube::core::ClusterResourceScope;
use kube::{Client, Config, Resource};
use serde::{Deserialize, Serialize};

use crate::metrics::{ServerMetrics, ServerStatus};

// -- Custom types for Metrics API (not in k8s-openapi) --

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct NodeMetrics {
    metadata: ObjectMeta,
    usage: NodeMetricsUsage,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct NodeMetricsUsage {
    cpu: Quantity,
    memory: Quantity,
}

impl Resource for NodeMetrics {
    type DynamicType = ();
    type Scope = ClusterResourceScope;

    fn kind(_dt: &()) -> Cow<'_, str> {
        "NodeMetrics".into()
    }
    fn group(_dt: &()) -> Cow<'_, str> {
        "metrics.k8s.io".into()
    }
    fn version(_dt: &()) -> Cow<'_, str> {
        "v1beta1".into()
    }
    fn plural(_dt: &()) -> Cow<'_, str> {
        "nodes".into()
    }
    fn meta(&self) -> &ObjectMeta {
        &self.metadata
    }
    fn meta_mut(&mut self) -> &mut ObjectMeta {
        &mut self.metadata
    }
}

// -- Kubelet stats summary types --

#[derive(Debug, Deserialize)]
struct StatsSummary {
    node: NodeStats,
}

#[derive(Debug, Deserialize)]
struct NodeStats {
    fs: Option<FsStats>,
    network: Option<NetworkStats>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FsStats {
    used_bytes: Option<u64>,
    capacity_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NetworkStats {
    rx_bytes: Option<u64>,
    tx_bytes: Option<u64>,
}

// -- K8s backend --

/// Kubernetes backend that collects cluster-wide metrics by
/// aggregating across all nodes.
pub struct K8sBackend {
    client: Option<Client>,
    kubeconfig: Option<String>,
    context: String,
    prev_net_bytes: Option<(u64, u64)>,
    prev_poll_time: Option<Instant>,
}

impl K8sBackend {
    pub fn new(kubeconfig: Option<String>, context: String) -> Self {
        Self {
            client: None,
            kubeconfig,
            context,
            prev_net_bytes: None,
            prev_poll_time: None,
        }
    }

    /// Build a `kube::Client` from the configured kubeconfig
    /// file and context.
    pub async fn connect(&mut self) -> Result<(), String> {
        let kubeconfig = match &self.kubeconfig {
            Some(path) => kube::config::Kubeconfig::read_from(path)
                .map_err(|e| format!(
                    "failed to read kubeconfig '{path}': {e}"
                ))?,
            None => kube::config::Kubeconfig::read()
                .map_err(|e| format!(
                    "failed to read default kubeconfig: {e}"
                ))?,
        };

        let options = kube::config::KubeConfigOptions {
            context: Some(self.context.clone()),
            ..Default::default()
        };

        let config = Config::from_custom_kubeconfig(
            kubeconfig,
            &options,
        )
        .await
        .map_err(|e| format!(
            "failed to build kube config for context '{}': {e}",
            self.context
        ))?;

        let client = Client::try_from(config)
            .map_err(|e| format!("failed to create kube client: {e}"))?;

        self.client = Some(client);
        Ok(())
    }

    /// Fetch CPU, memory, disk, and network metrics aggregated
    /// across all cluster nodes.
    #[expect(
        clippy::cast_precision_loss,
        reason = "byte/nanosecond sums fit comfortably in f64 \
                  mantissa for percentage and rate calculations"
    )]
    pub async fn collect_metrics(
        &mut self,
        server_name: &str,
    ) -> Result<ServerMetrics, String> {
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| "k8s client not connected".to_string())?
            .clone();

        let (cpu_pct, mem_pct) =
            fetch_cpu_memory(&client).await?;
        let (disk_pct, total_rx, total_tx) =
            fetch_disk_network(&client).await?;

        let now = Instant::now();
        let (rx_per_sec, tx_per_sec) =
            match (self.prev_net_bytes, self.prev_poll_time) {
                (Some((prev_rx, prev_tx)), Some(prev_time)) => {
                    let elapsed =
                        now.duration_since(prev_time).as_secs_f64();
                    if elapsed > 0.0 {
                        let rx_rate =
                            total_rx.saturating_sub(prev_rx) as f64
                                / elapsed;
                        let tx_rate =
                            total_tx.saturating_sub(prev_tx) as f64
                                / elapsed;
                        #[expect(
                            clippy::cast_possible_truncation,
                            clippy::cast_sign_loss,
                            reason = "rates from byte deltas are \
                                      always small positive"
                        )]
                        (rx_rate as u64, tx_rate as u64)
                    } else {
                        (0, 0)
                    }
                }
                _ => (0, 0),
            };

        self.prev_net_bytes = Some((total_rx, total_tx));
        self.prev_poll_time = Some(now);

        Ok(ServerMetrics {
            server_name: server_name.to_string(),
            server_type: "k8s".to_string(),
            status: ServerStatus::Online,
            cpu_percent: cpu_pct,
            memory_percent: mem_pct,
            disk_percent: disk_pct,
            net_rx_bytes_per_sec: rx_per_sec,
            net_tx_bytes_per_sec: tx_per_sec,
        })
    }
}

/// Fetch CPU and memory percentages by comparing metrics API
/// usage against allocatable resources from the Node API.
async fn fetch_cpu_memory(
    client: &Client,
) -> Result<(f64, f64), String> {
    let metrics_api: Api<NodeMetrics> = Api::all(client.clone());
    let node_metrics = metrics_api
        .list(&ListParams::default())
        .await
        .map_err(|e| format!(
            "failed to list node metrics (is metrics-server \
             installed?): {e}"
        ))?;

    let nodes_api: Api<Node> = Api::all(client.clone());
    let nodes = nodes_api
        .list(&ListParams::default())
        .await
        .map_err(|e| format!("failed to list nodes: {e}"))?;

    let mut total_cpu_used = 0.0_f64;
    let mut total_mem_used = 0_u64;

    for nm in &node_metrics {
        total_cpu_used += parse_cpu_quantity(&nm.usage.cpu)?;
        total_mem_used += parse_memory_quantity(&nm.usage.memory)?;
    }

    let mut total_cpu_allocatable = 0.0_f64;
    let mut total_mem_allocatable = 0_u64;

    for node in &nodes {
        let alloc = node
            .status
            .as_ref()
            .and_then(|s| s.allocatable.as_ref())
            .ok_or_else(|| {
                "node missing allocatable resources".to_string()
            })?;

        let cpu_q = alloc.get("cpu").ok_or_else(|| {
            "node allocatable missing 'cpu'".to_string()
        })?;
        let mem_q = alloc.get("memory").ok_or_else(|| {
            "node allocatable missing 'memory'".to_string()
        })?;

        total_cpu_allocatable += parse_cpu_quantity(cpu_q)?;
        total_mem_allocatable += parse_memory_quantity(mem_q)?;
    }

    let cpu_pct = if total_cpu_allocatable > 0.0 {
        total_cpu_used / total_cpu_allocatable * 100.0
    } else {
        0.0
    };

    #[expect(
        clippy::cast_precision_loss,
        reason = "memory byte totals fit comfortably in f64"
    )]
    let mem_pct = if total_mem_allocatable > 0 {
        total_mem_used as f64 / total_mem_allocatable as f64 * 100.0
    } else {
        0.0
    };

    Ok((cpu_pct, mem_pct))
}

/// Fetch disk and network stats from the kubelet stats summary
/// API on each node, aggregating across all nodes.
async fn fetch_disk_network(
    client: &Client,
) -> Result<(f64, u64, u64), String> {
    let nodes_api: Api<Node> = Api::all(client.clone());
    let nodes = nodes_api
        .list(&ListParams::default())
        .await
        .map_err(|e| format!("failed to list nodes: {e}"))?;

    let mut total_disk_used = 0_u64;
    let mut total_disk_capacity = 0_u64;
    let mut total_rx = 0_u64;
    let mut total_tx = 0_u64;

    for node in &nodes {
        let name = node.metadata.name.as_deref().unwrap_or("");
        if name.is_empty() {
            continue;
        }

        let summary = fetch_node_stats(client, name).await?;

        if let Some(fs) = &summary.node.fs {
            total_disk_used +=
                fs.used_bytes.unwrap_or(0);
            total_disk_capacity +=
                fs.capacity_bytes.unwrap_or(0);
        }
        if let Some(net) = &summary.node.network {
            total_rx += net.rx_bytes.unwrap_or(0);
            total_tx += net.tx_bytes.unwrap_or(0);
        }
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "disk byte totals fit comfortably in f64"
    )]
    let disk_pct = if total_disk_capacity > 0 {
        total_disk_used as f64 / total_disk_capacity as f64 * 100.0
    } else {
        0.0
    };

    Ok((disk_pct, total_rx, total_tx))
}

/// Fetch the kubelet stats summary for a single node via the
/// node proxy API.
async fn fetch_node_stats(
    client: &Client,
    node_name: &str,
) -> Result<StatsSummary, String> {
    let url = format!(
        "/api/v1/nodes/{node_name}/proxy/stats/summary"
    );

    let request = http::Request::get(&url)
        .body(Vec::new())
        .map_err(|e| format!(
            "failed to build stats request for node \
             '{node_name}': {e}"
        ))?;

    client
        .request::<StatsSummary>(request)
        .await
        .map_err(|e| format!(
            "failed to fetch stats for node '{node_name}': {e}"
        ))
}

// -- Quantity parsers --

/// Parse a Kubernetes CPU `Quantity` to fractional cores.
///
/// Handles nanocores ("100n"), millicores ("250m"), and whole
/// cores ("2").
fn parse_cpu_quantity(q: &Quantity) -> Result<f64, String> {
    let s = &q.0;
    if let Some(v) = s.strip_suffix('n') {
        v.parse::<f64>()
            .map(|n| n / 1_000_000_000.0)
            .map_err(|e| format!("invalid cpu quantity '{s}': {e}"))
    } else if let Some(v) = s.strip_suffix('m') {
        v.parse::<f64>()
            .map(|n| n / 1000.0)
            .map_err(|e| format!("invalid cpu quantity '{s}': {e}"))
    } else {
        s.parse::<f64>()
            .map_err(|e| format!("invalid cpu quantity '{s}': {e}"))
    }
}

/// Parse a Kubernetes memory `Quantity` to bytes.
///
/// Handles Ki, Mi, Gi suffixes and plain byte values.
fn parse_memory_quantity(q: &Quantity) -> Result<u64, String> {
    let s = &q.0;
    if let Some(v) = s.strip_suffix("Ki") {
        v.parse::<u64>()
            .map(|n| n * 1024)
            .map_err(|e| format!("invalid memory quantity '{s}': {e}"))
    } else if let Some(v) = s.strip_suffix("Mi") {
        v.parse::<u64>()
            .map(|n| n * 1024 * 1024)
            .map_err(|e| format!("invalid memory quantity '{s}': {e}"))
    } else if let Some(v) = s.strip_suffix("Gi") {
        v.parse::<u64>()
            .map(|n| n * 1024 * 1024 * 1024)
            .map_err(|e| format!("invalid memory quantity '{s}': {e}"))
    } else if let Some(v) = s.strip_suffix("Ti") {
        v.parse::<u64>()
            .map(|n| n * 1024 * 1024 * 1024 * 1024)
            .map_err(|e| format!("invalid memory quantity '{s}': {e}"))
    } else {
        s.parse::<u64>()
            .map_err(|e| format!("invalid memory quantity '{s}': {e}"))
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::*;

    fn q(s: &str) -> Quantity {
        Quantity(s.to_string())
    }

    fn assert_f64_near(left: f64, right: f64, epsilon: f64) {
        assert!(
            (left - right).abs() < epsilon,
            "expected ~{right}, got {left}"
        );
    }

    // -- CPU quantity parsing --

    #[test]
    fn cpu_nanocores() {
        let result = parse_cpu_quantity(&q("250000000n")).unwrap();
        assert_f64_near(result, 0.25, 1e-9);
    }

    #[test]
    fn cpu_nanocores_one_core() {
        let result =
            parse_cpu_quantity(&q("1000000000n")).unwrap();
        assert_f64_near(result, 1.0, 1e-9);
    }

    #[test]
    fn cpu_millicores() {
        let result = parse_cpu_quantity(&q("250m")).unwrap();
        assert_f64_near(result, 0.25, 1e-9);
    }

    #[test]
    fn cpu_millicores_full_core() {
        let result = parse_cpu_quantity(&q("1000m")).unwrap();
        assert_f64_near(result, 1.0, 1e-9);
    }

    #[test]
    fn cpu_whole_cores() {
        let result = parse_cpu_quantity(&q("4")).unwrap();
        assert_f64_near(result, 4.0, 1e-9);
    }

    #[test]
    fn cpu_fractional_cores() {
        let result = parse_cpu_quantity(&q("0.5")).unwrap();
        assert_f64_near(result, 0.5, 1e-9);
    }

    #[test]
    fn cpu_invalid_value() {
        let result = parse_cpu_quantity(&q("abcm"));
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("invalid cpu quantity")
        );
    }

    #[test]
    fn cpu_invalid_plain() {
        let result = parse_cpu_quantity(&q("xyz"));
        assert!(result.is_err());
    }

    #[test]
    fn cpu_zero_nanocores() {
        let result = parse_cpu_quantity(&q("0n")).unwrap();
        assert_f64_near(result, 0.0, 1e-9);
    }

    // -- Memory quantity parsing --

    #[test]
    fn memory_kibibytes() {
        let result = parse_memory_quantity(&q("1024Ki")).unwrap();
        assert_eq!(result, 1024 * 1024);
    }

    #[test]
    fn memory_mebibytes() {
        let result = parse_memory_quantity(&q("512Mi")).unwrap();
        assert_eq!(result, 512 * 1024 * 1024);
    }

    #[test]
    fn memory_gibibytes() {
        let result = parse_memory_quantity(&q("8Gi")).unwrap();
        assert_eq!(result, 8 * 1024 * 1024 * 1024);
    }

    #[test]
    fn memory_tebibytes() {
        let result = parse_memory_quantity(&q("1Ti")).unwrap();
        assert_eq!(result, 1024 * 1024 * 1024 * 1024);
    }

    #[test]
    fn memory_plain_bytes() {
        let result = parse_memory_quantity(&q("4096")).unwrap();
        assert_eq!(result, 4096);
    }

    #[test]
    fn memory_zero() {
        let result = parse_memory_quantity(&q("0")).unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn memory_invalid_value() {
        let result = parse_memory_quantity(&q("badMi"));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("invalid memory quantity")
        );
    }

    #[test]
    fn memory_invalid_plain() {
        let result = parse_memory_quantity(&q("notanumber"));
        assert!(result.is_err());
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
        let summary: StatsSummary =
            serde_json::from_str(json).expect("parse stats");

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
        let summary: StatsSummary =
            serde_json::from_str(json).expect("parse stats");

        let fs = summary.node.fs.expect("fs present");
        assert_eq!(fs.used_bytes, None);
        assert_eq!(fs.capacity_bytes, None);
        assert!(summary.node.network.is_none());
    }

    #[test]
    fn parse_stats_summary_no_fs_no_network() {
        let json = r#"{"node": {}}"#;
        let summary: StatsSummary =
            serde_json::from_str(json).expect("parse stats");

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
        let summary: StatsSummary =
            serde_json::from_str(json).expect("parse stats");

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
        let summary: StatsSummary =
            serde_json::from_str(json).expect("parse stats");

        let fs = summary.node.fs.expect("fs present");
        assert_eq!(fs.used_bytes, Some(5_000_000_000));
        assert_eq!(fs.capacity_bytes, None);

        let net = summary.node.network.expect("network present");
        assert_eq!(net.rx_bytes, None);
        assert_eq!(net.tx_bytes, Some(100));
    }

    // -- NodeMetrics JSON parsing --

    #[test]
    fn parse_node_metrics_usage() {
        let json = r#"{
            "metadata": {
                "name": "worker-1"
            },
            "usage": {
                "cpu": "250m",
                "memory": "1024Mi"
            }
        }"#;
        let nm: NodeMetrics =
            serde_json::from_str(json).expect("parse node metrics");

        assert_eq!(
            nm.metadata.name.as_deref(),
            Some("worker-1")
        );
        assert_eq!(nm.usage.cpu.0, "250m");
        assert_eq!(nm.usage.memory.0, "1024Mi");
    }
}

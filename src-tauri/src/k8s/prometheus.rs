//! Node disk, network and PVC figures from a Prometheus server inside the
//! cluster, for accounts that may not read kubelet stats (`nodes/proxy`).
//!
//! Prometheus is reached through the API server's service proxy with the
//! cluster's own client, so it needs `get` on `services/proxy` in
//! Prometheus's namespace and no token of its own. Each poll runs a fixed
//! set of instant queries concurrently; Prometheus computes the rates, over
//! a 5-minute window so they span several scrapes at the common 1-minute
//! interval.
//!
//! The queries assume the usual exporters: node-exporter series labelled
//! with the Kubernetes `node` name, cAdvisor's `container_network_*` with
//! `namespace` and `pod`, and the kubelet's `kubelet_volume_stats_*` with
//! `namespace` and `persistentvolumeclaim`. A series without those labels
//! (a host outside the cluster) is skipped.

use std::collections::HashMap;

use kube::Client;
use serde::Deserialize;

use crate::config::PrometheusService;
use crate::metrics::{Capacity, NetRate};

use super::K8sError;
use super::figures::{HostFigures, NamespaceFigures, PvcUsage};

/// Root filesystems only: container overlays and tmpfs are not the disk.
const DISK_SIZE: &str =
    r#"max by (node) (node_filesystem_size_bytes{mountpoint="/",fstype!~"tmpfs|overlay"})"#;
const DISK_AVAIL: &str =
    r#"max by (node) (node_filesystem_avail_bytes{mountpoint="/",fstype!~"tmpfs|overlay"})"#;

/// Node throughput over the physical uplinks. A bond already carries its
/// member NICs' traffic, so a node with a bond counts only its bonds (`or`
/// keeps the left side per node); other nodes count their `en*`/`eth*`
/// NICs. A NIC outside the bond on a bonded node is not counted.
const NODE_RX: &str = r#"sum by (node) (rate(node_network_receive_bytes_total{device=~"bond.+"}[5m])) or sum by (node) (rate(node_network_receive_bytes_total{device=~"en.+|eth.+"}[5m]))"#;
const NODE_TX: &str = r#"sum by (node) (rate(node_network_transmit_bytes_total{device=~"bond.+"}[5m])) or sum by (node) (rate(node_network_transmit_bytes_total{device=~"en.+|eth.+"}[5m]))"#;

/// Per-namespace queries; `{namespaces}` is replaced by an anchored regex
/// alternation of the watched namespaces.
const POD_RX: &str = r#"sum by (namespace, pod) (rate(container_network_receive_bytes_total{namespace=~"{namespaces}"}[5m]))"#;
const POD_TX: &str = r#"sum by (namespace, pod) (rate(container_network_transmit_bytes_total{namespace=~"{namespaces}"}[5m]))"#;
const PVC_USED: &str = r#"max by (namespace, persistentvolumeclaim) (kubelet_volume_stats_used_bytes{namespace=~"{namespaces}"})"#;
const PVC_CAPACITY: &str = r#"max by (namespace, persistentvolumeclaim) (kubelet_volume_stats_capacity_bytes{namespace=~"{namespaces}"})"#;

/// Disk, network and PVC figures for the nodes and the watched
/// `namespaces`, from the Prometheus at `service`. Runs eight instant
/// queries concurrently over the cluster connection.
///
/// # Errors
///
/// [`K8sError::QueryPrometheus`] when a query cannot be sent or Prometheus
/// rejects it (no `services/proxy` permission, no such service).
pub(super) async fn host_figures(
    client: &Client,
    service: &PrometheusService,
    namespaces: &[String],
) -> Result<HostFigures, K8sError> {
    let watched = namespace_regex(namespaces);
    let per_namespace = |query: &str| query.replace("{namespaces}", &watched);
    let (pod_rx, pod_tx) = (per_namespace(POD_RX), per_namespace(POD_TX));
    let (pvc_used, pvc_capacity) = (per_namespace(PVC_USED), per_namespace(PVC_CAPACITY));

    let (disk_size, disk_avail, node_rx, node_tx, pod_rx, pod_tx, pvc_used, pvc_capacity) = tokio::join!(
        query(client, service, DISK_SIZE),
        query(client, service, DISK_AVAIL),
        query(client, service, NODE_RX),
        query(client, service, NODE_TX),
        query(client, service, &pod_rx),
        query(client, service, &pod_tx),
        query(client, service, &pvc_used),
        query(client, service, &pvc_capacity),
    );

    let mut figures = HostFigures {
        node_disks: disks(
            &by_label(&disk_size?, "node"),
            &by_label(&disk_avail?, "node"),
        ),
        cluster_net: cluster_net(&node_rx?, &node_tx?),
        namespaces: HashMap::new(),
    };
    let (pod_rx, pod_tx) = (pod_rx?, pod_tx?);
    let (pvc_used, pvc_capacity) = (pvc_used?, pvc_capacity?);
    for namespace in namespaces {
        let in_namespace = |samples: &[Sample], key: &str| {
            let samples: Vec<&Sample> = samples
                .iter()
                .filter(|s| s.label("namespace") == Some(namespace.as_str()))
                .collect();
            by_label_refs(&samples, key)
        };
        let rx = in_namespace(&pod_rx, "pod");
        let tx = in_namespace(&pod_tx, "pod");
        let used = in_namespace(&pvc_used, "persistentvolumeclaim");
        let capacity = in_namespace(&pvc_capacity, "persistentvolumeclaim");
        figures.namespaces.insert(
            namespace.clone(),
            NamespaceFigures {
                pod_net: rates(&rx, &tx),
                pvc: PvcUsage::ByClaim(claims(&used, &capacity)),
            },
        );
    }
    Ok(figures)
}

/// An instant query's result: one value per series.
#[derive(Debug, Deserialize)]
struct QueryResponse {
    status: String,
    #[serde(default)]
    data: Option<QueryData>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct QueryData {
    #[serde(default)]
    result: Vec<Sample>,
}

/// One series of an instant vector: its labels and `[timestamp, "value"]`.
#[derive(Debug, Deserialize)]
struct Sample {
    #[serde(default)]
    metric: HashMap<String, String>,
    value: (f64, String),
}

impl Sample {
    fn label(&self, name: &str) -> Option<&str> {
        self.metric.get(name).map(String::as_str)
    }

    /// The value as bytes or bytes per second. `None` for NaN, infinities
    /// and negative values, which Prometheus returns for empty rates.
    fn bytes(&self) -> Option<u64> {
        let value: f64 = self.value.1.parse().ok()?;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "finite and non-negative here, and `as` saturates past u64::MAX"
        )]
        (value.is_finite() && value >= 0.0).then(|| value.round() as u64)
    }
}

/// Run one instant query through the service proxy.
async fn query(
    client: &Client,
    service: &PrometheusService,
    promql: &str,
) -> Result<Vec<Sample>, K8sError> {
    let encoded: String = url::form_urlencoded::byte_serialize(promql.as_bytes()).collect();
    let path = format!(
        "/api/v1/namespaces/{}/services/{}:{}/proxy/api/v1/query?query={encoded}",
        service.namespace, service.service, service.port
    );
    let request = http::Request::get(&path)
        .body(Vec::new())
        .map_err(|source| K8sError::BuildPrometheusRequest { source })?;
    let response: QueryResponse =
        client
            .request(request)
            .await
            .map_err(|source| K8sError::QueryPrometheus {
                service: service.to_string(),
                source: Box::new(source),
            })?;
    if response.status != "success" {
        return Err(K8sError::PrometheusRejected {
            service: service.to_string(),
            message: response.error.unwrap_or(response.status),
        });
    }
    Ok(response.data.map(|data| data.result).unwrap_or_default())
}

/// `a|b` alternation of `namespaces` for a regex label matcher (`=~`),
/// with regex metacharacters escaped. Prometheus anchors such matchers
/// itself.
fn namespace_regex(namespaces: &[String]) -> String {
    let escaped: Vec<String> = namespaces
        .iter()
        .map(|namespace| {
            namespace
                .chars()
                .flat_map(|c| {
                    let special = !c.is_ascii_alphanumeric() && c != '-' && c != '_';
                    special.then_some('\\').into_iter().chain([c])
                })
                .collect()
        })
        .collect();
    escaped.join("|")
}

fn by_label(samples: &[Sample], key: &str) -> HashMap<String, u64> {
    let refs: Vec<&Sample> = samples.iter().collect();
    by_label_refs(&refs, key)
}

/// Each sample's value keyed by its `key` label; samples without the label
/// or with no usable value are skipped.
fn by_label_refs(samples: &[&Sample], key: &str) -> HashMap<String, u64> {
    samples
        .iter()
        .filter_map(|sample| Some((sample.label(key)?.to_string(), sample.bytes()?)))
        .collect()
}

/// Capacity per key from sizes and available bytes; a key needs both.
fn disks(size: &HashMap<String, u64>, avail: &HashMap<String, u64>) -> HashMap<String, Capacity> {
    size.iter()
        .filter_map(|(key, &capacity_bytes)| {
            let avail = *avail.get(key)?;
            Some((
                key.clone(),
                Capacity {
                    used_bytes: capacity_bytes.saturating_sub(avail),
                    capacity_bytes,
                },
            ))
        })
        .collect()
}

/// Capacity per claim from used and capacity bytes; a claim needs both.
fn claims(
    used: &HashMap<String, u64>,
    capacity: &HashMap<String, u64>,
) -> HashMap<String, Capacity> {
    used.iter()
        .filter_map(|(claim, &used_bytes)| {
            Some((
                claim.clone(),
                Capacity {
                    used_bytes,
                    capacity_bytes: *capacity.get(claim)?,
                },
            ))
        })
        .collect()
}

/// Throughput per key; a key needs both directions.
fn rates(rx: &HashMap<String, u64>, tx: &HashMap<String, u64>) -> HashMap<String, NetRate> {
    rx.iter()
        .filter_map(|(key, &rx_bytes_per_sec)| {
            Some((
                key.clone(),
                NetRate {
                    rx_bytes_per_sec,
                    tx_bytes_per_sec: *tx.get(key)?,
                },
            ))
        })
        .collect()
}

/// All nodes' throughput together, or `None` when no node reported both.
fn cluster_net(rx: &[Sample], tx: &[Sample]) -> Option<NetRate> {
    let per_node = rates(&by_label(rx, "node"), &by_label(tx, "node"));
    (!per_node.is_empty()).then(|| {
        per_node
            .values()
            .fold(NetRate::default(), |total, node| NetRate {
                rx_bytes_per_sec: total.rx_bytes_per_sec.saturating_add(node.rx_bytes_per_sec),
                tx_bytes_per_sec: total.tx_bytes_per_sec.saturating_add(node.tx_bytes_per_sec),
            })
    })
}

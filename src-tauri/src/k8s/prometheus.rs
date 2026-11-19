//! Node disk, network and PVC figures from a Prometheus server inside the
//! cluster, for accounts that may not read kubelet stats (`nodes/proxy`).
//!
//! Prometheus is reached through the API server's service proxy with the
//! cluster's own client, so it needs `get` on `services/proxy` in
//! Prometheus's namespace and no token of its own. Each poll runs up to
//! eight instant queries concurrently; Prometheus computes the rates, over
//! a 5-minute window so they span several scrapes at the common 1-minute
//! interval.
//!
//! The queries assume the usual exporters: node-exporter series labelled
//! with the Kubernetes `node` name, cAdvisor's `container_network_*` with
//! `namespace` and `pod`, and the kubelet's `kubelet_volume_stats_*` with
//! `namespace` and `persistentvolumeclaim`. A series without those labels
//! (a host outside the cluster), or for a node the API did not list (one
//! deleted within Prometheus's staleness window), is skipped. Each
//! aggregation takes the `max` of duplicate series first, so a target
//! scraped by two jobs is not counted twice.
//!
//! Figures follow the kubelet's definitions where they differ from the
//! obvious query: disk used is size minus free (root-reserved blocks are
//! not used), and pod network is the pod's `eth0`, as in the kubelet's
//! stats summary. cAdvisor reports a host-network pod with the host's
//! interfaces, so such a pod has no network figure on a node whose
//! interfaces are named otherwise, and the node's own on one whose uplink
//! is `eth0`.

use std::collections::{HashMap, HashSet};

use kube::Client;
use serde::Deserialize;

use crate::config::PrometheusService;
use crate::metrics::{Capacity, NetRate};

use super::K8sError;
use super::figures::{HostFigures, NamespaceFigures, PvcUsage};

/// The root filesystem: `rootfs` is a second, initramfs entry for `/` that
/// node-exporter can report beside the real one; overlays and tmpfs are
/// not the disk. Used is size minus free of the same series, so it never
/// mixes two filesystems' figures.
const DISK_SIZE: &str =
    r#"max by (node) (node_filesystem_size_bytes{mountpoint="/",fstype!~"tmpfs|overlay|rootfs"})"#;
const DISK_USED: &str = r#"max by (node) (node_filesystem_size_bytes{mountpoint="/",fstype!~"tmpfs|overlay|rootfs"} - node_filesystem_free_bytes{mountpoint="/",fstype!~"tmpfs|overlay|rootfs"})"#;

/// Node throughput over the physical uplinks. A bond already carries its
/// member NICs' traffic, so a node whose bonds carry traffic counts only
/// those (`or` keeps the left side per node; `> 0` lets an idle leftover
/// bond fall through to the NICs). Other nodes count their `en*`/`eth*`
/// NICs. VLAN sub-interfaces (`bond0.100`) are excluded: their traffic is
/// already on the parent. A NIC outside the bond on a bonded node is not
/// counted.
const NODE_RX: &str = r#"(sum by (node) (max by (node, device) (rate(node_network_receive_bytes_total{device=~"bond[0-9]+"}[5m]))) > 0) or sum by (node) (max by (node, device) (rate(node_network_receive_bytes_total{device=~"(en|eth)[^.]+"}[5m])))"#;
const NODE_TX: &str = r#"(sum by (node) (max by (node, device) (rate(node_network_transmit_bytes_total{device=~"bond[0-9]+"}[5m]))) > 0) or sum by (node) (max by (node, device) (rate(node_network_transmit_bytes_total{device=~"(en|eth)[^.]+"}[5m])))"#;

/// Per-namespace queries; `{namespaces}` is replaced by [`namespace_regex`]
/// inside a raw (backtick) query string, where a backslash is a regex
/// escape rather than a string escape.
const POD_RX: &str = r#"max by (namespace, pod) (rate(container_network_receive_bytes_total{interface="eth0",namespace=~`{namespaces}`}[5m]))"#;
const POD_TX: &str = r#"max by (namespace, pod) (rate(container_network_transmit_bytes_total{interface="eth0",namespace=~`{namespaces}`}[5m]))"#;
const PVC_USED: &str = r"max by (namespace, persistentvolumeclaim) (kubelet_volume_stats_used_bytes{namespace=~`{namespaces}`})";
const PVC_CAPACITY: &str = r"max by (namespace, persistentvolumeclaim) (kubelet_volume_stats_capacity_bytes{namespace=~`{namespaces}`})";

/// What one poll read from Prometheus: the figures of every query that
/// answered, and the first failure if any did not. A failed query leaves
/// only its own figures out.
#[derive(Debug)]
pub(super) struct Reading {
    pub(super) figures: HostFigures,
    pub(super) failure: Option<K8sError>,
}

/// Disk and network figures of `nodes` and pod figures of the watched
/// `namespaces`, from the Prometheus at `service`. Runs the node queries
/// only when there are nodes and the namespace queries only when there are
/// namespaces, concurrently over the cluster connection.
///
/// A query fails with [`K8sError::QueryPrometheus`] when it cannot be sent
/// or Prometheus answers with an error (no `services/proxy` permission, no
/// such service, a query it cannot parse), and with
/// [`K8sError::BuildPrometheusRequest`] when the request cannot be built.
pub(super) async fn host_figures(
    client: &Client,
    service: &PrometheusService,
    nodes: &[String],
    namespaces: &[String],
) -> Reading {
    let watched = namespace_regex(namespaces);
    let per_namespace = |query: &str| query.replace("{namespaces}", &watched);
    let (pod_rx, pod_tx) = (per_namespace(POD_RX), per_namespace(POD_TX));
    let (pvc_used, pvc_capacity) = (per_namespace(PVC_USED), per_namespace(PVC_CAPACITY));
    let node_query = |promql| query_if(!nodes.is_empty(), client, service, promql);
    let namespace_query = |promql| query_if(!watched.is_empty(), client, service, promql);

    let (disk_size, disk_used, node_rx, node_tx, pod_rx, pod_tx, pvc_used, pvc_capacity) = tokio::join!(
        node_query(DISK_SIZE),
        node_query(DISK_USED),
        node_query(NODE_RX),
        node_query(NODE_TX),
        namespace_query(&pod_rx),
        namespace_query(&pod_tx),
        namespace_query(&pvc_used),
        namespace_query(&pvc_capacity),
    );

    let mut failure = None;
    let mut answered = |result: Result<Vec<Sample>, K8sError>| {
        result.unwrap_or_else(|e| {
            failure.get_or_insert(e);
            Vec::new()
        })
    };
    let listed: HashSet<&str> = nodes.iter().map(String::as_str).collect();
    let per_node = |samples: &[Sample]| {
        let listed_samples = samples
            .iter()
            .filter(|s| s.label("node").is_some_and(|node| listed.contains(node)));
        by_label(listed_samples, "node")
    };
    let node_disks = capacities(
        &per_node(&answered(disk_used)),
        &per_node(&answered(disk_size)),
    );
    let node_net = rates(&per_node(&answered(node_rx)), &per_node(&answered(node_tx)));
    let (pod_rx, pod_tx) = (answered(pod_rx), answered(pod_tx));
    let (pvc_used, pvc_capacity) = (answered(pvc_used), answered(pvc_capacity));

    let mut figures = HostFigures {
        node_disks,
        cluster_net: total(&node_net),
        namespaces: HashMap::new(),
    };
    for namespace in namespaces {
        let in_namespace = |samples: &[Sample], key: &str| {
            let here = samples
                .iter()
                .filter(|s| s.label("namespace") == Some(namespace.as_str()));
            by_label(here, key)
        };
        let pod_net = rates(&in_namespace(&pod_rx, "pod"), &in_namespace(&pod_tx, "pod"));
        let claims = capacities(
            &in_namespace(&pvc_used, "persistentvolumeclaim"),
            &in_namespace(&pvc_capacity, "persistentvolumeclaim"),
        );
        figures.namespaces.insert(
            namespace.clone(),
            NamespaceFigures {
                pod_net,
                pvc: PvcUsage::ByClaim(claims),
            },
        );
    }
    Reading { figures, failure }
}

/// An instant query's result: one value per series. Prometheus reports a
/// failed query with an error status, which kube turns into an error before
/// this is parsed.
#[derive(Debug, Deserialize)]
struct QueryResponse {
    data: QueryData,
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

/// Run `promql` when `wanted`; otherwise answer with no series.
async fn query_if(
    wanted: bool,
    client: &Client,
    service: &PrometheusService,
    promql: &str,
) -> Result<Vec<Sample>, K8sError> {
    if !wanted {
        return Ok(Vec::new());
    }
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
    Ok(response.data.result)
}

/// `a|b` alternation of `namespaces` for a regex label matcher inside a raw
/// (backtick) query string, with regex metacharacters backslash-escaped.
/// Prometheus anchors such matchers itself. A name containing a backtick
/// cannot be written in a raw string and is left out; Kubernetes names
/// never contain one.
fn namespace_regex(namespaces: &[String]) -> String {
    let escaped: Vec<String> = namespaces
        .iter()
        .filter(|namespace| !namespace.contains('`'))
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

/// Each sample's value keyed by its `key` label; samples without the label
/// or with no usable value are skipped.
fn by_label<'s>(samples: impl Iterator<Item = &'s Sample>, key: &str) -> HashMap<String, u64> {
    samples
        .filter_map(|sample| Some((sample.label(key)?.to_string(), sample.bytes()?)))
        .collect()
}

/// Capacity per key from used and capacity bytes; a key needs both.
fn capacities(
    used: &HashMap<String, u64>,
    capacity: &HashMap<String, u64>,
) -> HashMap<String, Capacity> {
    used.iter()
        .filter_map(|(key, &used_bytes)| {
            Some((
                key.clone(),
                Capacity {
                    used_bytes,
                    capacity_bytes: *capacity.get(key)?,
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

/// All nodes' throughput together, or `None` when no node reported both
/// directions.
fn total(per_node: &HashMap<String, NetRate>) -> Option<NetRate> {
    (!per_node.is_empty()).then(|| {
        per_node
            .values()
            .fold(NetRate::default(), |total, node| NetRate {
                rx_bytes_per_sec: total.rx_bytes_per_sec.saturating_add(node.rx_bytes_per_sec),
                tx_bytes_per_sec: total.tx_bytes_per_sec.saturating_add(node.tx_bytes_per_sec),
            })
    })
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::{
        DISK_SIZE, DISK_USED, NODE_RX, NODE_TX, POD_RX, POD_TX, PVC_CAPACITY, PVC_USED,
        namespace_regex, query_if,
    };

    /// `<kubeconfig>|<context>|<namespace/service:port>` of a cluster with a
    /// Prometheus, for [`every_query_runs_on_a_real_prometheus`].
    const REAL_PROMETHEUS: &str = "OBSERVER_WARD_TEST_PROMETHEUS";

    /// The stub servers answer by metric name and never parse a query, so
    /// only a real Prometheus catches a query it would reject (a string
    /// escape it does not accept, an operator in the wrong place). Skipped
    /// unless [`REAL_PROMETHEUS`] is set: CI has no cluster.
    #[tokio::test]
    async fn every_query_runs_on_a_real_prometheus() {
        let Ok(target) = std::env::var(REAL_PROMETHEUS) else {
            return;
        };
        let mut parts = target.splitn(3, '|');
        let (Some(kubeconfig), Some(context), Some(service)) =
            (parts.next(), parts.next(), parts.next())
        else {
            unreachable!("{REAL_PROMETHEUS} must be <kubeconfig>|<context>|<service>");
        };
        let client = crate::k8s::connect(Some(kubeconfig.into()), context)
            .await
            .expect("client builds");
        let service = service.parse().expect("a valid service");
        // Includes a name the escaping has to handle.
        let regex = namespace_regex(&["default".to_string(), "app.prod".to_string()]);

        let queries = [
            DISK_SIZE,
            DISK_USED,
            NODE_RX,
            NODE_TX,
            POD_RX,
            POD_TX,
            PVC_USED,
            PVC_CAPACITY,
        ];
        for promql in queries {
            let promql = promql.replace("{namespaces}", &regex);
            let result = query_if(true, &client, &service, &promql).await;
            assert!(
                result.is_ok(),
                "{promql}: {}",
                result.map_or_else(|e| crate::error::error_chain(&e), |_| String::new())
            );
        }
    }

    #[test]
    fn namespace_regex_escapes_for_a_raw_promql_string() {
        let names = ["default", "app.prod", "we`ird"].map(String::from);

        assert_eq!(namespace_regex(&names), r"default|app\.prod");
        assert_eq!(
            namespace_regex(&[]),
            "",
            "no namespaces, no namespace queries"
        );
    }
}

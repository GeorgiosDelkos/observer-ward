//! Kubernetes Metrics API + kubelet stats collection.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, ListParams, ObjectList};
use kube::{Client, Config};

use crate::metrics::{ServerMetrics, ServerStatus};

mod error;
mod events;
mod metrics_api;
mod pods;
mod quantity;
mod stats;

pub(crate) use error::K8sError;

use events::fetch_pod_events;
use metrics_api::{NodeMetrics, PodMetrics};
use pods::{
    PodMetricCtx, apply_pod_net_rates, build_pod_server_metrics, derive_pod_status,
    pod_restart_count, pod_start_time,
};
use quantity::{parse_cpu_quantity, parse_memory_quantity};
use stats::{
    StatsSummary, compute_cluster_disk_net, extract_pod_network, extract_pod_pvc,
    fetch_all_node_stats,
};

const KUBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const KUBE_READ_TIMEOUT: Duration = Duration::from_secs(20);

/// Kubernetes backend that collects cluster-wide metrics by
/// aggregating across all nodes.
pub(crate) struct K8sBackend {
    client: Option<Client>,
    kubeconfig: Option<String>,
    context: String,
    prev_net_bytes: Option<(u64, u64)>,
    prev_poll_time: Option<Instant>,
    prev_pod_net: HashMap<String, (u64, u64)>,
    prev_pod_poll_time: Option<Instant>,
}

impl K8sBackend {
    pub(crate) fn new(kubeconfig: Option<String>, context: String) -> Self {
        Self {
            client: None,
            kubeconfig,
            context,
            prev_net_bytes: None,
            prev_poll_time: None,
            prev_pod_net: HashMap::new(),
            prev_pod_poll_time: None,
        }
    }

    fn is_connected(&self) -> bool {
        self.client.is_some()
    }

    fn client(&self) -> Option<Client> {
        self.client.clone()
    }

    pub(crate) fn matches_config(&self, kubeconfig: Option<&String>, context: &str) -> bool {
        self.kubeconfig.as_ref() == kubeconfig && self.context == context
    }

    /// Build a `kube::Client` from the configured kubeconfig
    /// file and context.
    async fn connect(&mut self) -> Result<(), K8sError> {
        let kubeconfig_path = self
            .kubeconfig
            .clone()
            .map(|p| crate::config::expand_tilde(&p));
        let kubeconfig = tokio::task::spawn_blocking(move || match kubeconfig_path {
            Some(path) => kube::config::Kubeconfig::read_from(&path).map_err(|source| {
                K8sError::ReadKubeconfig {
                    path,
                    source: Box::new(source),
                }
            }),
            None => kube::config::Kubeconfig::read()
                .map_err(|source| K8sError::ReadDefaultKubeconfig(Box::new(source))),
        })
        .await
        .map_err(|_| K8sError::KubeconfigTask)??;

        let options = kube::config::KubeConfigOptions {
            context: Some(self.context.clone()),
            ..Default::default()
        };

        let mut config = Config::from_custom_kubeconfig(kubeconfig, &options)
            .await
            .map_err(|source| K8sError::BuildConfig {
                context: self.context.clone(),
                source: Box::new(source),
            })?;
        config.connect_timeout = Some(KUBE_CONNECT_TIMEOUT);
        config.read_timeout = Some(KUBE_READ_TIMEOUT);

        let client =
            Client::try_from(config).map_err(|source| K8sError::CreateClient(Box::new(source)))?;

        self.client = Some(client);
        Ok(())
    }

    /// Fetch CPU, memory, disk, and network metrics aggregated
    /// across all cluster nodes. Allocatable totals are passed
    /// in to avoid redundant API calls.
    #[expect(
        clippy::cast_precision_loss,
        reason = "byte/nanosecond sums fit comfortably in f64 \
                  mantissa for percentage and rate calculations"
    )]
    fn cluster_metrics_from_usage(
        &mut self,
        data: &ClusterPollData<'_>,
        collected_pods: bool,
    ) -> ServerMetrics {
        let cpu_pct = if data.alloc_cpu > 0.0 {
            data.cpu_used / data.alloc_cpu * 100.0
        } else {
            0.0
        };
        #[expect(
            clippy::cast_precision_loss,
            reason = "memory byte totals fit comfortably in f64"
        )]
        let mem_pct = if data.alloc_mem > 0 {
            data.mem_used as f64 / data.alloc_mem as f64 * 100.0
        } else {
            0.0
        };

        let (disk_pct, total_rx, total_tx, disk_used, disk_capacity) =
            compute_cluster_disk_net(data.stats);

        let now = Instant::now();
        let (rx_per_sec, tx_per_sec) = match (self.prev_net_bytes, self.prev_poll_time) {
            (Some((prev_rx, prev_tx)), Some(prev_time)) => {
                let elapsed = now.duration_since(prev_time).as_secs_f64();
                if elapsed > 0.0 {
                    let rx_rate = total_rx.saturating_sub(prev_rx) as f64 / elapsed;
                    let tx_rate = total_tx.saturating_sub(prev_tx) as f64 / elapsed;
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
            (Some(_) | None, None) | (None, Some(_)) => (0, 0),
        };

        self.prev_net_bytes = Some((total_rx, total_tx));
        self.prev_poll_time = Some(now);

        #[expect(
            clippy::cast_possible_truncation,
            reason = "cluster node count is always small"
        )]
        let node_count = data.stats.len() as u32;

        ServerMetrics {
            server_name: data.server_name.to_string(),
            server_type: "k8s".to_string(),
            status: ServerStatus::Online,
            cpu_percent: cpu_pct,
            memory_percent: mem_pct,
            disk_percent: disk_pct,
            net_rx_bytes_per_sec: rx_per_sec,
            net_tx_bytes_per_sec: tx_per_sec,
            cpu_millicores: data.cpu_used * 1000.0,
            memory_bytes: data.mem_used,
            disk_used_bytes: disk_used,
            disk_capacity_bytes: disk_capacity,
            node_count,
            collected_pods,
            ..ServerMetrics::default()
        }
    }

    /// Build per-pod `ServerMetrics` from already-fetched lists.
    ///
    /// Pods present in the spec list but missing from metrics-server
    /// (Pending, `CrashLoopBackOff` with no samples) still get a card so they
    /// are not invisible. Succeeded pods are skipped.
    fn assemble_pod_metrics(
        &mut self,
        data: &ClusterPollData<'_>,
        pod_metrics_list: &ObjectList<PodMetrics>,
        pod_specs: &ObjectList<Pod>,
    ) -> Vec<ServerMetrics> {
        let pvc_map = extract_pod_pvc(data.stats, data.namespace);
        let net_map = extract_pod_network(data.stats, data.namespace);

        let pod_index: HashMap<&str, &Pod> = pod_specs
            .items
            .iter()
            .filter_map(|p| p.metadata.name.as_deref().map(|n| (n, p)))
            .collect();

        let mut results = Vec::with_capacity(pod_metrics_list.items.len() + pod_specs.items.len());
        let mut seen = HashSet::new();

        for pm in pod_metrics_list {
            match build_pod_server_metrics(
                pm,
                &pod_index,
                &PodMetricCtx {
                    cluster_name: data.server_name,
                    cluster_cpu: data.alloc_cpu,
                    cluster_mem: data.alloc_mem,
                    pvc_map: &pvc_map,
                    events: data.events,
                },
            ) {
                Ok(m) => {
                    if let Some(name) = pm.metadata.name.as_deref() {
                        seen.insert(name.to_string());
                    }
                    results.push(m);
                }
                Err(e) => {
                    let name = pm.metadata.name.as_deref().unwrap_or("unknown");
                    tracing::warn!("skipping pod {name}: {}", crate::error::error_chain(&e));
                }
            }
        }

        for pod in &pod_specs.items {
            let Some(name) = pod.metadata.name.as_deref() else {
                continue;
            };
            if seen.contains(name) {
                continue;
            }
            let status = derive_pod_status(Some(pod));
            if status == "Succeeded" {
                continue;
            }
            results.push(ServerMetrics {
                server_name: format!("{}/{name}", data.server_name),
                server_type: "pod".to_string(),
                status: ServerStatus::Online,
                restart_count: pod_restart_count(Some(pod)),
                start_time: pod_start_time(Some(pod)),
                pod_status: status,
                last_event: data.events.get(name).cloned().unwrap_or_default(),
                ..ServerMetrics::default()
            });
        }

        apply_pod_net_rates(
            &mut results,
            &net_map,
            &self.prev_pod_net,
            self.prev_pod_poll_time,
        );
        self.prev_pod_net = net_map;
        self.prev_pod_poll_time = Some(Instant::now());

        results
    }

    /// Collect all metrics for a K8s cluster: node-level
    /// aggregates plus per-pod metrics. Fetches allocatable
    /// totals once and reuses them for both calculations.
    ///
    /// Resets the internal client on connection-level failures
    /// so the next poll attempt creates a fresh connection.
    pub(crate) async fn collect_all(
        &mut self,
        server_name: &str,
        namespace: &str,
    ) -> Result<Vec<ServerMetrics>, K8sError> {
        if !self.is_connected() {
            self.connect().await?;
        }

        let client = self.client().ok_or(K8sError::NotConnected)?;

        let nodes = match fetch_nodes(&client).await {
            Ok(n) => n,
            Err(e) => {
                self.client = None;
                return Err(e);
            }
        };

        let (alloc_cpu, alloc_mem) = match sum_allocatable(&nodes) {
            Ok(totals) => totals,
            Err(e) => {
                self.client = None;
                return Err(e);
            }
        };

        let (stats, usage_result, events_result, pod_lists_result) = tokio::join!(
            fetch_all_node_stats(&client, &nodes),
            fetch_cpu_mem_usage(&client),
            fetch_pod_events(&client, namespace),
            fetch_pod_metrics_and_specs(&client, namespace),
        );

        let (cpu_used, mem_used) = match usage_result {
            Ok(usage) => usage,
            Err(e) => {
                self.client = None;
                return Err(e);
            }
        };

        let events = events_result.unwrap_or_else(|e| {
            tracing::warn!(
                "failed to fetch pod events for {server_name}/{namespace}: {}",
                crate::error::error_chain(&e)
            );
            HashMap::new()
        });

        let poll_data = ClusterPollData {
            server_name,
            namespace,
            stats: &stats,
            events: &events,
            alloc_cpu,
            alloc_mem,
            cpu_used,
            mem_used,
        };

        let (collected_pods, pod_metrics) = match pod_lists_result {
            Ok((pod_metrics_list, pod_specs)) => (
                true,
                self.assemble_pod_metrics(&poll_data, &pod_metrics_list, &pod_specs),
            ),
            Err(e) => {
                tracing::warn!(
                    "failed to collect pod metrics for {server_name}/{namespace}: {}",
                    crate::error::error_chain(&e)
                );
                (false, Vec::new())
            }
        };

        let node_metrics = self.cluster_metrics_from_usage(&poll_data, collected_pods);

        let mut results = Vec::with_capacity(1 + pod_metrics.len());
        results.push(node_metrics);
        results.extend(pod_metrics);
        Ok(results)
    }
}

struct ClusterPollData<'a> {
    server_name: &'a str,
    namespace: &'a str,
    stats: &'a [StatsSummary],
    events: &'a HashMap<String, String>,
    alloc_cpu: f64,
    alloc_mem: u64,
    cpu_used: f64,
    mem_used: u64,
}

async fn fetch_pod_metrics_and_specs(
    client: &Client,
    namespace: &str,
) -> Result<(ObjectList<PodMetrics>, ObjectList<Pod>), K8sError> {
    let metrics_api: Api<PodMetrics> = Api::namespaced(client.clone(), namespace);
    let pods_api: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let params = ListParams::default();
    let (pod_metrics_result, pod_specs_result) =
        tokio::join!(metrics_api.list(&params), pods_api.list(&params),);
    let pod_metrics_list = pod_metrics_result.map_err(|source| K8sError::ListPodMetrics {
        namespace: namespace.to_string(),
        source: Box::new(source),
    })?;
    let pod_specs = pod_specs_result.map_err(|source| K8sError::ListPods {
        namespace: namespace.to_string(),
        source: Box::new(source),
    })?;
    Ok((pod_metrics_list, pod_specs))
}

/// Fetch all cluster nodes from the API.
async fn fetch_nodes(client: &Client) -> Result<Vec<Node>, K8sError> {
    let nodes_api: Api<Node> = Api::all(client.clone());
    let nodes = nodes_api
        .list(&ListParams::default())
        .await
        .map_err(|source| K8sError::ListNodes(Box::new(source)))?;
    Ok(nodes.items)
}

/// Sum allocatable CPU (fractional cores) and memory (bytes)
/// from the provided node list.
fn sum_allocatable(nodes: &[Node]) -> Result<(f64, u64), K8sError> {
    let mut total_cpu = 0.0_f64;
    let mut total_mem = 0_u64;

    for node in nodes {
        let Some(alloc) = node.status.as_ref().and_then(|s| s.allocatable.as_ref()) else {
            let name = node.metadata.name.as_deref().unwrap_or("unknown");
            tracing::warn!("node {name} missing allocatable resources, skipping");
            continue;
        };

        let Some(cpu_q) = alloc.get("cpu") else {
            continue;
        };
        let Some(mem_q) = alloc.get("memory") else {
            continue;
        };

        total_cpu += parse_cpu_quantity(cpu_q)?;
        total_mem = total_mem.saturating_add(parse_memory_quantity(mem_q)?);
    }

    Ok((total_cpu, total_mem))
}

/// Fetch total CPU (fractional cores) and memory (bytes)
/// currently used across all nodes from the Metrics API.
async fn fetch_cpu_mem_usage(client: &Client) -> Result<(f64, u64), K8sError> {
    let metrics_api: Api<NodeMetrics> = Api::all(client.clone());
    let node_metrics = metrics_api
        .list(&ListParams::default())
        .await
        .map_err(|source| K8sError::ListNodeMetrics(Box::new(source)))?;

    let mut total_cpu = 0.0_f64;
    let mut total_mem = 0_u64;

    for nm in &node_metrics {
        total_cpu += parse_cpu_quantity(&nm.usage.cpu)?;
        total_mem = total_mem.saturating_add(parse_memory_quantity(&nm.usage.memory)?);
    }

    Ok((total_cpu, total_mem))
}

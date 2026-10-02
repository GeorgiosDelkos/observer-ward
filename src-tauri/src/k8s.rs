//! Kubernetes Metrics API + kubelet stats collection.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::{Node, Pod};
use kube::Client;
use kube::api::{Api, ListParams};

use crate::config::K8sTarget;
use crate::metrics::{ClusterMetrics, NetSample, PodMetrics, Usage, percent};

mod error;
mod events;
mod kubeconfig;
mod metrics_api;
mod pods;
mod quantity;
mod stats;

pub(crate) use error::K8sError;
pub(crate) use kubeconfig::{KubeconfigSummary, inspect, validate_server};

use events::fetch_pod_events;
use metrics_api::{NodeMetrics, PodMetrics as PodMetricsSample};
use pods::{PodContext, assemble_pods};
use quantity::{parse_cpu_quantity, parse_memory_quantity};
use stats::{
    StatsSummary, cluster_totals, extract_pod_network, extract_pod_pvc, fetch_all_node_stats,
};

const KUBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const KUBE_READ_TIMEOUT: Duration = Duration::from_secs(20);

/// CPU (fractional cores) and memory (bytes).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct CpuMem {
    cpu: f64,
    mem: u64,
}

impl CpuMem {
    fn add(&mut self, cpu: f64, mem: u64) {
        self.cpu += cpu;
        self.mem = self.mem.saturating_add(mem);
    }
}

/// Kubernetes backend that collects cluster-wide metrics by aggregating
/// across all nodes, plus one card per pod in the configured namespace.
pub(crate) struct K8sBackend {
    target: K8sTarget,
    client: Option<Client>,
    prev_net: Option<NetSample>,
    prev_pod_net: HashMap<String, NetSample>,
}

impl K8sBackend {
    pub(crate) fn new(target: K8sTarget) -> Self {
        Self {
            target,
            client: None,
            prev_net: None,
            prev_pod_net: HashMap::new(),
        }
    }

    pub(crate) fn target(&self) -> &K8sTarget {
        &self.target
    }

    /// Collect node aggregates and pod cards. Any failure drops the client
    /// so the next poll rebuilds it from the kubeconfig (picking up
    /// refreshed credentials).
    ///
    /// # Errors
    ///
    /// [`K8sError`] if the client cannot be built or a required API call
    /// (nodes, node metrics) fails. Pod and event listing failures only
    /// degrade the result.
    pub(crate) async fn collect(&mut self) -> Result<ClusterMetrics, K8sError> {
        let client = match &self.client {
            Some(client) => client.clone(),
            None => self.connect().await?,
        };
        let result = self.collect_with(&client).await;
        self.client = result.is_ok().then_some(client);
        result
    }

    /// Build a `kube::Client` from the configured kubeconfig and context.
    async fn connect(&self) -> Result<Client, K8sError> {
        let path = self.target.kubeconfig.clone();
        let kubeconfig = tokio::task::spawn_blocking(move || kubeconfig::load(path.as_deref()))
            .await
            .map_err(K8sError::KubeconfigTask)??;

        let mut config = kubeconfig::build_config(kubeconfig, &self.target.context).await?;
        config.connect_timeout = Some(KUBE_CONNECT_TIMEOUT);
        config.read_timeout = Some(KUBE_READ_TIMEOUT);

        Client::try_from(config).map_err(|source| K8sError::CreateClient(Box::new(source)))
    }

    async fn collect_with(&mut self, client: &Client) -> Result<ClusterMetrics, K8sError> {
        let namespace = self.target.namespace.as_str();
        let nodes = fetch_nodes(client).await?;
        let allocatable = sum_allocatable(&nodes)?;

        let (stats, usage, events, pod_lists) = tokio::join!(
            fetch_all_node_stats(client, &nodes),
            fetch_cpu_mem_usage(client),
            fetch_pod_events(client, namespace),
            fetch_pod_lists(client, namespace),
        );
        let used = usage?;
        let events = events.unwrap_or_else(|e| {
            tracing::warn!(
                "failed to fetch pod events for {}/{namespace}: {}",
                self.target.name,
                crate::error::error_chain(&e)
            );
            HashMap::new()
        });

        let pods = match pod_lists {
            Ok((samples, specs)) => {
                Some(self.pod_cards(&samples, &specs, &stats, &events, allocatable))
            }
            Err(e) => {
                tracing::warn!(
                    "failed to collect pod metrics for {}/{namespace}: {}",
                    self.target.name,
                    crate::error::error_chain(&e)
                );
                None
            }
        };

        Ok(self.cluster_metrics(&stats, allocatable, used, pods))
    }

    fn cluster_metrics(
        &mut self,
        stats: &[StatsSummary],
        allocatable: CpuMem,
        used: CpuMem,
        pods: Option<Vec<PodMetrics>>,
    ) -> ClusterMetrics {
        let totals = cluster_totals(stats);
        let now = NetSample {
            rx_bytes: totals.rx_bytes,
            tx_bytes: totals.tx_bytes,
            at: Instant::now(),
        };
        let net = self.prev_net.and_then(|prev| now.rate_since(&prev));
        self.prev_net = Some(now);

        #[expect(
            clippy::cast_precision_loss,
            reason = "byte counts fit comfortably in f64 for a percentage"
        )]
        let memory_percent = percent(used.mem as f64, allocatable.mem as f64);

        ClusterMetrics {
            usage: Usage {
                cpu_percent: percent(used.cpu, allocatable.cpu),
                memory_percent,
                disk_percent: totals.disk.percent(),
            },
            net,
            cpu_millicores: used.cpu * 1000.0,
            memory_bytes: used.mem,
            disk: totals.disk,
            node_count: stats.len(),
            pods,
        }
    }

    fn pod_cards(
        &mut self,
        samples: &[PodMetricsSample],
        specs: &[Pod],
        stats: &[StatsSummary],
        events: &HashMap<String, String>,
        allocatable: CpuMem,
    ) -> Vec<PodMetrics> {
        let namespace = self.target.namespace.as_str();
        let pvc = extract_pod_pvc(stats, namespace);
        let net = extract_pod_network(stats, namespace, Instant::now());

        let pods = assemble_pods(
            samples,
            specs,
            &PodContext {
                cluster_cpu: allocatable.cpu,
                cluster_mem: allocatable.mem,
                pvc: &pvc,
                net: &net,
                prev_net: &self.prev_pod_net,
                events,
            },
        );
        self.prev_pod_net = net;
        pods
    }
}

async fn fetch_pod_lists(
    client: &Client,
    namespace: &str,
) -> Result<(Vec<PodMetricsSample>, Vec<Pod>), K8sError> {
    let metrics_api: Api<PodMetricsSample> = Api::namespaced(client.clone(), namespace);
    let pods_api: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let params = ListParams::default();
    let (samples, specs) = tokio::join!(metrics_api.list(&params), pods_api.list(&params));

    let samples = samples.map_err(|source| K8sError::ListPodMetrics {
        namespace: namespace.to_string(),
        source: Box::new(source),
    })?;
    let specs = specs.map_err(|source| K8sError::ListPods {
        namespace: namespace.to_string(),
        source: Box::new(source),
    })?;
    Ok((samples.items, specs.items))
}

async fn fetch_nodes(client: &Client) -> Result<Vec<Node>, K8sError> {
    let nodes_api: Api<Node> = Api::all(client.clone());
    let nodes = nodes_api
        .list(&ListParams::default())
        .await
        .map_err(|source| K8sError::ListNodes(Box::new(source)))?;
    Ok(nodes.items)
}

/// Sum allocatable CPU and memory over the nodes that report both.
fn sum_allocatable(nodes: &[Node]) -> Result<CpuMem, K8sError> {
    let mut total = CpuMem::default();

    for node in nodes {
        let name = node.metadata.name.as_deref().unwrap_or("unknown");
        let allocatable = node.status.as_ref().and_then(|s| s.allocatable.as_ref());
        let Some((cpu, mem)) = allocatable.and_then(|a| a.get("cpu").zip(a.get("memory"))) else {
            tracing::warn!("node {name} reports no allocatable cpu/memory, skipping");
            continue;
        };
        total.add(parse_cpu_quantity(cpu)?, parse_memory_quantity(mem)?);
    }

    Ok(total)
}

/// Total CPU and memory currently used across all nodes, from the
/// Metrics API.
async fn fetch_cpu_mem_usage(client: &Client) -> Result<CpuMem, K8sError> {
    let metrics_api: Api<NodeMetrics> = Api::all(client.clone());
    let node_metrics = metrics_api
        .list(&ListParams::default())
        .await
        .map_err(|source| K8sError::ListNodeMetrics(Box::new(source)))?;

    let mut total = CpuMem::default();
    for nm in &node_metrics {
        total.add(
            parse_cpu_quantity(&nm.usage.cpu)?,
            parse_memory_quantity(&nm.usage.memory)?,
        );
    }
    Ok(total)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use std::collections::BTreeMap;

    use k8s_openapi::api::core::v1::NodeStatus;
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
    use kube::api::ObjectMeta;

    use super::*;

    fn node(name: &str, cpu: Option<&str>, memory: Option<&str>) -> Node {
        let mut allocatable = BTreeMap::new();
        if let Some(cpu) = cpu {
            allocatable.insert("cpu".to_string(), Quantity(cpu.to_string()));
        }
        if let Some(memory) = memory {
            allocatable.insert("memory".to_string(), Quantity(memory.to_string()));
        }
        Node {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                ..ObjectMeta::default()
            },
            status: Some(NodeStatus {
                allocatable: Some(allocatable),
                ..NodeStatus::default()
            }),
            ..Node::default()
        }
    }

    #[test]
    fn allocatable_sums_complete_nodes_only() {
        let nodes = [
            node("a", Some("3920m"), Some("16Gi")),
            node("b", Some("2"), Some("8Gi")),
            node("c", Some("4"), None),
            Node::default(),
        ];

        let total = sum_allocatable(&nodes).expect("sum");

        assert!((total.cpu - 5.92).abs() < 1e-9);
        assert_eq!(total.mem, 24 * 1024 * 1024 * 1024);
    }

    #[test]
    fn allocatable_rejects_malformed_quantities() {
        assert!(sum_allocatable(&[node("a", Some("lots"), Some("1Gi"))]).is_err());
    }
}

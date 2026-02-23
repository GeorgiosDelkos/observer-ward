use std::borrow::Cow;
use std::collections::HashMap;
use std::time::Instant;

use k8s_openapi::api::core::v1::{Event, Node, Pod};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::api::{Api, ListParams, ObjectMeta};
use kube::core::{ClusterResourceScope, NamespaceResourceScope};
use kube::{Client, Config, Resource};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

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

// -- Pod metrics API types --

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PodMetrics {
    metadata: ObjectMeta,
    containers: Vec<ContainerMetrics>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct ContainerMetrics {
    usage: ContainerMetricsUsage,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct ContainerMetricsUsage {
    cpu: Quantity,
    memory: Quantity,
}

impl Resource for PodMetrics {
    type DynamicType = ();
    type Scope = NamespaceResourceScope;

    fn kind(_dt: &()) -> Cow<'_, str> {
        "PodMetrics".into()
    }
    fn group(_dt: &()) -> Cow<'_, str> {
        "metrics.k8s.io".into()
    }
    fn version(_dt: &()) -> Cow<'_, str> {
        "v1beta1".into()
    }
    fn plural(_dt: &()) -> Cow<'_, str> {
        "pods".into()
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
pub(crate) struct StatsSummary {
    node: NodeStats,
    #[serde(default)]
    pods: Vec<PodStatsSummary>,
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
    #[serde(default)]
    interfaces: Vec<InterfaceStats>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InterfaceStats {
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
                rx += iface.rx_bytes.unwrap_or(0);
                tx += iface.tx_bytes.unwrap_or(0);
            }
        }
        (rx, tx)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PodStatsSummary {
    pod_ref: PodRef,
    #[serde(default)]
    volume: Vec<VolumeStats>,
    network: Option<NetworkStats>,
}

#[derive(Debug, Deserialize)]
struct PodRef {
    name: String,
    namespace: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VolumeStats {
    used_bytes: Option<u64>,
    capacity_bytes: Option<u64>,
    pvc_ref: Option<PvcRef>,
}

#[derive(Debug, Deserialize)]
struct PvcRef {
    #[serde(rename = "name")]
    _name: String,
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
    prev_pod_net: HashMap<String, (u64, u64)>,
    prev_pod_poll_time: Option<Instant>,
}

impl K8sBackend {
    pub fn new(kubeconfig: Option<String>, context: String) -> Self {
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

    pub fn is_connected(&self) -> bool {
        self.client.is_some()
    }

    pub fn client(&self) -> Option<Client> {
        self.client.clone()
    }

    pub fn matches_config(&self, kubeconfig: Option<&String>, context: &str) -> bool {
        self.kubeconfig.as_ref() == kubeconfig && self.context == context
    }

    /// Build a `kube::Client` from the configured kubeconfig
    /// file and context.
    pub async fn connect(&mut self) -> Result<(), String> {
        let kubeconfig = match &self.kubeconfig {
            Some(path) => kube::config::Kubeconfig::read_from(path)
                .map_err(|e| format!("failed to read kubeconfig '{path}': {e}"))?,
            None => kube::config::Kubeconfig::read()
                .map_err(|e| format!("failed to read default kubeconfig: {e}"))?,
        };

        let options = kube::config::KubeConfigOptions {
            context: Some(self.context.clone()),
            ..Default::default()
        };

        let config = Config::from_custom_kubeconfig(kubeconfig, &options)
            .await
            .map_err(|e| {
                format!(
                    "failed to build kube config for context '{}': {e}",
                    self.context
                )
            })?;

        let client =
            Client::try_from(config).map_err(|e| format!("failed to create kube client: {e}"))?;

        self.client = Some(client);
        Ok(())
    }

    /// Fetch kubelet stats summaries from all cluster nodes.
    ///
    /// Nodes are fetched concurrently. Individual node failures
    /// are logged and skipped rather than aborting the entire
    /// operation.
    pub async fn fetch_all_node_stats(client: &Client) -> Result<Vec<StatsSummary>, String> {
        let nodes_api: Api<Node> = Api::all(client.clone());
        let nodes = nodes_api
            .list(&ListParams::default())
            .await
            .map_err(|e| format!("failed to list nodes: {e}"))?;

        let mut tasks = JoinSet::new();

        for node in &nodes {
            let name = node.metadata.name.as_deref().unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let client = client.clone();
            let name = name.to_string();
            tasks.spawn(async move { fetch_node_stats(&client, &name).await });
        }

        let mut summaries = Vec::with_capacity(nodes.items.len());
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(summary)) => summaries.push(summary),
                Ok(Err(e)) => {
                    tracing::warn!("skipping node stats: {e}");
                }
                Err(e) => {
                    tracing::warn!("node stats task panicked: {e}");
                }
            }
        }

        Ok(summaries)
    }

    /// Fetch recent K8s events for pods in a namespace.
    /// Returns a map from pod name to the most recent event
    /// formatted as `"{type}: {reason}"`.
    pub async fn fetch_pod_events(
        client: &Client,
        namespace: &str,
    ) -> Result<HashMap<String, String>, String> {
        let events_api: Api<Event> = Api::namespaced(client.clone(), namespace);
        let events = events_api.list(&ListParams::default()).await.map_err(|e| {
            format!(
                "failed to list events in namespace \
                     '{namespace}': {e}"
            )
        })?;

        let mut latest: HashMap<
            String,
            (
                String,
                Option<k8s_openapi::apimachinery::pkg::apis::meta::v1::Time>,
            ),
        > = HashMap::new();

        for event in &events {
            let is_pod = event.involved_object.kind.as_deref() == Some("Pod");
            if !is_pod {
                continue;
            }

            let Some(pod_name) = &event.involved_object.name else {
                continue;
            };

            let event_type = event.type_.as_deref().unwrap_or("Normal");
            let reason = event.reason.as_deref().unwrap_or("Unknown");
            let formatted = format!("{event_type}: {reason}");

            let is_newer = match latest.get(pod_name.as_str()) {
                None => true,
                Some((_, prev_ts)) => match (&event.last_timestamp, prev_ts) {
                    (Some(new), Some(old)) => new.0 >= old.0,
                    (Some(_), None) => true,
                    _ => false,
                },
            };

            if is_newer {
                latest.insert(pod_name.clone(), (formatted, event.last_timestamp.clone()));
            }
        }

        Ok(latest.into_iter().map(|(k, (v, _))| (k, v)).collect())
    }

    /// Fetch CPU, memory, disk, and network metrics aggregated
    /// across all cluster nodes. Allocatable totals are passed
    /// in to avoid redundant API calls.
    #[expect(
        clippy::cast_precision_loss,
        reason = "byte/nanosecond sums fit comfortably in f64 \
                  mantissa for percentage and rate calculations"
    )]
    pub async fn collect_metrics(
        &mut self,
        server_name: &str,
        stats: &[StatsSummary],
        alloc_cpu: f64,
        alloc_mem: u64,
    ) -> Result<ServerMetrics, String> {
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| "k8s client not connected".to_string())?
            .clone();

        let (cpu_used_cores, mem_used_bytes) = fetch_cpu_mem_usage(&client).await?;

        let cpu_pct = if alloc_cpu > 0.0 {
            cpu_used_cores / alloc_cpu * 100.0
        } else {
            0.0
        };
        #[expect(
            clippy::cast_precision_loss,
            reason = "memory byte totals fit comfortably in f64"
        )]
        let mem_pct = if alloc_mem > 0 {
            mem_used_bytes as f64 / alloc_mem as f64 * 100.0
        } else {
            0.0
        };

        let (disk_pct, total_rx, total_tx, disk_used, disk_capacity) =
            compute_cluster_disk_net(stats);

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
            _ => (0, 0),
        };

        self.prev_net_bytes = Some((total_rx, total_tx));
        self.prev_poll_time = Some(now);

        #[expect(
            clippy::cast_possible_truncation,
            reason = "cluster node count is always small"
        )]
        let node_count = stats.len() as u32;

        Ok(ServerMetrics {
            server_name: server_name.to_string(),
            server_type: "k8s".to_string(),
            status: ServerStatus::Online,
            cpu_percent: cpu_pct,
            memory_percent: mem_pct,
            disk_percent: disk_pct,
            net_rx_bytes_per_sec: rx_per_sec,
            net_tx_bytes_per_sec: tx_per_sec,
            cpu_millicores: cpu_used_cores * 1000.0,
            memory_bytes: mem_used_bytes,
            disk_used_bytes: disk_used,
            disk_capacity_bytes: disk_capacity,
            node_count,
            ..ServerMetrics::default()
        })
    }

    /// Collect per-pod CPU and memory metrics for a namespace.
    ///
    /// Each pod becomes a separate `ServerMetrics` entry with
    /// `server_type: "pod"` and `server_name: "{cluster}/{pod}"`.
    /// Percentages are relative to the pod's own resource
    /// requests (preferred) or limits. Falls back to cluster
    /// allocatable when neither is set.
    #[expect(
        clippy::cast_precision_loss,
        reason = "byte deltas fit comfortably in f64 for rate calc"
    )]
    pub async fn collect_pod_metrics(
        &mut self,
        cluster_name: &str,
        namespace: &str,
        stats: &[StatsSummary],
        events: &HashMap<String, String>,
        cluster_cpu: f64,
        cluster_mem: u64,
    ) -> Result<Vec<ServerMetrics>, String> {
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| "k8s client not connected".to_string())?
            .clone();

        let metrics_api: Api<PodMetrics> = Api::namespaced(client.clone(), namespace);
        let pod_metrics_list = metrics_api
            .list(&ListParams::default())
            .await
            .map_err(|e| {
                format!(
                    "failed to list pod metrics in namespace \
                     '{namespace}': {e}"
                )
            })?;

        let pods_api: Api<Pod> = Api::namespaced(client, namespace);
        let pod_specs = pods_api.list(&ListParams::default()).await.map_err(|e| {
            format!(
                "failed to list pods in namespace \
                     '{namespace}': {e}"
            )
        })?;

        let pvc_map = extract_pod_pvc(stats, namespace);
        let net_map = extract_pod_network(stats, namespace);

        let pod_index: HashMap<&str, &Pod> = pod_specs
            .items
            .iter()
            .filter_map(|p| p.metadata.name.as_deref().map(|n| (n, p)))
            .collect();

        let mut results = Vec::with_capacity(pod_metrics_list.items.len());

        for pm in &pod_metrics_list {
            let m = build_pod_server_metrics(
                pm,
                &pod_index,
                cluster_name,
                cluster_cpu,
                cluster_mem,
                &pvc_map,
                events,
            )?;
            results.push(m);
        }

        let now = Instant::now();
        if let Some(prev_time) = self.prev_pod_poll_time {
            let elapsed = now.duration_since(prev_time).as_secs_f64();
            if elapsed > 0.0 {
                for m in &mut results {
                    let pod_name = m.server_name.split_once('/').map_or("", |(_, p)| p);
                    let Some(&(curr_rx, curr_tx)) = net_map.get(pod_name) else {
                        continue;
                    };
                    let Some(&(prev_rx, prev_tx)) = self.prev_pod_net.get(pod_name) else {
                        continue;
                    };
                    let rx_rate = curr_rx.saturating_sub(prev_rx) as f64 / elapsed;
                    let tx_rate = curr_tx.saturating_sub(prev_tx) as f64 / elapsed;
                    #[expect(
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss,
                        reason = "rates from byte deltas are \
                                  always small positive"
                    )]
                    {
                        m.net_rx_bytes_per_sec = rx_rate as u64;
                        m.net_tx_bytes_per_sec = tx_rate as u64;
                    }
                }
            }
        }

        self.prev_pod_net = net_map;
        self.prev_pod_poll_time = Some(now);

        Ok(results)
    }

    /// Collect all metrics for a K8s cluster: node-level
    /// aggregates plus per-pod metrics. Fetches allocatable
    /// totals once and reuses them for both calculations.
    pub async fn collect_all(
        &mut self,
        server_name: &str,
        namespace: &str,
    ) -> Result<Vec<ServerMetrics>, String> {
        if !self.is_connected() {
            self.connect().await?;
        }

        let client = self
            .client()
            .ok_or_else(|| "k8s client not connected".to_string())?;

        let (alloc_cpu, alloc_mem) = fetch_allocatable_totals(&client).await?;

        let stats = Self::fetch_all_node_stats(&client)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("failed to fetch node stats for {server_name}: {e}");
                Vec::new()
            });

        let events = Self::fetch_pod_events(&client, namespace)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    "failed to fetch pod events \
                     for {server_name}/{namespace}: {e}"
                );
                HashMap::new()
            });

        let node_metrics = self
            .collect_metrics(server_name, &stats, alloc_cpu, alloc_mem)
            .await?;

        let pod_metrics = self
            .collect_pod_metrics(
                server_name,
                namespace,
                &stats,
                &events,
                alloc_cpu,
                alloc_mem,
            )
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    "failed to collect pod metrics \
                     for {server_name}/{namespace}: {e}"
                );
                Vec::new()
            });

        let mut results = Vec::with_capacity(1 + pod_metrics.len());
        results.push(node_metrics);
        results.extend(pod_metrics);
        Ok(results)
    }
}

/// Build a `ServerMetrics` for one pod from its metrics and
/// spec data.
#[expect(
    clippy::cast_precision_loss,
    reason = "byte sums fit in f64 for percentage calculations"
)]
fn build_pod_server_metrics(
    pm: &PodMetrics,
    pod_index: &HashMap<&str, &Pod>,
    cluster_name: &str,
    cluster_cpu: f64,
    cluster_mem: u64,
    pvc_map: &HashMap<String, (u64, u64)>,
    events: &HashMap<String, String>,
) -> Result<ServerMetrics, String> {
    let pod_name = pm.metadata.name.as_deref().unwrap_or("unknown");
    let pod = pod_index.get(pod_name).copied();

    let mut cpu_used = 0.0_f64;
    let mut mem_used = 0_u64;

    for c in &pm.containers {
        cpu_used += parse_cpu_quantity(&c.usage.cpu)?;
        mem_used += parse_memory_quantity(&c.usage.memory)?;
    }

    let (cpu_pct, mem_pct) =
        compute_pod_percentages(cpu_used, mem_used, pod, cluster_cpu, cluster_mem);

    let (pvc_used, pvc_cap) = pvc_map.get(pod_name).copied().unwrap_or((0, 0));
    let disk_pct = if pvc_cap > 0 {
        pvc_used as f64 / pvc_cap as f64 * 100.0
    } else {
        0.0
    };

    let last_event = events.get(pod_name).cloned().unwrap_or_default();

    Ok(ServerMetrics {
        server_name: format!("{cluster_name}/{pod_name}"),
        server_type: "pod".to_string(),
        status: ServerStatus::Online,
        cpu_percent: cpu_pct,
        memory_percent: mem_pct,
        disk_percent: disk_pct,
        cpu_millicores: cpu_used * 1000.0,
        memory_bytes: mem_used,
        restart_count: pod_restart_count(pod),
        start_time: pod_start_time(pod),
        pod_status: derive_pod_status(pod),
        pvc_used_bytes: pvc_used,
        pvc_capacity_bytes: pvc_cap,
        last_event,
        ..ServerMetrics::default()
    })
}

/// Compute CPU and memory percentages for a pod, preferring
/// pod-level requests/limits over cluster allocatable.
#[expect(
    clippy::cast_precision_loss,
    reason = "byte sums fit in f64 for percentage calculations"
)]
fn compute_pod_percentages(
    cpu_used: f64,
    mem_used: u64,
    pod: Option<&Pod>,
    cluster_cpu: f64,
    cluster_mem: u64,
) -> (f64, f64) {
    let (pod_cpu_alloc, pod_mem_alloc) = pod_allocations(pod);

    let cpu_base = if pod_cpu_alloc > 0.0 {
        pod_cpu_alloc
    } else {
        cluster_cpu
    };
    let mem_base = if pod_mem_alloc > 0 {
        pod_mem_alloc
    } else {
        cluster_mem
    };

    let cpu_pct = if cpu_base > 0.0 {
        cpu_used / cpu_base * 100.0
    } else {
        0.0
    };
    let mem_pct = if mem_base > 0 {
        mem_used as f64 / mem_base as f64 * 100.0
    } else {
        0.0
    };

    (cpu_pct, mem_pct)
}

/// Sum restart counts across all containers in a pod.
fn pod_restart_count(pod: Option<&Pod>) -> u32 {
    let Some(pod) = pod else {
        return 0;
    };

    let Some(status) = &pod.status else {
        return 0;
    };

    let Some(statuses) = &status.container_statuses else {
        return 0;
    };

    #[expect(
        clippy::cast_sign_loss,
        reason = "restart_count is always non-negative"
    )]
    statuses.iter().map(|cs| cs.restart_count as u32).sum()
}

/// Extract the pod start time as an ISO 8601 string.
fn pod_start_time(pod: Option<&Pod>) -> String {
    let time = pod
        .and_then(|p| p.status.as_ref())
        .and_then(|s| s.start_time.as_ref());

    let Some(t) = time else {
        return String::new();
    };

    k8s_openapi::jiff::fmt::strtime::format("%Y-%m-%dT%H:%M:%SZ", t.0).unwrap_or_default()
}

/// Derive a human-readable pod status from phase and container
/// states. Container-level reasons (e.g. `CrashLoopBackOff`)
/// override the pod phase since they are more specific.
fn derive_pod_status(pod: Option<&Pod>) -> String {
    let Some(pod) = pod else {
        return String::new();
    };

    let Some(status) = &pod.status else {
        return String::new();
    };

    if let Some(reason) = container_override_reason(status) {
        return reason;
    }

    status.phase.clone().unwrap_or_default()
}

/// Check container statuses for waiting/terminated reasons
/// that should override the pod phase.
fn container_override_reason(status: &k8s_openapi::api::core::v1::PodStatus) -> Option<String> {
    let statuses = status.container_statuses.as_ref()?;

    let override_waiting = ["CrashLoopBackOff", "ImagePullBackOff", "ErrImagePull"];
    let override_terminated = ["OOMKilled"];

    for cs in statuses {
        let Some(state) = &cs.state else { continue };

        if let Some(waiting) = &state.waiting {
            if let Some(reason) = &waiting.reason {
                if override_waiting.contains(&reason.as_str()) {
                    return Some(reason.clone());
                }
            }
        }

        if let Some(terminated) = &state.terminated {
            if let Some(reason) = &terminated.reason {
                if override_terminated.contains(&reason.as_str()) {
                    return Some(reason.clone());
                }
            }
        }
    }

    None
}

/// Sum allocatable CPU (fractional cores) and memory (bytes)
/// across all cluster nodes.
async fn fetch_allocatable_totals(client: &Client) -> Result<(f64, u64), String> {
    let nodes_api: Api<Node> = Api::all(client.clone());
    let nodes = nodes_api
        .list(&ListParams::default())
        .await
        .map_err(|e| format!("failed to list nodes: {e}"))?;

    let mut total_cpu = 0.0_f64;
    let mut total_mem = 0_u64;

    for node in &nodes {
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
        total_mem += parse_memory_quantity(mem_q)?;
    }

    Ok((total_cpu, total_mem))
}

/// Sum a pod's container resource requests (CPU in fractional
/// cores, memory in bytes). Prefers `requests`; falls back to
/// `limits`. Returns `(0.0, 0)` when neither is set.
fn pod_allocations(pod: Option<&Pod>) -> (f64, u64) {
    let Some(pod) = pod else {
        return (0.0, 0);
    };

    let Some(spec) = &pod.spec else {
        return (0.0, 0);
    };

    let mut total_cpu = 0.0_f64;
    let mut total_mem = 0_u64;

    for container in &spec.containers {
        let Some(res) = &container.resources else {
            continue;
        };

        // Prefer requests, fall back to limits
        let cpu_q = res
            .requests
            .as_ref()
            .and_then(|r| r.get("cpu"))
            .or_else(|| res.limits.as_ref().and_then(|l| l.get("cpu")));
        let mem_q = res
            .requests
            .as_ref()
            .and_then(|r| r.get("memory"))
            .or_else(|| res.limits.as_ref().and_then(|l| l.get("memory")));

        if let Some(q) = cpu_q {
            if let Ok(v) = parse_cpu_quantity(q) {
                total_cpu += v;
            }
        }
        if let Some(q) = mem_q {
            if let Ok(v) = parse_memory_quantity(q) {
                total_mem += v;
            }
        }
    }

    (total_cpu, total_mem)
}

/// Fetch total CPU (fractional cores) and memory (bytes)
/// currently used across all nodes from the Metrics API.
async fn fetch_cpu_mem_usage(client: &Client) -> Result<(f64, u64), String> {
    let metrics_api: Api<NodeMetrics> = Api::all(client.clone());
    let node_metrics = metrics_api
        .list(&ListParams::default())
        .await
        .map_err(|e| {
            format!(
                "failed to list node metrics (is metrics-server \
             installed?): {e}"
            )
        })?;

    let mut total_cpu = 0.0_f64;
    let mut total_mem = 0_u64;

    for nm in &node_metrics {
        total_cpu += parse_cpu_quantity(&nm.usage.cpu)?;
        total_mem += parse_memory_quantity(&nm.usage.memory)?;
    }

    Ok((total_cpu, total_mem))
}

/// Compute aggregate disk and network stats from pre-fetched
/// node stats summaries. Returns
/// `(disk_pct, total_rx, total_tx, disk_used, disk_capacity)`.
#[expect(
    clippy::cast_precision_loss,
    reason = "disk byte totals fit comfortably in f64"
)]
fn compute_cluster_disk_net(summaries: &[StatsSummary]) -> (f64, u64, u64, u64, u64) {
    let mut total_disk_used = 0_u64;
    let mut total_disk_capacity = 0_u64;
    let mut total_rx = 0_u64;
    let mut total_tx = 0_u64;

    for summary in summaries {
        if let Some(fs) = &summary.node.fs {
            total_disk_used += fs.used_bytes.unwrap_or(0);
            total_disk_capacity += fs.capacity_bytes.unwrap_or(0);
        }
        if let Some(net) = &summary.node.network {
            let (rx, tx) = net.effective_bytes();
            total_rx += rx;
            total_tx += tx;
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
fn extract_pod_pvc(summaries: &[StatsSummary], namespace: &str) -> HashMap<String, (u64, u64)> {
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
                entry.0 += vol.used_bytes.unwrap_or(0);
                entry.1 += vol.capacity_bytes.unwrap_or(0);
            }
        }
    }

    result
}

/// Extract cumulative network rx/tx bytes per pod from kubelet
/// stats summaries, filtered to a specific namespace.
/// Host-networked pods lack top-level `rxBytes`/`txBytes`;
/// `effective_bytes()` falls back to summing physical interfaces.
fn extract_pod_network(summaries: &[StatsSummary], namespace: &str) -> HashMap<String, (u64, u64)> {
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
            entry.0 += rx;
            entry.1 += tx;
        }
    }

    result
}

/// Fetch the kubelet stats summary for a single node via the
/// node proxy API.
async fn fetch_node_stats(client: &Client, node_name: &str) -> Result<StatsSummary, String> {
    let url = format!("/api/v1/nodes/{node_name}/proxy/stats/summary");

    let request = http::Request::get(&url).body(Vec::new()).map_err(|e| {
        format!(
            "failed to build stats request for node \
             '{node_name}': {e}"
        )
    })?;

    client
        .request::<StatsSummary>(request)
        .await
        .map_err(|e| format!("failed to fetch stats for node '{node_name}': {e}"))
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
/// Handles binary suffixes (Ki, Mi, Gi, Ti), decimal suffixes
/// (k, M, G, T), exponent notation (e.g. "129e6"), and plain
/// byte values.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "memory quantities from K8s are always non-negative \
              and fit in u64"
)]
fn parse_memory_quantity(q: &Quantity) -> Result<u64, String> {
    let s = &q.0;
    let err = |e| format!("invalid memory quantity '{s}': {e}");

    // Binary suffixes (check before decimal — "Mi" before "M")
    if let Some(v) = s.strip_suffix("Ki") {
        return v.parse::<u64>().map(|n| n * 1024).map_err(err);
    }
    if let Some(v) = s.strip_suffix("Mi") {
        return v.parse::<u64>().map(|n| n * 1024 * 1024).map_err(err);
    }
    if let Some(v) = s.strip_suffix("Gi") {
        return v
            .parse::<u64>()
            .map(|n| n * 1024 * 1024 * 1024)
            .map_err(err);
    }
    if let Some(v) = s.strip_suffix("Ti") {
        return v
            .parse::<u64>()
            .map(|n| n * 1024 * 1024 * 1024 * 1024)
            .map_err(err);
    }

    // Decimal suffixes — parse as f64 to support fractional
    // values like "1.5G"
    let ferr = |e| format!("invalid memory quantity '{s}': {e}");
    if let Some(v) = s.strip_suffix('k') {
        return v.parse::<u64>().map(|n| n * 1000).map_err(err);
    }
    if let Some(v) = s.strip_suffix('M') {
        return v
            .parse::<f64>()
            .map(|n| (n * 1_000_000.0) as u64)
            .map_err(ferr);
    }
    if let Some(v) = s.strip_suffix('G') {
        return v
            .parse::<f64>()
            .map(|n| (n * 1_000_000_000.0) as u64)
            .map_err(ferr);
    }
    if let Some(v) = s.strip_suffix('T') {
        return v
            .parse::<f64>()
            .map(|n| (n * 1_000_000_000_000.0) as u64)
            .map_err(ferr);
    }

    // Plain bytes or exponent notation (e.g. "4096", "129e6")
    if let Ok(n) = s.parse::<u64>() {
        return Ok(n);
    }
    s.parse::<f64>().map_err(ferr).map(|n| n as u64)
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
        let result = parse_cpu_quantity(&q("1000000000n")).unwrap();
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
        assert!(result.unwrap_err().contains("invalid cpu quantity"));
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
        assert!(result.unwrap_err().contains("invalid memory quantity"));
    }

    #[test]
    fn memory_invalid_plain() {
        let result = parse_memory_quantity(&q("notanumber"));
        assert!(result.is_err());
    }

    // -- Decimal memory suffixes --

    #[test]
    fn memory_decimal_kilo() {
        let result = parse_memory_quantity(&q("500k")).unwrap();
        assert_eq!(result, 500_000);
    }

    #[test]
    fn memory_decimal_mega() {
        let result = parse_memory_quantity(&q("256M")).unwrap();
        assert_eq!(result, 256_000_000);
    }

    #[test]
    fn memory_decimal_mega_fractional() {
        let result = parse_memory_quantity(&q("1.5M")).unwrap();
        assert_eq!(result, 1_500_000);
    }

    #[test]
    fn memory_decimal_giga() {
        let result = parse_memory_quantity(&q("2G")).unwrap();
        assert_eq!(result, 2_000_000_000);
    }

    #[test]
    fn memory_decimal_tera() {
        let result = parse_memory_quantity(&q("1T")).unwrap();
        assert_eq!(result, 1_000_000_000_000);
    }

    #[test]
    fn memory_exponent_notation() {
        let result = parse_memory_quantity(&q("129e6")).unwrap();
        assert_eq!(result, 129_000_000);
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

    // -- NodeMetrics JSON parsing --

    // -- PodMetrics JSON parsing --

    #[test]
    fn parse_pod_metrics() {
        let json = r#"{
            "metadata": {
                "name": "nginx-abc123",
                "namespace": "default"
            },
            "containers": [
                {
                    "name": "nginx",
                    "usage": { "cpu": "50m", "memory": "128Mi" }
                },
                {
                    "name": "sidecar",
                    "usage": { "cpu": "10m", "memory": "64Mi" }
                }
            ]
        }"#;
        let pm: PodMetrics = serde_json::from_str(json).expect("parse pod metrics");

        assert_eq!(pm.metadata.name.as_deref(), Some("nginx-abc123"));
        assert_eq!(pm.containers.len(), 2);
        assert_eq!(pm.containers[0].usage.cpu.0, "50m");
        assert_eq!(pm.containers[0].usage.memory.0, "128Mi");
        assert_eq!(pm.containers[1].usage.cpu.0, "10m");
        assert_eq!(pm.containers[1].usage.memory.0, "64Mi");
    }

    #[test]
    fn parse_pod_metrics_empty_containers() {
        let json = r#"{
            "metadata": { "name": "init-pod" },
            "containers": []
        }"#;
        let pm: PodMetrics = serde_json::from_str(json).expect("parse pod metrics");

        assert_eq!(pm.metadata.name.as_deref(), Some("init-pod"));
        assert!(pm.containers.is_empty());
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
        let nm: NodeMetrics = serde_json::from_str(json).expect("parse node metrics");

        assert_eq!(nm.metadata.name.as_deref(), Some("worker-1"));
        assert_eq!(nm.usage.cpu.0, "250m");
        assert_eq!(nm.usage.memory.0, "1024Mi");
    }

    // -- Pod metadata helper tests --

    fn make_pod_with_restarts(name: &str, restarts: &[i32]) -> Pod {
        use k8s_openapi::api::core::v1::{ContainerStatus, PodStatus};

        let statuses: Vec<ContainerStatus> = restarts
            .iter()
            .map(|&r| ContainerStatus {
                restart_count: r,
                ..ContainerStatus::default()
            })
            .collect();

        Pod {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                ..Default::default()
            },
            status: Some(PodStatus {
                container_statuses: Some(statuses),
                ..Default::default()
            }),
            ..Pod::default()
        }
    }

    #[test]
    fn restart_count_sums_containers() {
        let pod = make_pod_with_restarts("web", &[3, 1]);
        assert_eq!(pod_restart_count(Some(&pod)), 4);
    }

    #[test]
    fn restart_count_zero_for_missing_pod() {
        assert_eq!(pod_restart_count(None), 0);
    }

    #[test]
    fn restart_count_zero_for_no_statuses() {
        let pod = Pod {
            metadata: ObjectMeta {
                name: Some("empty".to_string()),
                ..Default::default()
            },
            status: Some(k8s_openapi::api::core::v1::PodStatus::default()),
            ..Pod::default()
        };
        assert_eq!(pod_restart_count(Some(&pod)), 0);
    }

    #[test]
    fn start_time_returns_iso_string() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;

        let ts = k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap();
        let pod = Pod {
            metadata: ObjectMeta {
                name: Some("timed".to_string()),
                ..Default::default()
            },
            status: Some(k8s_openapi::api::core::v1::PodStatus {
                start_time: Some(Time(ts)),
                ..Default::default()
            }),
            ..Pod::default()
        };
        let result = pod_start_time(Some(&pod));
        assert_eq!(result, "2023-11-14T22:13:20Z");
    }

    #[test]
    fn start_time_empty_for_missing_pod() {
        assert_eq!(pod_start_time(None), "");
    }

    #[test]
    fn derive_status_running() {
        let pod = Pod {
            metadata: ObjectMeta {
                name: Some("run".to_string()),
                ..Default::default()
            },
            status: Some(k8s_openapi::api::core::v1::PodStatus {
                phase: Some("Running".to_string()),
                ..Default::default()
            }),
            ..Pod::default()
        };
        assert_eq!(derive_pod_status(Some(&pod)), "Running");
    }

    #[test]
    fn derive_status_crashloop_overrides_phase() {
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateWaiting, ContainerStatus, PodStatus,
        };

        let pod = Pod {
            metadata: ObjectMeta {
                name: Some("crash".to_string()),
                ..Default::default()
            },
            status: Some(PodStatus {
                phase: Some("Running".to_string()),
                container_statuses: Some(vec![ContainerStatus {
                    state: Some(ContainerState {
                        waiting: Some(ContainerStateWaiting {
                            reason: Some("CrashLoopBackOff".to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..ContainerStatus::default()
                }]),
                ..Default::default()
            }),
            ..Pod::default()
        };
        assert_eq!(derive_pod_status(Some(&pod)), "CrashLoopBackOff");
    }

    #[test]
    fn derive_status_oomkilled() {
        use k8s_openapi::api::core::v1::{
            ContainerState, ContainerStateTerminated, ContainerStatus, PodStatus,
        };

        let pod = Pod {
            metadata: ObjectMeta {
                name: Some("oom".to_string()),
                ..Default::default()
            },
            status: Some(PodStatus {
                phase: Some("Running".to_string()),
                container_statuses: Some(vec![ContainerStatus {
                    state: Some(ContainerState {
                        terminated: Some(ContainerStateTerminated {
                            reason: Some("OOMKilled".to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..ContainerStatus::default()
                }]),
                ..Default::default()
            }),
            ..Pod::default()
        };
        assert_eq!(derive_pod_status(Some(&pod)), "OOMKilled");
    }

    #[test]
    fn derive_status_empty_for_missing_pod() {
        assert_eq!(derive_pod_status(None), "");
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

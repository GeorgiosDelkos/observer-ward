//! Kubernetes Metrics API + kubelet stats collection.
//!
//! One backend per cluster. Nodes, node metrics and kubelet stats are read
//! once per poll and reported both per node and as cluster totals; pods,
//! events and Jobs are read for each watched namespace over the same
//! connection.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::{Namespace, Node, Pod};
use kube::Client;
use kube::api::{Api, ListParams};
use tokio::task::{JoinError, JoinSet};

use crate::config::K8sTarget;
use crate::metrics::{
    ClusterMetrics, NamespaceMetrics, NetRate, NetSample, NodeMetrics, PodMetrics, Requested,
    ResourceUsage, Resources, Usage,
};

mod error;
mod events;
mod jobs;
mod kubeconfig;
mod metrics_api;
mod nodes;
mod pods;
mod quantity;
mod requests;
mod stats;

pub(crate) use error::K8sError;
pub(crate) use kubeconfig::{KubeconfigSummary, inspect, validate_server};

use events::fetch_pod_events;
use jobs::{JobList, JobOwners, list_job_owners};
use metrics_api::{NodeMetrics as NodeMetricsSample, PodMetrics as PodMetricsSample};
use nodes::{node_metrics, node_totals};
use pods::{PodContext, assemble_pods, jobs_of};
use quantity::{parse_cpu_quantity, parse_memory_quantity};
use requests::{RequestList, list_requests};
use stats::{
    ClusterTotals, StatsSummary, cluster_totals, extract_pod_network, extract_pod_pvc,
    fetch_all_node_stats, node_disks,
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
    fn resources(self) -> Resources {
        Resources {
            cpu_millicores: self.cpu * 1000.0,
            memory_bytes: self.mem,
        }
    }

    /// The larger of `self` and `other`, per resource.
    fn max(self, other: Self) -> Self {
        Self {
            cpu: self.cpu.max(other.cpu),
            mem: self.mem.max(other.mem),
        }
    }
}

impl std::iter::Sum for CpuMem {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::default(), |total, next| Self {
            cpu: total.cpu + next.cpu,
            mem: total.mem.saturating_add(next.mem),
        })
    }
}

/// Kubernetes backend for one cluster: node totals and per-node reports,
/// plus one card per pod in each watched namespace.
pub(crate) struct K8sBackend {
    target: K8sTarget,
    client: Option<Client>,
    prev_net: Option<NetSample>,
    /// Keyed by namespace name; only watched namespaces are kept.
    namespaces: HashMap<String, NamespaceState>,
    /// Listing pods in all namespaces was refused; not retried until the
    /// client is rebuilt (new credentials may allow it).
    requests_forbidden: bool,
}

/// What a backend remembers about one watched namespace between polls.
#[derive(Default)]
struct NamespaceState {
    prev_pod_net: HashMap<String, NetSample>,
    /// From the last Job list, kept until a Job with pods is missing from it.
    job_owners: JobOwners,
    /// Reading Jobs was refused; not retried until the client is rebuilt.
    jobs_forbidden: bool,
}

/// One namespace's pod listing and events, read concurrently with the
/// other namespaces.
struct NamespaceListing {
    name: String,
    pods: Result<(Vec<PodMetricsSample>, Vec<Pod>), K8sError>,
    events: HashMap<String, String>,
}

impl K8sBackend {
    pub(crate) fn new(target: K8sTarget) -> Self {
        Self {
            target,
            client: None,
            prev_net: None,
            namespaces: HashMap::new(),
            requests_forbidden: false,
        }
    }

    /// Take an edited target for the same cluster (see
    /// [`K8sTarget::same_connection`]): watched namespaces and hidden
    /// workloads may change without a new connection.
    pub(crate) fn set_target(&mut self, target: K8sTarget) {
        self.target = target;
    }

    pub(crate) fn target(&self) -> &K8sTarget {
        &self.target
    }

    /// Collect node reports, cluster totals and pod cards. Any failure
    /// drops the client so the next poll rebuilds it from the kubeconfig
    /// (picking up refreshed credentials).
    ///
    /// # Errors
    ///
    /// [`K8sError`] if the client cannot be built or a required API call
    /// (nodes, node metrics) fails. Pod, event, Job and request listing
    /// failures only degrade the result.
    pub(crate) async fn collect(&mut self) -> Result<ClusterMetrics, K8sError> {
        let client = if let Some(client) = &self.client {
            client.clone()
        } else {
            // New credentials may be allowed what the old ones were not.
            self.requests_forbidden = false;
            for state in self.namespaces.values_mut() {
                state.jobs_forbidden = false;
            }
            connect(self.target.kubeconfig.clone(), &self.target.context).await?
        };
        let result = self.collect_with(&client).await;
        self.client = result.is_ok().then_some(client);
        result
    }

    async fn collect_with(&mut self, client: &Client) -> Result<ClusterMetrics, K8sError> {
        let nodes = fetch_nodes(client).await?;
        let watched: Vec<String> = self
            .target
            .namespaces
            .iter()
            .map(|ns| ns.name.clone())
            .collect();

        let (stats, node_usage, requests, listings) = tokio::join!(
            fetch_all_node_stats(client, &nodes),
            fetch_node_usage(client),
            self.read_requests(client),
            fetch_namespaces(client, &watched),
        );
        let nodes = node_metrics(&nodes, &node_usage?, &node_disks(&stats));
        let totals = node_totals(&nodes);

        self.namespaces.retain(|name, _| watched.contains(name));
        self.resolve_jobs(client, &listings).await;
        let namespaces = listings
            .into_iter()
            .map(|listing| self.namespace_metrics(listing, &stats, &nodes))
            .collect();

        let fs_net = cluster_totals(&stats);
        Ok(ClusterMetrics {
            usage: ResourceUsage {
                usage: Usage {
                    disk_percent: fs_net.disk.percent(),
                    ..totals.usage
                },
                used: totals.used,
            },
            net: self.net_rate(&fs_net),
            disk: fs_net.disk,
            requested: self.requested(requests, &totals.allocatable),
            nodes,
            namespaces,
        })
    }

    /// Requests across the cluster, unless that was refused before.
    async fn read_requests(&self, client: &Client) -> RequestList {
        if self.requests_forbidden {
            return RequestList::Forbidden;
        }
        list_requests(client).await
    }

    /// `requests` as a share of `allocatable`. A refusal is logged once and
    /// remembered until the client is rebuilt.
    fn requested(&mut self, requests: RequestList, allocatable: &Resources) -> Option<Requested> {
        match requests {
            RequestList::Requested(requested) => {
                let share = requested.resources().share_of(allocatable);
                Some(Requested {
                    cpu_percent: share.cpu_percent,
                    memory_percent: share.memory_percent,
                })
            }
            RequestList::Forbidden => {
                if !self.requests_forbidden {
                    tracing::warn!(
                        "no permission to list pods in all namespaces of {}: \
                         requested capacity is not shown",
                        self.target.name
                    );
                    self.requests_forbidden = true;
                }
                None
            }
            RequestList::Failed => None,
        }
    }

    /// Cluster network throughput since the last poll.
    fn net_rate(&mut self, totals: &ClusterTotals) -> Option<NetRate> {
        let now = NetSample {
            rx_bytes: totals.rx_bytes,
            tx_bytes: totals.tx_bytes,
            at: Instant::now(),
        };
        let net = self.prev_net.and_then(|prev| now.rate_since(&prev));
        self.prev_net = Some(now);
        net
    }

    /// Pod cards for one namespace. A failed pod listing leaves `pods`
    /// `None`, so the UI keeps the cards it has.
    fn namespace_metrics(
        &mut self,
        listing: NamespaceListing,
        stats: &[StatsSummary],
        nodes: &[NodeMetrics],
    ) -> NamespaceMetrics {
        let NamespaceListing { name, pods, events } = listing;
        let pods = match pods {
            Ok((samples, specs)) => {
                Some(self.pod_cards(&name, &samples, &specs, stats, &events, nodes))
            }
            Err(e) => {
                tracing::warn!(
                    "failed to collect pod metrics for {}/{name}: {}",
                    self.target.name,
                    crate::error::error_chain(&e)
                );
                None
            }
        };
        NamespaceMetrics { name, pods }
    }

    /// Re-list the Jobs of each namespace where a Job with pods is not in
    /// the last list (a new cron job run), all namespaces at once. Elsewhere
    /// the last list still answers.
    async fn resolve_jobs(&mut self, client: &Client, listings: &[NamespaceListing]) {
        let stale = self.stale_jobs(listings);
        let lists = stale.iter().map(|(namespace, wanted)| {
            let client = client.clone();
            let namespace = namespace.clone();
            let wanted = wanted.clone();
            async move {
                let wanted: HashSet<&str> = wanted.iter().map(String::as_str).collect();
                list_job_owners(&client, &namespace, &wanted).await
            }
        });
        let lists = join_ordered(lists, |_, e| {
            tracing::warn!("job listing task failed: {e}");
            JobList::Failed
        })
        .await;

        for ((namespace, _), list) in stale.into_iter().zip(lists) {
            let state = self.namespaces.entry(namespace.clone()).or_default();
            match list {
                JobList::Owners(owners) => state.job_owners = owners,
                JobList::Forbidden => {
                    tracing::warn!(
                        "no permission to list jobs in {}/{namespace}: \
                         hidden cron jobs are matched by run name only",
                        self.target.name
                    );
                    state.jobs_forbidden = true;
                }
                JobList::Failed => {}
            }
        }
    }

    /// Each listed namespace whose pods name a Job the last Job list did
    /// not have, with those Jobs; namespaces where listing Jobs was refused
    /// are left out.
    fn stale_jobs(&self, listings: &[NamespaceListing]) -> Vec<(String, HashSet<String>)> {
        listings
            .iter()
            .filter_map(|listing| {
                let (_, specs) = listing.pods.as_ref().ok()?;
                let wanted = jobs_of(specs);
                let state = self.namespaces.get(&listing.name);
                let forbidden = state.is_some_and(|s| s.jobs_forbidden);
                let known = wanted
                    .iter()
                    .all(|job| state.is_some_and(|s| s.job_owners.contains_key(*job)));
                let wanted = wanted.into_iter().map(str::to_string).collect();
                (!known && !forbidden).then(|| (listing.name.clone(), wanted))
            })
            .collect()
    }

    fn pod_cards(
        &mut self,
        namespace: &str,
        samples: &[PodMetricsSample],
        specs: &[Pod],
        stats: &[StatsSummary],
        events: &HashMap<String, String>,
        nodes: &[NodeMetrics],
    ) -> Vec<PodMetrics> {
        let pvc = extract_pod_pvc(stats, namespace);
        let net = extract_pod_network(stats, namespace, Instant::now());
        let unhidden = BTreeSet::new();
        let hidden = self
            .target
            .namespace(namespace)
            .map_or(&unhidden, |ns| &ns.hidden);
        let state = self.namespaces.entry(namespace.to_string()).or_default();

        let pods = assemble_pods(
            samples,
            specs,
            &PodContext {
                nodes,
                pvc: &pvc,
                net: &net,
                prev_net: &state.prev_pod_net,
                events,
                hidden,
                job_owners: &state.job_owners,
            },
        );
        state.prev_pod_net = net;
        pods
    }
}

/// Build a `kube::Client` for `context` of the kubeconfig at `path` (the
/// default one when `None`).
async fn connect(path: Option<PathBuf>, context: &str) -> Result<Client, K8sError> {
    let kubeconfig = tokio::task::spawn_blocking(move || kubeconfig::load(path.as_deref()))
        .await
        .map_err(K8sError::KubeconfigTask)??;

    let mut config = kubeconfig::build_config(kubeconfig, context).await?;
    config.connect_timeout = Some(KUBE_CONNECT_TIMEOUT);
    config.read_timeout = Some(KUBE_READ_TIMEOUT);

    Client::try_from(config).map_err(|source| K8sError::CreateClient(Box::new(source)))
}

/// Names of the namespaces `context` can list, sorted, so the add form
/// can offer them instead of free-typed names.
///
/// # Errors
///
/// [`K8sError`] if the client cannot be built or listing namespaces fails
/// (an account limited to some namespaces may not list them at all).
pub(crate) async fn list_namespaces(
    kubeconfig: Option<PathBuf>,
    context: &str,
) -> Result<Vec<String>, K8sError> {
    let client = connect(kubeconfig, context).await?;
    let api: Api<Namespace> = Api::all(client);
    let list = api
        .list_metadata(&ListParams::default())
        .await
        .map_err(|source| K8sError::ListNamespaces(Box::new(source)))?;
    let mut names: Vec<String> = list
        .items
        .into_iter()
        .filter_map(|ns| ns.metadata.name)
        .collect();
    names.sort();
    Ok(names)
}

/// Pod lists and events of every namespace in `names`, concurrently, one
/// listing per name in the order given. A task that panics counts as a
/// failed pod listing, so the UI and the notifier treat it like one.
/// Events are decoration: a failed event list is logged and leaves the
/// namespace without them.
async fn fetch_namespaces(client: &Client, names: &[String]) -> Vec<NamespaceListing> {
    let fetches = names.iter().map(|name| {
        let client = client.clone();
        let name = name.clone();
        async move {
            let (pods, events) = tokio::join!(
                fetch_pod_lists(&client, &name),
                fetch_pod_events(&client, &name),
            );
            let events = events.unwrap_or_else(|e| {
                tracing::warn!(
                    "failed to fetch pod events in {name}: {}",
                    crate::error::error_chain(&e)
                );
                HashMap::new()
            });
            NamespaceListing { name, pods, events }
        }
    });
    join_ordered(fetches, |index, source| NamespaceListing {
        name: names[index].clone(),
        pods: Err(K8sError::NamespaceTask {
            namespace: names[index].clone(),
            source,
        }),
        events: HashMap::new(),
    })
    .await
}

/// Run `futures` concurrently, each on its own task, and return their
/// outputs in input order. A future that panics yields `failed(index,
/// error)` in its place, so the output always has one entry per input.
async fn join_ordered<T, F>(
    futures: impl IntoIterator<Item = F>,
    failed: impl Fn(usize, JoinError) -> T,
) -> Vec<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let mut tasks = JoinSet::new();
    let mut index_of = HashMap::new();
    for (index, future) in futures.into_iter().enumerate() {
        index_of.insert(tasks.spawn(future).id(), index);
    }

    let mut outputs: Vec<Option<T>> = std::iter::repeat_with(|| None)
        .take(index_of.len())
        .collect();
    while let Some(joined) = tasks.join_next_with_id().await {
        let (id, output) = match joined {
            Ok((id, output)) => (id, Ok(output)),
            Err(e) => (e.id(), Err(e)),
        };
        let Some(&index) = index_of.get(&id) else {
            continue;
        };
        outputs[index] = Some(output.unwrap_or_else(|e| failed(index, e)));
    }
    // Every spawned task is joined exactly once, so every slot is filled.
    outputs.into_iter().flatten().collect()
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

/// CPU and memory each node is using now, from the Metrics API, keyed by
/// node name.
async fn fetch_node_usage(client: &Client) -> Result<HashMap<String, CpuMem>, K8sError> {
    let metrics_api: Api<NodeMetricsSample> = Api::all(client.clone());
    let node_metrics = metrics_api
        .list(&ListParams::default())
        .await
        .map_err(|source| K8sError::ListNodeMetrics(Box::new(source)))?;

    let mut usage = HashMap::with_capacity(node_metrics.items.len());
    for sample in &node_metrics {
        let Some(name) = sample.metadata.name.clone() else {
            continue;
        };
        let used = CpuMem {
            cpu: parse_cpu_quantity(&sample.usage.cpu)?,
            mem: parse_memory_quantity(&sample.usage.memory)?,
        };
        usage.insert(name, used);
    }
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::config::{K8sTarget, WatchedNamespace};

    #[tokio::test]
    async fn join_ordered_keeps_input_order_and_reports_panics_in_place() {
        let futures = [30_u64, 0, 10]
            .into_iter()
            .enumerate()
            .map(|(i, delay)| async move {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                assert_ne!(i, 1, "namespace task blew up");
                Ok::<usize, usize>(i)
            });

        let outputs = join_ordered(futures, |index, _| Err(index)).await;

        assert_eq!(outputs, [Ok(0), Err(1), Ok(2)]);
    }

    fn backend() -> K8sBackend {
        K8sBackend::new(K8sTarget {
            name: "prod".into(),
            kubeconfig: None,
            context: "ctx".to_string(),
            namespaces: vec![WatchedNamespace {
                name: "default".to_string(),
                hidden: std::collections::BTreeSet::new(),
            }],
        })
    }

    #[test]
    fn requested_is_a_share_of_allocatable_and_a_refusal_is_remembered() {
        let mut backend = backend();
        let allocatable = Resources {
            cpu_millicores: 4000.0,
            memory_bytes: 8,
        };
        let requested = RequestList::Requested(CpuMem { cpu: 1.0, mem: 6 });

        assert_eq!(
            backend.requested(requested, &allocatable),
            Some(Requested {
                cpu_percent: 25.0,
                memory_percent: 75.0,
            })
        );
        assert!(!backend.requests_forbidden);

        assert_eq!(backend.requested(RequestList::Failed, &allocatable), None);
        assert!(!backend.requests_forbidden, "a failure is retried");

        assert_eq!(
            backend.requested(RequestList::Forbidden, &allocatable),
            None
        );
        assert!(backend.requests_forbidden, "a refusal is not");
    }

    /// A listing of `namespace` whose pods are each run by one Job.
    fn listing(namespace: &str, jobs: &[&str]) -> NamespaceListing {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
        use kube::api::ObjectMeta;

        let specs = jobs
            .iter()
            .map(|job| Pod {
                metadata: ObjectMeta {
                    name: Some(format!("{job}-pod")),
                    owner_references: Some(vec![OwnerReference {
                        kind: "Job".to_string(),
                        name: (*job).to_string(),
                        controller: Some(true),
                        ..OwnerReference::default()
                    }]),
                    ..ObjectMeta::default()
                },
                ..Pod::default()
            })
            .collect();
        NamespaceListing {
            name: namespace.to_string(),
            pods: Ok((Vec::new(), specs)),
            events: HashMap::new(),
        }
    }

    #[test]
    fn only_namespaces_with_unknown_jobs_are_relisted() {
        let mut backend = backend();
        let known = backend.namespaces.entry("known".to_string()).or_default();
        known
            .job_owners
            .insert("backup-1".to_string(), Some("backup".to_string()));
        backend
            .namespaces
            .entry("refused".to_string())
            .or_default()
            .jobs_forbidden = true;
        let mut failed = listing("failed", &["x-1"]);
        failed.pods = Err(K8sError::ListNodes(Box::new(
            kube::Error::LinesCodecMaxLineLengthExceeded,
        )));

        let stale = backend.stale_jobs(&[
            listing("known", &["backup-1"]),
            listing("new", &["sweep-7"]),
            listing("refused", &["sweep-8"]),
            listing("no-jobs", &[]),
            failed,
        ]);

        assert_eq!(
            stale,
            [("new".to_string(), HashSet::from(["sweep-7".to_string()]))]
        );
    }
}

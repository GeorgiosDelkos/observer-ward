//! Kubernetes Metrics API + kubelet stats collection.
//!
//! One backend per cluster. Nodes, node metrics and kubelet stats are read
//! once per poll and reported both per node and as cluster totals; pods,
//! events and Jobs are read for each watched namespace over the same
//! connection. Credentials that may not list nodes (a namespace-scoped
//! account) get the namespaces alone.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::{Namespace, Node, Pod};
use kube::Client;
use kube::api::{Api, ListParams};
use kube::config::Kubeconfig;
use tokio::task::{JoinError, JoinSet};

use crate::config::K8sTarget;
use crate::metrics::{
    ClusterCapacity, ClusterMetrics, NamespaceMetrics, NetRate, NetSample, NodeMetrics, PodMetrics,
    Requested, ResourceUsage, Resources, Usage,
};

mod error;
mod events;
mod exec_auth;
mod figures;
mod jobs;
mod kubeconfig;
mod metrics_api;
mod nodes;
mod pods;
mod prometheus;
mod quantity;
mod requests;
mod stats;

pub(crate) use error::K8sError;
pub(crate) use exec_auth::{login, stderr_summary};
pub(crate) use kubeconfig::{KubeconfigSummary, inspect, validate_server};

use events::fetch_pod_events;
use figures::{HostFigures, NamespaceFigures, PvcUsage, rates_since};
use jobs::{JobList, JobOwners, list_job_owners};
use metrics_api::{NodeMetrics as NodeMetricsSample, PodMetrics as PodMetricsSample};
use nodes::{node_metrics, node_totals};
use pods::pvc_by_pod;
use pods::{PodContext, assemble_pods, jobs_of};
use quantity::{parse_cpu_quantity, parse_memory_quantity};
use requests::{RequestList, list_requests};
use stats::{
    ClusterTotals, StatsList, StatsSummary, cluster_totals, extract_pod_network, extract_pod_pvc,
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
    /// What the current client was refused; cleared when the client is
    /// rebuilt, since new credentials may be allowed more.
    refused: Refusals,
    /// The last Prometheus query failed and that was logged; cleared by the
    /// next success, so a lasting failure is logged once.
    prometheus_failing: bool,
    /// The context's user runs a kubeconfig exec credential plugin. Set
    /// when connecting, before the plugin first runs, so a poll that times
    /// out while the plugin waits can still be told apart.
    credential_plugin: bool,
}

/// Cluster-wide reads the current client was refused.
#[derive(Debug, Default)]
struct Refusals {
    /// Listing pods in all namespaces; not retried until reconnect.
    requests: bool,
    /// Listing nodes, which was logged. Unlike the others the node list is
    /// still requested every poll: with nothing else required, it is what
    /// notices expired credentials or an unreachable cluster, since failed
    /// namespace listings only degrade.
    nodes: bool,
    /// Kubelet stats on every node (no `nodes/proxy`); not retried until
    /// reconnect.
    stats: bool,
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

/// Where a poll's disk, network and PVC figures came from, before they are
/// turned into [`HostFigures`].
enum HostSource {
    Prometheus(prometheus::Reading),
    Kubelet(StatsList),
    /// No Prometheus is set and kubelet stats are refused or have no nodes.
    Unavailable,
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
            refused: Refusals::default(),
            prometheus_failing: false,
            credential_plugin: false,
        }
    }

    /// Take an edited target for the same cluster (see
    /// [`K8sTarget::same_connection`]): watched namespaces, hidden
    /// workloads and the Prometheus service may change without a new
    /// connection. A new Prometheus service starts afresh: its failures are
    /// logged again, and kubelet stats are tried again when it is cleared.
    pub(crate) fn set_target(&mut self, target: K8sTarget) {
        if target.prometheus != self.target.prometheus {
            self.prometheus_failing = false;
            self.refused.stats = false;
        }
        self.target = target;
    }

    pub(crate) fn target(&self) -> &K8sTarget {
        &self.target
    }

    /// Whether the cluster's credentials come from an exec plugin, which may
    /// be waiting for a sign-in when a poll times out.
    pub(crate) fn uses_credential_plugin(&self) -> bool {
        self.credential_plugin
    }

    /// Collect node reports, cluster totals and pod cards. Any failure
    /// drops the client so the next poll rebuilds it from the kubeconfig
    /// (picking up refreshed credentials).
    ///
    /// # Errors
    ///
    /// [`K8sError`] if the client cannot be built, listing nodes fails
    /// other than by a refusal, or listing node metrics fails. Pod, event,
    /// Job and request listing failures only degrade the result.
    pub(crate) async fn collect(&mut self) -> Result<ClusterMetrics, K8sError> {
        let client = if let Some(client) = &self.client {
            client.clone()
        } else {
            // New credentials may be allowed what the old ones were not.
            self.refused = Refusals::default();
            for state in self.namespaces.values_mut() {
                state.jobs_forbidden = false;
            }
            let kubeconfig = load_kubeconfig(self.target.kubeconfig.clone()).await?;
            self.credential_plugin = exec_auth::uses_plugin(&kubeconfig, &self.target.context);
            build_client(kubeconfig, &self.target.context).await?
        };
        let result = self
            .collect_with(&client)
            .await
            .map_err(exec_auth::plugin_failure);
        self.client = result.is_ok().then_some(client);
        result
    }

    async fn collect_with(&mut self, client: &Client) -> Result<ClusterMetrics, K8sError> {
        let watched: Vec<String> = self
            .target
            .namespaces
            .iter()
            .map(|ns| ns.name.clone())
            .collect();

        let Some(nodes) = self.read_nodes(client).await? else {
            let (listings, source) = tokio::join!(
                fetch_namespaces(client, &watched),
                self.fetch_host(client, &[], &watched),
            );
            let figures = self.host_figures(source, &watched);
            let namespaces = self
                .namespace_reports(client, listings, &figures, &[])
                .await;
            return Ok(ClusterMetrics {
                capacity: None,
                namespaces,
            });
        };

        let (source, node_usage, requests, listings) = tokio::join!(
            self.fetch_host(client, &nodes, &watched),
            fetch_node_usage(client),
            self.read_requests(client),
            fetch_namespaces(client, &watched),
        );
        let figures = self.host_figures(source, &watched);
        let nodes = node_metrics(&nodes, &node_usage?, &figures.node_disks);
        let totals = node_totals(&nodes);
        let namespaces = self
            .namespace_reports(client, listings, &figures, &nodes)
            .await;

        let disk = figures.cluster_disk();
        let capacity = ClusterCapacity {
            usage: ResourceUsage {
                usage: Usage {
                    disk_percent: disk.percent(),
                    ..totals.usage
                },
                used: totals.used,
            },
            net: figures.cluster_net,
            disk,
            requested: self.requested(requests, &totals.allocatable),
            nodes,
        };
        Ok(ClusterMetrics {
            capacity: Some(capacity),
            namespaces,
        })
    }

    /// Read disk, network and PVC figures from where the target says:
    /// Prometheus when configured, else kubelet stats of `nodes` unless
    /// those were refused before.
    async fn fetch_host(&self, client: &Client, nodes: &[Node], watched: &[String]) -> HostSource {
        if let Some(service) = &self.target.prometheus {
            let names: Vec<String> = nodes
                .iter()
                .filter_map(|n| n.metadata.name.clone())
                .collect();
            let reading = prometheus::host_figures(client, service, &names, watched).await;
            return HostSource::Prometheus(reading);
        }
        if self.refused.stats || nodes.is_empty() {
            return HostSource::Unavailable;
        }
        HostSource::Kubelet(fetch_all_node_stats(client, nodes).await)
    }

    /// Turn what [`Self::fetch_host`] read into this poll's figures: rates
    /// from kubelet counters against the last poll's, and a refusal or a
    /// Prometheus failure logged once. Kubelet counters are dropped on any
    /// other source, so a later kubelet poll does not average over the gap.
    fn host_figures(&mut self, source: HostSource, watched: &[String]) -> HostFigures {
        if !matches!(source, HostSource::Kubelet(StatsList::Summaries(_))) {
            self.prev_net = None;
            for state in self.namespaces.values_mut() {
                state.prev_pod_net.clear();
            }
        }
        match source {
            HostSource::Prometheus(prometheus::Reading { figures, failure }) => {
                match failure {
                    None => self.prometheus_failing = false,
                    Some(e) if !self.prometheus_failing => {
                        tracing::warn!(
                            "missing disk or network figures for {}: {}",
                            self.target.name,
                            crate::error::error_chain(&e)
                        );
                        self.prometheus_failing = true;
                    }
                    Some(_) => {}
                }
                figures
            }
            HostSource::Kubelet(StatsList::Summaries(stats)) => {
                self.kubelet_figures(&stats, watched)
            }
            HostSource::Kubelet(StatsList::Forbidden) => {
                tracing::warn!(
                    "no permission to read kubelet stats of {} (nodes/proxy): disk and network \
                     figures need a Prometheus service set for the cluster",
                    self.target.name
                );
                self.refused.stats = true;
                HostFigures::default()
            }
            HostSource::Unavailable => HostFigures::default(),
        }
    }

    /// Figures from kubelet stats. Network counters become rates against
    /// the previous poll's counters, which are kept for the next one.
    fn kubelet_figures(&mut self, stats: &[StatsSummary], watched: &[String]) -> HostFigures {
        let mut figures = HostFigures {
            node_disks: node_disks(stats),
            cluster_net: self.net_rate(&cluster_totals(stats)),
            namespaces: HashMap::new(),
        };
        let now = Instant::now();
        for namespace in watched {
            let samples = extract_pod_network(stats, namespace, now);
            let state = self.namespaces.entry(namespace.clone()).or_default();
            let pod_net = rates_since(&samples, &state.prev_pod_net);
            state.prev_pod_net = samples;
            figures.namespaces.insert(
                namespace.clone(),
                NamespaceFigures {
                    pod_net,
                    pvc: PvcUsage::ByPod(extract_pod_pvc(stats, namespace)),
                },
            );
        }
        figures
    }

    /// The cluster's nodes, or `None` when listing them is refused. A
    /// refusal is logged once per client.
    async fn read_nodes(&mut self, client: &Client) -> Result<Option<Vec<Node>>, K8sError> {
        let nodes_api: Api<Node> = Api::all(client.clone());
        match nodes_api.list(&ListParams::default()).await {
            Ok(nodes) => Ok(Some(nodes.items)),
            Err(kube::Error::Api(status)) if status.is_forbidden() => {
                if !self.refused.nodes {
                    tracing::warn!(
                        "no permission to list nodes of {}: showing its namespaces only",
                        self.target.name
                    );
                    self.refused.nodes = true;
                }
                Ok(None)
            }
            Err(source) => Err(K8sError::ListNodes(Box::new(source))),
        }
    }

    /// One report per listing, in listing order. Forgets namespaces no
    /// longer watched and refreshes Job owners first, so hidden cron job
    /// runs are matched.
    async fn namespace_reports(
        &mut self,
        client: &Client,
        listings: Vec<NamespaceListing>,
        figures: &HostFigures,
        nodes: &[NodeMetrics],
    ) -> Vec<NamespaceMetrics> {
        self.namespaces
            .retain(|name, _| listings.iter().any(|listing| listing.name == *name));
        self.resolve_jobs(client, &listings).await;
        listings
            .into_iter()
            .map(|listing| self.namespace_metrics(listing, figures, nodes))
            .collect()
    }

    /// Requests across the cluster, unless that was refused before.
    async fn read_requests(&self, client: &Client) -> RequestList {
        if self.refused.requests {
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
                if !self.refused.requests {
                    tracing::warn!(
                        "no permission to list pods in all namespaces of {}: \
                         requested capacity is not shown",
                        self.target.name
                    );
                    self.refused.requests = true;
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
        figures: &HostFigures,
        nodes: &[NodeMetrics],
    ) -> NamespaceMetrics {
        let NamespaceListing { name, pods, events } = listing;
        let pods = match pods {
            Ok((samples, specs)) => {
                let figures = figures.namespaces.get(&name);
                Some(self.pod_cards(&name, &samples, &specs, figures, &events, nodes))
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
        figures: Option<&NamespaceFigures>,
        events: &HashMap<String, String>,
        nodes: &[NodeMetrics],
    ) -> Vec<PodMetrics> {
        let none = NamespaceFigures::default();
        let figures = figures.unwrap_or(&none);
        let by_claim;
        let pvc = match &figures.pvc {
            PvcUsage::ByPod(pvc) => pvc,
            PvcUsage::ByClaim(claims) => {
                by_claim = pvc_by_pod(specs, claims);
                &by_claim
            }
        };
        let unhidden = BTreeSet::new();
        let hidden = self
            .target
            .namespace(namespace)
            .map_or(&unhidden, |ns| &ns.hidden);
        let state = self.namespaces.entry(namespace.to_string()).or_default();

        assemble_pods(
            samples,
            specs,
            &PodContext {
                nodes,
                pvc,
                net: &figures.pod_net,
                events,
                hidden,
                job_owners: &state.job_owners,
            },
        )
    }
}

/// Build a `kube::Client` for `context` of the kubeconfig at `path` (the
/// default one when `None`). Exec credential plugins run non-interactively
/// with the login-shell PATH (see `exec_auth`).
async fn connect(path: Option<PathBuf>, context: &str) -> Result<Client, K8sError> {
    let kubeconfig = load_kubeconfig(path).await?;
    build_client(kubeconfig, context).await
}

/// [`connect`], with `login_path` as the PATH of exec plugins.
#[cfg(test)]
async fn connect_with(
    path: Option<PathBuf>,
    context: &str,
    login_path: Option<&str>,
) -> Result<Client, K8sError> {
    let kubeconfig = load_kubeconfig(path).await?;
    build_client_with(kubeconfig, context, login_path).await
}

/// The kubeconfig at `path`, or the default one when `None`.
async fn load_kubeconfig(path: Option<PathBuf>) -> Result<Kubeconfig, K8sError> {
    tokio::task::spawn_blocking(move || kubeconfig::load(path.as_deref()))
        .await
        .map_err(K8sError::KubeconfigTask)?
}

/// Build a client for `context`. The login shell runs for PATH only when
/// the context's user has an exec plugin.
async fn build_client(kubeconfig: Kubeconfig, context: &str) -> Result<Client, K8sError> {
    let login_path = if exec_auth::uses_plugin(&kubeconfig, context) {
        exec_auth::login_path().await
    } else {
        None
    };
    build_client_with(kubeconfig, context, login_path).await
}

/// [`build_client`], with `login_path` as the PATH of exec plugins.
async fn build_client_with(
    mut kubeconfig: Kubeconfig,
    context: &str,
    login_path: Option<&str>,
) -> Result<Client, K8sError> {
    exec_auth::prepare_for_polling(&mut kubeconfig, login_path);
    let mut config = kubeconfig::build_config(kubeconfig, context).await?;
    config.connect_timeout = Some(KUBE_CONNECT_TIMEOUT);
    config.read_timeout = Some(KUBE_READ_TIMEOUT);

    // Building the client runs an exec plugin once, synchronously, so it
    // goes to the blocking pool (kube runs later refreshes there itself).
    tokio::task::spawn_blocking(move || Client::try_from(config))
        .await
        .map_err(K8sError::KubeconfigTask)?
        .map_err(|source| exec_auth::plugin_failure(K8sError::CreateClient(Box::new(source))))
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
        .map_err(|source| exec_auth::plugin_failure(K8sError::ListNamespaces(Box::new(source))))?;
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
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::config::{K8sTarget, PrometheusService, WatchedNamespace};
    use crate::metrics::Capacity;

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

    /// kube builds its rustls config with `ClientConfig::builder()`, which
    /// panics when no process default provider is installed and rustls is
    /// built with two (kube without its `aws-lc-rs` feature keeps `ring` next
    /// to reqwest's aws-lc-rs). Nothing fails at build time, so this builds a
    /// real client. `Client::try_from` creates the TLS config without
    /// connecting, so no server is needed.
    #[tokio::test]
    async fn client_builds_with_one_crypto_provider() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("kubeconfig.yaml");
        let kubeconfig = r#"
apiVersion: v1
kind: Config
clusters:
- name: "local"
  cluster:
    server: "https://127.0.0.1:1"
users:
- name: "local"
  user:
    token: "test"
contexts:
- name: "local"
  context:
    user: "local"
    cluster: "local"
current-context: "local"
"#;
        std::fs::write(&path, kubeconfig).expect("write test kubeconfig");

        connect(Some(path), "local").await.expect("client builds");
    }

    /// The request lines a stub API server has seen.
    type Seen = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    /// A plain-HTTP stand-in for an API server. `route` maps each request
    /// line to a status line and a JSON body. Returns its URL and the
    /// request lines it has seen.
    fn serve(route: impl Fn(&str) -> (String, String) + Send + 'static) -> (String, Seen) {
        use std::io::{BufRead, BufReader, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stub API");
        let url = format!("http://{}", listener.local_addr().expect("stub address"));
        let seen = Seen::default();
        let log = std::sync::Arc::clone(&seen);
        // Detached: it serves until the test process exits.
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(&stream);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                let mut header = String::new();
                while reader.read_line(&mut header).is_ok_and(|n| n > 2) {
                    header.clear();
                }
                let (status, body) = route(&request_line);
                log.lock()
                    .expect("stub log lock")
                    .push(request_line.trim_end().to_string());
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = (&stream).write_all(response.as_bytes());
            }
        });
        (url, seen)
    }

    /// A failure as the API server sends it: kube reads the code from the
    /// body, not the status line.
    fn failure(status: &str) -> (String, String) {
        let code = &status[..3];
        let body = format!(
            r#"{{"kind":"Status","apiVersion":"v1","metadata":{{}},"status":"Failure","code":{code},"message":"refused"}}"#
        );
        (status.to_string(), body)
    }

    fn list(items: &str) -> (String, String) {
        let body =
            format!(r#"{{"kind":"List","apiVersion":"v1","metadata":{{}},"items":[{items}]}}"#);
        ("200 OK".to_string(), body)
    }

    /// A stub API server that answers the node list with `nodes` (an HTTP
    /// status line), lists namespaces `b` and `a`, and answers every other
    /// list with no items. Returns its URL.
    fn serve_api(nodes: &'static str) -> String {
        let (url, _) = serve(move |request| {
            if request.contains(" /api/v1/nodes") {
                failure(nodes)
            } else if request.contains(" /api/v1/namespaces?")
                || request.contains(" /api/v1/namespaces ")
            {
                list(r#"{"metadata":{"name":"b"}},{"metadata":{"name":"a"}}"#)
            } else {
                list("")
            }
        });
        url
    }

    /// A Prometheus answer: one series per `(labels, value)`.
    fn vector(series: &[(&str, &str)]) -> (String, String) {
        let result: Vec<String> = series
            .iter()
            .map(|(labels, value)| format!(r#"{{"metric":{{{labels}}},"value":[0,"{value}"]}}"#))
            .collect();
        let body = format!(
            r#"{{"status":"success","data":{{"resultType":"vector","result":[{}]}}}}"#,
            result.join(",")
        );
        ("200 OK".to_string(), body)
    }

    /// Answers for a Prometheus with node `n1` and pod `default/web`
    /// mounting claim `data`, plus series that must be ignored: a host
    /// outside the cluster, a deleted node and a pod in an unwatched
    /// namespace. Matched by the first metric name a query contains, so the
    /// disk-used query (size minus free) is matched by `free` first.
    fn prometheus_route(request: &str) -> (String, String) {
        let answers: [(&str, &[(&str, &str)]); 8] = [
            (
                "node_filesystem_free_bytes",
                &[(r#""node":"n1""#, "60"), (r#""node":"old-worker""#, "1")],
            ),
            (
                "node_filesystem_size_bytes",
                &[
                    (r#""node":"n1""#, "100"),
                    (r#""node":"old-worker""#, "500"),
                    ("", "5"),
                ],
            ),
            ("node_network_receive", &[(r#""node":"n1""#, "1000.4")]),
            ("node_network_transmit", &[(r#""node":"n1""#, "2000")]),
            (
                "container_network_receive",
                &[
                    (r#""namespace":"default","pod":"web""#, "10"),
                    (r#""namespace":"other","pod":"x""#, "99"),
                ],
            ),
            (
                "container_network_transmit",
                &[(r#""namespace":"default","pod":"web""#, "20")],
            ),
            (
                "kubelet_volume_stats_used_bytes",
                &[(
                    r#""namespace":"default","persistentvolumeclaim":"data""#,
                    "3",
                )],
            ),
            (
                "kubelet_volume_stats_capacity_bytes",
                &[(
                    r#""namespace":"default","persistentvolumeclaim":"data""#,
                    "30",
                )],
            ),
        ];
        answers
            .iter()
            .find(|(metric, _)| request.contains(metric))
            .map_or_else(|| vector(&[]), |(_, series)| vector(series))
    }

    fn prometheus_service() -> PrometheusService {
        "monitoring/prometheus-server:80"
            .parse()
            .expect("valid service")
    }

    #[tokio::test]
    async fn prometheus_figures_are_keyed_by_node_pod_and_claim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (url, seen) = serve(prometheus_route);
        let client = connect(Some(token_kubeconfig(&dir, &url)), "ctx")
            .await
            .expect("client builds");

        let reading = prometheus::host_figures(
            &client,
            &prometheus_service(),
            &["n1".to_string()],
            &["default".to_string()],
        )
        .await;

        assert!(reading.failure.is_none(), "{:?}", reading.failure);
        let figures = reading.figures;

        let disk = Capacity {
            used_bytes: 60,
            capacity_bytes: 100,
        };
        assert_eq!(
            figures.node_disks,
            HashMap::from([("n1".to_string(), disk)])
        );
        assert_eq!(
            figures.cluster_net,
            Some(NetRate {
                rx_bytes_per_sec: 1000,
                tx_bytes_per_sec: 2000,
            })
        );
        let default = &figures.namespaces["default"];
        let web = default.pod_net.get("web").copied();
        assert_eq!(
            web.map(|n| (n.rx_bytes_per_sec, n.tx_bytes_per_sec)),
            Some((10, 20))
        );
        assert_eq!(default.pod_net.len(), 1, "unwatched namespace left out");
        let PvcUsage::ByClaim(claims) = &default.pvc else {
            unreachable!("Prometheus reports usage per claim");
        };
        assert_eq!(claims["data"].capacity_bytes, 30);

        let seen = seen.lock().expect("stub log lock");
        assert!(
            seen.iter()
                .all(|r| r.contains("/namespaces/monitoring/services/prometheus-server:80/proxy/")),
            "{seen:?}"
        );
        // The pod and PVC queries name the watched namespace, URL-encoded
        // as `namespace=~` followed by a backtick-quoted `default`.
        let matcher = "namespace%3D%7E%60default%60";
        let scoped = seen.iter().filter(|r| r.contains(matcher)).count();
        assert_eq!(scoped, 4, "{seen:?}");
    }

    #[tokio::test]
    async fn a_rejected_query_loses_only_its_own_figures() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (url, _) = serve(|request| {
            if request.contains("kubelet_volume_stats") {
                let body = r#"{"status":"error","errorType":"bad_data","error":"parse error"}"#;
                ("400 Bad Request".to_string(), body.to_string())
            } else {
                prometheus_route(request)
            }
        });
        let client = connect(Some(token_kubeconfig(&dir, &url)), "ctx")
            .await
            .expect("client builds");

        let reading = prometheus::host_figures(
            &client,
            &prometheus_service(),
            &["n1".to_string()],
            &["default".to_string()],
        )
        .await;

        assert!(
            matches!(&reading.failure, Some(K8sError::QueryPrometheus { service, .. })
                if service == "monitoring/prometheus-server:80"),
            "{:?}",
            reading.failure
        );
        assert_eq!(reading.figures.node_disks.len(), 1, "node disks still read");
        let default = &reading.figures.namespaces["default"];
        assert_eq!(default.pod_net.len(), 1, "pod network still read");
        assert!(matches!(&default.pvc, PvcUsage::ByClaim(claims) if claims.is_empty()));
    }

    /// Pod `web` in `default`, mounting claim `data`.
    const WEB_POD: &str = r#"{"metadata":{"name":"web","namespace":"default"},
        "spec":{"containers":[{"name":"c"}],
            "volumes":[{"name":"d","persistentVolumeClaim":{"claimName":"data"}}]},
        "status":{"phase":"Running"}}"#;

    /// One node, `n1`, with allocatable capacity.
    const NODE_LIST: &str =
        r#"{"metadata":{"name":"n1"},"status":{"allocatable":{"cpu":"4","memory":"8Gi"}}}"#;

    #[tokio::test]
    async fn a_cluster_with_prometheus_takes_disk_and_network_from_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (url, seen) = serve(|request| {
            if request.contains("/proxy/api/v1/query") {
                prometheus_route(request)
            } else if request.contains(" /api/v1/nodes") {
                list(NODE_LIST)
            } else if request.contains(" /api/v1/namespaces/default/pods") {
                list(WEB_POD)
            } else {
                list("")
            }
        });
        let mut backend = backend();
        backend.target.kubeconfig = Some(token_kubeconfig(&dir, &url));
        backend.target.prometheus = Some(prometheus_service());

        let metrics = backend.collect().await.expect("collects");

        let capacity = metrics.capacity.expect("nodes are listed");
        assert_eq!(capacity.disk.capacity_bytes, 100, "only listed nodes count");
        assert!(
            (capacity.usage.usage.disk_percent - 60.0).abs() < 1e-9,
            "the disk ring"
        );
        assert_eq!(capacity.nodes[0].disk.map(|d| d.used_bytes), Some(60));
        assert_eq!(capacity.net.map(|n| n.tx_bytes_per_sec), Some(2000));
        // The claim reaches the pod through its spec.
        let pods = metrics.namespaces[0].pods.as_deref().expect("pods listed");
        assert_eq!(pods[0].pvc.map(|c| c.capacity_bytes), Some(30));
        assert_eq!(pods[0].net.map(|n| n.rx_bytes_per_sec), Some(10));
        let seen = seen.lock().expect("stub log lock");
        assert!(
            !seen.iter().any(|r| r.contains("/stats/summary")),
            "{seen:?}"
        );
    }

    #[tokio::test]
    async fn refused_kubelet_stats_are_not_requested_again_until_reconnect() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (url, seen) = serve(|request| {
            if request.contains("/stats/summary") {
                failure("403 Forbidden")
            } else if request.contains(" /api/v1/nodes") {
                list(NODE_LIST)
            } else {
                list("")
            }
        });
        let mut backend = backend();
        backend.target.kubeconfig = Some(token_kubeconfig(&dir, &url));

        for _ in 0..2 {
            let metrics = backend.collect().await.expect("collects without stats");
            assert!(
                metrics
                    .capacity
                    .is_some_and(|c| c.disk == Capacity::default())
            );
        }

        let stats_requests = || {
            let seen = seen.lock().expect("stub log lock");
            seen.iter().filter(|r| r.contains("/stats/summary")).count()
        };
        assert_eq!(stats_requests(), 1);

        // Setting Prometheus and clearing it again, as after `nodes/proxy`
        // was granted, tries kubelet stats again without a reconnect.
        let mut with_prometheus = backend.target().clone();
        with_prometheus.prometheus = Some(prometheus_service());
        backend.set_target(with_prometheus);
        let mut cleared = backend.target().clone();
        cleared.prometheus = None;
        backend.set_target(cleared);
        backend.collect().await.expect("collects");
        assert_eq!(stats_requests(), 2);
    }

    #[tokio::test]
    async fn kubelet_rates_restart_after_a_spell_on_prometheus() {
        // The first kubelet poll after Prometheus must not compute a rate
        // against counters from before it, which would average over the
        // whole Prometheus period.
        let dir = tempfile::tempdir().expect("tempdir");
        let (url, _) = serve(|request| {
            if request.contains("/stats/summary") {
                let body = r#"{"node":{"nodeName":"n1","network":{"rxBytes":100,"txBytes":100}}}"#;
                ("200 OK".to_string(), body.to_string())
            } else if request.contains("/proxy/api/v1/query") {
                prometheus_route(request)
            } else if request.contains(" /api/v1/nodes") {
                list(NODE_LIST)
            } else {
                list("")
            }
        });
        let mut backend = backend();
        backend.target.kubeconfig = Some(token_kubeconfig(&dir, &url));
        let net = |metrics: ClusterMetrics| metrics.capacity.and_then(|c| c.net);

        let kubelet = backend.target().clone();
        assert_eq!(
            net(backend.collect().await.expect("kubelet poll")),
            None,
            "one sample"
        );
        let steady = backend.collect().await.expect("kubelet poll");
        assert_eq!(
            net(steady).map(|n| n.rx_bytes_per_sec),
            Some(0),
            "a second kubelet sample gives a rate"
        );
        let mut prometheus = kubelet.clone();
        prometheus.prometheus = Some(prometheus_service());
        backend.set_target(prometheus);
        let on_prometheus = backend.collect().await.expect("Prometheus poll");
        assert!(net(on_prometheus).is_some(), "Prometheus has rates at once");
        backend.set_target(kubelet);
        let back = backend.collect().await.expect("kubelet poll");

        assert_eq!(net(back), None, "one kubelet sample has no rate yet");

        // Pod counters go too, not only the cluster's.
        let start = Instant::now();
        let sample = NetSample {
            rx_bytes: 1,
            tx_bytes: 1,
            at: start,
        };
        let state = backend.namespaces.entry("default".to_string()).or_default();
        state.prev_pod_net.insert("web".to_string(), sample);
        let reading = prometheus::Reading {
            figures: HostFigures::default(),
            failure: None,
        };
        backend.host_figures(HostSource::Prometheus(reading), &["default".to_string()]);
        assert!(backend.namespaces["default"].prev_pod_net.is_empty());
    }

    #[tokio::test]
    async fn no_nodes_is_no_refusal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (url, _) = serve(|_| list(""));
        let client = connect(Some(token_kubeconfig(&dir, &url)), "ctx")
            .await
            .expect("client builds");

        let stats = fetch_all_node_stats(&client, &[]).await;

        assert!(
            matches!(stats, StatsList::Summaries(ref s) if s.is_empty()),
            "{stats:?}"
        );
    }

    #[tokio::test]
    async fn stats_are_retried_when_a_refusal_comes_with_other_failures() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (url, seen) = serve(|request| {
            if request.contains("/nodes/n1/proxy/stats/summary") {
                failure("403 Forbidden")
            } else if request.contains("/stats/summary") {
                failure("500 Internal Server Error")
            } else if request.contains(" /api/v1/nodes") {
                list(&format!("{NODE_LIST},{}", NODE_LIST.replace("n1", "n2")))
            } else {
                list("")
            }
        });
        let mut backend = backend();
        backend.target.kubeconfig = Some(token_kubeconfig(&dir, &url));

        for _ in 0..2 {
            backend.collect().await.expect("collects without stats");
        }

        let seen = seen.lock().expect("stub log lock");
        let stats_requests = seen.iter().filter(|r| r.contains("/stats/summary")).count();
        assert_eq!(stats_requests, 4, "a down node is not a refusal: {seen:?}");
    }

    #[tokio::test]
    async fn expired_credentials_take_a_cluster_without_node_access_offline() {
        // With nodes forbidden every other call only degrades, so caching
        // the refusal would leave an expired session online with stale
        // pods, never offering a login.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let expired = dir.path().join("expired");
        let plugin = dir.path().join("plugin");
        // The token is already expired, so kube runs the plugin before
        // every request.
        let script = format!(
            "#!/bin/sh\nif [ -e '{}' ]; then echo 'session expired' >&2; exit 1; fi\n\
             printf '%s' '{{\"apiVersion\":\"client.authentication.k8s.io/v1\",\
             \"kind\":\"ExecCredential\",\"status\":{{\"token\":\"t\",\
             \"expirationTimestamp\":\"2000-01-01T00:00:00Z\"}}}}'\n",
            expired.display()
        );
        std::fs::write(&plugin, script).expect("write plugin");
        std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755))
            .expect("make plugin executable");
        let kubeconfig = dir.path().join("kubeconfig.yaml");
        let yaml = format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    \
             server: \"{}\"\nusers:\n- name: u\n  user:\n    exec:\n      \
             apiVersion: client.authentication.k8s.io/v1\n      command: \"{}\"\n\
             contexts:\n- name: ctx\n  context: {{cluster: c, user: u}}\n",
            serve_api("403 Forbidden"),
            plugin.display()
        );
        std::fs::write(&kubeconfig, yaml).expect("write kubeconfig");
        let mut backend = backend();
        backend.target.kubeconfig = Some(kubeconfig);

        let metrics = backend.collect().await.expect("namespaces only");
        assert!(backend.uses_credential_plugin());
        assert!(metrics.capacity.is_none());
        assert_eq!(metrics.namespaces[0].pods.as_deref(), Some(&[][..]));

        std::fs::write(&expired, "").expect("expire the session");
        let err = backend.collect().await.expect_err("credentials expired");
        assert!(err.needs_login(), "{}", crate::error::error_chain(&err));
    }

    /// A kubeconfig in `dir` whose context `ctx` reaches `server` with a
    /// static token.
    fn token_kubeconfig(dir: &tempfile::TempDir, server: &str) -> PathBuf {
        let path = dir.path().join("kubeconfig.yaml");
        let yaml = format!(
            "clusters:\n- name: c\n  cluster: {{server: \"{server}\"}}\n\
             users:\n- name: u\n  user: {{token: t}}\n\
             contexts:\n- name: ctx\n  context: {{cluster: c, user: u}}\n"
        );
        std::fs::write(&path, yaml).expect("write kubeconfig");
        path
    }

    #[tokio::test]
    async fn a_node_list_failure_other_than_a_refusal_takes_the_cluster_offline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut backend = backend();
        backend.target.kubeconfig = Some(token_kubeconfig(
            &dir,
            &serve_api("500 Internal Server Error"),
        ));

        let err = backend.collect().await.expect_err("nodes failed");

        assert!(matches!(err, K8sError::ListNodes(_)), "{err:?}");
        assert!(!backend.uses_credential_plugin());
    }

    #[tokio::test]
    async fn namespaces_are_listed_sorted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kubeconfig = token_kubeconfig(&dir, &serve_api("403 Forbidden"));

        let names = list_namespaces(Some(kubeconfig), "ctx")
            .await
            .expect("namespaces");

        assert_eq!(names, ["a", "b"]);
    }

    #[tokio::test]
    async fn namespace_reports_keep_the_state_of_watched_namespaces_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kubeconfig = token_kubeconfig(&dir, "https://127.0.0.1:1");
        let client = connect(Some(kubeconfig), "ctx")
            .await
            .expect("client builds");
        let mut backend = backend();
        for name in ["default", "unwatched"] {
            let state = backend.namespaces.entry(name.to_string()).or_default();
            state.jobs_forbidden = true;
        }
        let listing = NamespaceListing {
            name: "default".to_string(),
            pods: Ok((Vec::new(), Vec::new())),
            events: HashMap::new(),
        };

        let reports = backend
            .namespace_reports(&client, vec![listing], &HostFigures::default(), &[])
            .await;

        assert_eq!(reports.len(), 1);
        let kept: Vec<&String> = backend.namespaces.keys().collect();
        assert_eq!(kept, ["default"]);
        assert!(backend.namespaces["default"].jobs_forbidden, "state kept");
    }

    fn backend() -> K8sBackend {
        K8sBackend::new(K8sTarget {
            prometheus: None,
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
        assert!(!backend.refused.requests);

        assert_eq!(backend.requested(RequestList::Failed, &allocatable), None);
        assert!(!backend.refused.requests, "a failure is retried");

        assert_eq!(
            backend.requested(RequestList::Forbidden, &allocatable),
            None
        );
        assert!(backend.refused.requests, "a refusal is not");
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

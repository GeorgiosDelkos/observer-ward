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
mod jobs;
mod kubeconfig;
mod metrics_api;
mod nodes;
mod pods;
mod quantity;
mod requests;
mod stats;

pub(crate) use error::K8sError;
pub(crate) use exec_auth::{login, stderr_summary};
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
    /// Listing nodes was refused and that was logged. Unlike the flags
    /// above, the node list is still requested every poll: with nothing
    /// else required, it is what notices expired credentials or an
    /// unreachable cluster, since failed namespace listings only degrade.
    nodes_forbidden: bool,
    /// The context's user runs a kubeconfig exec credential plugin. Set
    /// when connecting, before the plugin first runs, so a poll that times
    /// out while the plugin waits can still be told apart.
    credential_plugin: bool,
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
            nodes_forbidden: false,
            credential_plugin: false,
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
            self.requests_forbidden = false;
            self.nodes_forbidden = false;
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
            let listings = fetch_namespaces(client, &watched).await;
            let namespaces = self.namespace_reports(client, listings, &[], &[]).await;
            return Ok(ClusterMetrics {
                capacity: None,
                namespaces,
            });
        };

        let (stats, node_usage, requests, listings) = tokio::join!(
            fetch_all_node_stats(client, &nodes),
            fetch_node_usage(client),
            self.read_requests(client),
            fetch_namespaces(client, &watched),
        );
        let nodes = node_metrics(&nodes, &node_usage?, &node_disks(&stats));
        let totals = node_totals(&nodes);
        let namespaces = self
            .namespace_reports(client, listings, &stats, &nodes)
            .await;

        let fs_net = cluster_totals(&stats);
        let capacity = ClusterCapacity {
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
        };
        Ok(ClusterMetrics {
            capacity: Some(capacity),
            namespaces,
        })
    }

    /// The cluster's nodes, or `None` when listing them is refused. A
    /// refusal is logged once per client.
    async fn read_nodes(&mut self, client: &Client) -> Result<Option<Vec<Node>>, K8sError> {
        let nodes_api: Api<Node> = Api::all(client.clone());
        match nodes_api.list(&ListParams::default()).await {
            Ok(nodes) => Ok(Some(nodes.items)),
            Err(kube::Error::Api(status)) if status.is_forbidden() => {
                if !self.nodes_forbidden {
                    tracing::warn!(
                        "no permission to list nodes of {}: showing its namespaces only",
                        self.target.name
                    );
                    self.nodes_forbidden = true;
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
        stats: &[StatsSummary],
        nodes: &[NodeMetrics],
    ) -> Vec<NamespaceMetrics> {
        self.namespaces
            .retain(|name, _| listings.iter().any(|listing| listing.name == *name));
        self.resolve_jobs(client, &listings).await;
        listings
            .into_iter()
            .map(|listing| self.namespace_metrics(listing, stats, nodes))
            .collect()
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

    /// A plain-HTTP stand-in for an API server that answers the node list
    /// with `nodes` (an HTTP status line), lists namespaces `b` and `a`, and
    /// answers every other list with no items. Returns its URL.
    fn serve_api(nodes: &'static str) -> String {
        use std::io::{BufRead, BufReader, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind stub API");
        let url = format!("http://{}", listener.local_addr().expect("stub address"));
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
                // kube reads the failure's code from the body, not the status line.
                let (status, body) = if request_line.contains(" /api/v1/nodes") {
                    let code = &nodes[..3];
                    let body = format!(
                        r#"{{"kind":"Status","apiVersion":"v1","metadata":{{}},"status":"Failure","code":{code},"message":"nodes refused"}}"#
                    );
                    (nodes, body)
                } else if request_line.contains(" /api/v1/namespaces?")
                    || request_line.contains(" /api/v1/namespaces ")
                {
                    let body = r#"{"kind":"List","apiVersion":"v1","metadata":{},"items":[{"metadata":{"name":"b"}},{"metadata":{"name":"a"}}]}"#;
                    ("200 OK", body.to_string())
                } else {
                    let body = r#"{"kind":"List","apiVersion":"v1","metadata":{},"items":[]}"#;
                    ("200 OK", body.to_string())
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = (&stream).write_all(response.as_bytes());
            }
        });
        url
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
            .namespace_reports(&client, vec![listing], &[], &[])
            .await;

        assert_eq!(reports.len(), 1);
        let kept: Vec<&String> = backend.namespaces.keys().collect();
        assert_eq!(kept, ["default"]);
        assert!(backend.namespaces["default"].jobs_forbidden, "state kept");
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

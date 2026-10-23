//! Per-pod metric assembly from Metrics API samples and pod specs.

use std::collections::{BTreeSet, HashMap, HashSet};

use k8s_openapi::api::core::v1::{Pod, PodStatus as K8sPodStatus, ResourceRequirements};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;

use crate::config::WorkloadRef;
use crate::metrics::{
    Bound, Capacity, Ceiling, CeilingSource, NetSample, NodeMetrics, PodMetrics, PodStatus,
    PodUsage, Resources, Usage,
};

use super::error::K8sError;
use super::jobs::{CRON_JOB, JOB, JobOwners};
use super::metrics_api::PodMetrics as PodMetricsSample;
use super::nodes::node_totals;
use super::quantity::{parse_cpu_quantity, parse_memory_quantity};

/// Cluster-wide inputs shared by every pod card of one poll.
pub(super) struct PodContext<'a> {
    /// The cluster's node reports. A pod without a limit may use whatever
    /// its node has free, so its node's allocatable is the ceiling it is
    /// measured against.
    pub(super) nodes: &'a [NodeMetrics],
    pub(super) pvc: &'a HashMap<String, Capacity>,
    pub(super) net: &'a HashMap<String, NetSample>,
    pub(super) prev_net: &'a HashMap<String, NetSample>,
    pub(super) events: &'a HashMap<String, String>,
    /// Workloads whose pods get no card, and so no further work.
    pub(super) hidden: &'a BTreeSet<WorkloadRef>,
    pub(super) job_owners: &'a JobOwners,
}

/// One card per pod in the pod listing, with usage when metrics-server has
/// a sample for it. Pending or crash-looping pods without a sample still get
/// a card so they are not invisible. Succeeded pods and pods of hidden
/// workloads get none.
///
/// A sample whose pod is not in the listing is ignored: both lists come
/// from one poll, so that pod is already gone, and with no spec its owner
/// is unknown, so a hidden workload's pod would come back unhideable.
pub(super) fn assemble_pods(
    samples: &[PodMetricsSample],
    specs: &[Pod],
    ctx: &PodContext<'_>,
) -> Vec<PodMetrics> {
    let sample_by_name: HashMap<&str, &PodMetricsSample> = samples
        .iter()
        .filter_map(|s| s.metadata.name.as_deref().map(|n| (n, s)))
        .collect();

    let mut pods: Vec<PodMetrics> = specs
        .iter()
        .filter_map(|spec| {
            let name = spec.metadata.name.as_deref()?;
            if derive_pod_status(spec) == Some(PodStatus::Succeeded) {
                return None;
            }
            let workload = pod_workload(name, spec, ctx.job_owners);
            if is_hidden(&workload, ctx) {
                return None;
            }
            let usage = sample_by_name.get(name).and_then(|sample| {
                sample_usage(name, sample, spec, ctx)
                    .inspect_err(|e| {
                        tracing::warn!(
                            "ignoring metrics for pod {name}: {}",
                            crate::error::error_chain(e)
                        );
                    })
                    .ok()
            });
            Some(pod_card(name, spec, workload, usage, ctx))
        })
        .collect();

    pods.sort_by(|a, b| a.name.cmp(&b.name));
    pods
}

/// Whether `workload` is hidden. A Job not resolved to its cron job (the
/// Job list failed or is not allowed) is also matched by its name, which
/// the cron job controller makes `<cron job>-<digits>`, so a hidden cron
/// job stays hidden while its Jobs cannot be read.
fn is_hidden(workload: &WorkloadRef, ctx: &PodContext<'_>) -> bool {
    if ctx.hidden.contains(workload) {
        return true;
    }
    if workload.kind != JOB || ctx.job_owners.contains_key(&workload.name) {
        return false;
    }
    cron_job_by_name(&workload.name).is_some_and(|cron_job| {
        ctx.hidden.contains(&WorkloadRef {
            kind: CRON_JOB.to_string(),
            name: cron_job.to_string(),
        })
    })
}

/// The cron job a Job named `<cron job>-<digits>` would belong to.
fn cron_job_by_name(job: &str) -> Option<&str> {
    let (cron_job, run) = job.rsplit_once('-')?;
    let is_run = !run.is_empty() && run.bytes().all(|b| b.is_ascii_digit());
    (is_run && !cron_job.is_empty()).then_some(cron_job)
}

fn pod_card(
    name: &str,
    spec: &Pod,
    workload: WorkloadRef,
    usage: Option<PodUsage>,
    ctx: &PodContext<'_>,
) -> PodMetrics {
    let net = match (ctx.net.get(name), ctx.prev_net.get(name)) {
        (Some(now), Some(prev)) => now.rate_since(prev),
        (Some(_) | None, None) | (None, Some(_)) => None,
    };
    PodMetrics {
        name: name.to_string(),
        workload,
        status: derive_pod_status(spec),
        restart_count: pod_restart_count(spec),
        start_time: pod_start_time(spec),
        last_event: ctx.events.get(name).cloned(),
        node: spec.spec.as_ref().and_then(|s| s.node_name.clone()),
        usage,
        net,
        pvc: ctx.pvc.get(name).copied(),
    }
}

/// Usage of one pod from its metrics-server sample, as a share of its
/// [`pod_ceiling`].
fn sample_usage(
    name: &str,
    sample: &PodMetricsSample,
    spec: &Pod,
    ctx: &PodContext<'_>,
) -> Result<PodUsage, K8sError> {
    let mut used = Resources::default();
    for container in &sample.containers {
        used.add(Resources {
            cpu_millicores: parse_cpu_quantity(&container.usage.cpu)? * 1000.0,
            memory_bytes: parse_memory_quantity(&container.usage.memory)?,
        });
    }

    let ceiling = pod_ceiling(spec, ctx.nodes);
    let disk = ctx.pvc.get(name);

    Ok(PodUsage {
        usage: Usage {
            disk_percent: disk.map_or(0.0, Capacity::percent),
            ..used.share_of(&ceiling.resources())
        },
        used,
        ceiling,
    })
}

/// What `pod`'s usage is measured against, per resource: its limit, else
/// its node's allocatable.
///
/// Requests are never the ceiling. A request only reserves room on the
/// node; a pod without a limit may run past it, so usage against a request
/// reads over 100% for a pod that is merely busy.
fn pod_ceiling(pod: &Pod, nodes: &[NodeMetrics]) -> Ceiling {
    let limits = pod_limits(pod);
    // The node is looked up only for a resource without a limit.
    let node = || node_allocatable(pod, nodes);
    Ceiling {
        cpu: limits.cpu_millicores.map_or_else(
            || {
                let (node, source) = node();
                Bound {
                    amount: node.cpu_millicores,
                    source,
                }
            },
            Bound::limit,
        ),
        memory: limits.memory_bytes.map_or_else(
            || {
                let (node, source) = node();
                Bound {
                    amount: node.memory_bytes,
                    source,
                }
            },
            Bound::limit,
        ),
    }
}

/// Allocatable of the node `pod` runs on, else of the whole cluster: a
/// node missing from this poll's listing, or with a malformed allocatable
/// (logged by `node_metrics`), must not cost the pod its usage. The
/// cluster is a ceiling the pod cannot reach, so the fallback understates
/// usage and never raises a false warning.
fn node_allocatable(pod: &Pod, nodes: &[NodeMetrics]) -> (Resources, CeilingSource) {
    pod.spec
        .as_ref()
        .and_then(|s| s.node_name.as_deref())
        .and_then(|node| nodes.iter().find(|n| n.name == node)?.allocatable)
        .map_or_else(
            || (node_totals(nodes).allocatable, CeilingSource::Cluster),
            |node| (node, CeilingSource::Node),
        )
}

/// Kind of a pod without a controller, which is its own workload.
const POD: &str = "Pod";
const REPLICA_SET: &str = "ReplicaSet";

/// Controllers that make a new replica set on every rollout, named
/// `<owner>-<hash>`, with the hash in a pod label: the label, then the
/// owner's kind. Argo Rollouts uses its own label.
const REPLICA_SET_OWNERS: [(&str, &str); 2] = [
    ("pod-template-hash", "Deployment"),
    ("rollouts-pod-template-hash", "Rollout"),
];

/// The workload that owns `pod`: its controller, with a replica set
/// resolved to the Deployment (or Argo Rollout) that made it, and a Job
/// to its cron job when `job_owners` knows one. A pod without a controller
/// is its own workload.
pub(super) fn pod_workload(name: &str, pod: &Pod, job_owners: &JobOwners) -> WorkloadRef {
    let Some(owner) = controller(pod) else {
        return WorkloadRef {
            kind: POD.to_string(),
            name: name.to_string(),
        };
    };
    if owner.kind == JOB
        && let Some(cron_job) = job_owners.get(&owner.name).and_then(Option::as_ref)
    {
        return WorkloadRef {
            kind: CRON_JOB.to_string(),
            name: cron_job.clone(),
        };
    }

    let labels = pod.metadata.labels.as_ref();
    let rollout_owner = REPLICA_SET_OWNERS
        .iter()
        .filter(|_| owner.kind == REPLICA_SET)
        .find_map(|&(label, kind)| {
            let hash = labels?.get(label)?;
            let name = replica_set_owner(&owner.name, hash)?;
            Some(WorkloadRef {
                kind: kind.to_string(),
                name: name.to_string(),
            })
        });
    rollout_owner.unwrap_or_else(|| WorkloadRef {
        kind: owner.kind.clone(),
        name: owner.name.clone(),
    })
}

/// The Jobs controlling a pod in `specs`. Succeeded pods count too: one
/// still sampled by metrics-server gets a card.
pub(super) fn jobs_of(specs: &[Pod]) -> HashSet<&str> {
    specs
        .iter()
        .filter_map(controller)
        .filter(|owner| owner.kind == JOB)
        .map(|owner| owner.name.as_str())
        .collect()
}

fn controller(pod: &Pod) -> Option<&OwnerReference> {
    pod.metadata
        .owner_references
        .as_deref()?
        .iter()
        .find(|o| o.controller == Some(true))
}

/// The owner that made `replica_set`, named `<owner>-<template_hash>`.
/// `None` for a replica set not named that way (one created by hand or
/// by another controller), which is then shown as itself.
fn replica_set_owner<'a>(replica_set: &'a str, template_hash: &str) -> Option<&'a str> {
    if template_hash.is_empty() {
        return None;
    }
    replica_set
        .strip_suffix(template_hash)?
        .strip_suffix('-')
        .filter(|name| !name.is_empty())
}

/// Sum restart counts across all containers in a pod.
pub(super) fn pod_restart_count(pod: &Pod) -> u32 {
    let statuses = pod
        .status
        .as_ref()
        .and_then(|s| s.container_statuses.as_deref())
        .unwrap_or_default();

    // Saturating: a plain `sum` panics on overflow in debug builds, and a
    // negative count from a misbehaving API server clamps to zero.
    statuses.iter().fold(0_u32, |total, cs| {
        total.saturating_add(u32::try_from(cs.restart_count).unwrap_or(0))
    })
}

/// The pod start time as RFC 3339 with second precision.
pub(super) fn pod_start_time(pod: &Pod) -> Option<String> {
    let start = pod.status.as_ref()?.start_time.as_ref()?;
    k8s_openapi::jiff::fmt::strtime::format("%Y-%m-%dT%H:%M:%SZ", start.0).ok()
}

/// The pod's status for display. Container-level reasons such as
/// `CrashLoopBackOff` override the phase since they are more specific.
pub(super) fn derive_pod_status(pod: &Pod) -> Option<PodStatus> {
    let status = pod.status.as_ref()?;
    container_override(status).or_else(|| status.phase.as_deref().map(phase))
}

fn phase(phase: &str) -> PodStatus {
    match phase {
        "Pending" => PodStatus::Pending,
        "Running" => PodStatus::Running,
        "Succeeded" => PodStatus::Succeeded,
        "Failed" => PodStatus::Failed,
        _ => PodStatus::Unknown,
    }
}

/// A waiting/terminated container reason that should override the phase.
fn container_override(status: &K8sPodStatus) -> Option<PodStatus> {
    status
        .container_statuses
        .iter()
        .flatten()
        .filter_map(|cs| cs.state.as_ref())
        .find_map(|state| {
            let waiting = state.waiting.as_ref().and_then(|w| w.reason.as_deref());
            let terminated = state.terminated.as_ref().and_then(|t| t.reason.as_deref());
            match (waiting, terminated) {
                (Some("CrashLoopBackOff"), _) => Some(PodStatus::CrashLoopBackOff),
                (Some("ImagePullBackOff"), _) => Some(PodStatus::ImagePullBackOff),
                (Some("ErrImagePull"), _) => Some(PodStatus::ErrImagePull),
                (_, Some("OOMKilled")) => Some(PodStatus::OomKilled),
                _ => None,
            }
        })
}

/// A pod's CPU and memory limits. `None` where the pod has no limit.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Limits {
    cpu_millicores: Option<f64>,
    memory_bytes: Option<u64>,
}

impl Limits {
    /// No containers yet: a zero budget that each one adds to.
    const ZERO: Self = Self {
        cpu_millicores: Some(0.0),
        memory_bytes: Some(0),
    };

    /// Both budgets together. An unlimited side leaves the sum unlimited.
    fn add(self, other: Self) -> Self {
        Self {
            cpu_millicores: zip_with(self.cpu_millicores, other.cpu_millicores, |a, b| a + b),
            memory_bytes: zip_with(self.memory_bytes, other.memory_bytes, u64::saturating_add),
        }
    }

    /// The larger budget. An unlimited side leaves the result unlimited.
    fn max(self, other: Self) -> Self {
        Self {
            cpu_millicores: zip_with(self.cpu_millicores, other.cpu_millicores, f64::max),
            memory_bytes: zip_with(self.memory_bytes, other.memory_bytes, u64::max),
        }
    }
}

fn zip_with<T>(a: Option<T>, b: Option<T>, f: impl FnOnce(T, T) -> T) -> Option<T> {
    a.zip(b).map(|(a, b)| f(a, b))
}

/// A pod's limits, computed as Kubernetes' `PodLimits`
/// (`k8s.io/component-helpers/resource`) does, which is what the kubelet
/// caps the pod's cgroup at:
///
/// - App containers and native sidecars (init containers that restart
///   always) run together, so their limits add up.
/// - Init containers run one at a time, in order, each alongside the
///   sidecars started before it; the Metrics API reports a running init
///   container's usage with the pod's. The pod may use the largest step.
///
/// The ceiling is the larger of the two. A resource is `None` unless
/// *every* container is limited: an unlimited one may use the whole node.
/// Unlike `PodLimits`, the pod overhead is not added: metrics-server
/// reports container usage only, which the overhead never holds.
/// A pod-level limit (`spec.resources`, beta in Kubernetes 1.34) replaces
/// the containers' for its resource, as Kubernetes applies it.
fn pod_limits(pod: &Pod) -> Limits {
    let Some(spec) = &pod.spec else {
        return Limits::default();
    };
    if spec.containers.is_empty() {
        return Limits::default();
    }

    let mut running = spec
        .containers
        .iter()
        .map(|c| limits_of(c.resources.as_ref()))
        .fold(Limits::ZERO, Limits::add);
    let mut sidecars = Limits::ZERO;
    let mut init_peak = Limits::ZERO;
    for init in spec.init_containers.iter().flatten() {
        let limits = limits_of(init.resources.as_ref());
        let step = if init.restart_policy.as_deref() == Some("Always") {
            running = running.add(limits);
            sidecars = sidecars.add(limits);
            sidecars
        } else {
            sidecars.add(limits)
        };
        init_peak = init_peak.max(step);
    }
    let containers = running.max(init_peak);

    let pod_level = limits_of(spec.resources.as_ref());
    Limits {
        cpu_millicores: pod_level.cpu_millicores.or(containers.cpu_millicores),
        memory_bytes: pod_level.memory_bytes.or(containers.memory_bytes),
    }
}

/// The limits of a container, or of a whole pod. A request is deliberately
/// no fallback: it reserves room, it does not cap usage. A zero limit is no
/// limit, as the kubelet treats it.
fn limits_of(resources: Option<&ResourceRequirements>) -> Limits {
    let limits = resources.and_then(|r| r.limits.as_ref());
    let limit = |resource: &str| limits.and_then(|l| l.get(resource));
    Limits {
        cpu_millicores: limit("cpu")
            .and_then(|q| parse_logged(q, parse_cpu_quantity))
            .filter(|&cores| cores > 0.0)
            .map(|cores| cores * 1000.0),
        memory_bytes: limit("memory")
            .and_then(|q| parse_logged(q, parse_memory_quantity))
            .filter(|&bytes| bytes > 0),
    }
}

fn parse_logged<T>(
    quantity: &Quantity,
    parse: fn(&Quantity) -> Result<T, super::error::QuantityParseError>,
) -> Option<T> {
    parse(quantity)
        .inspect_err(|e| {
            tracing::warn!("ignoring limit: {}", crate::error::error_chain(e));
        })
        .ok()
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::time::Instant;

    use k8s_openapi::api::core::v1::{
        Container, ContainerState, ContainerStateTerminated, ContainerStateWaiting,
        ContainerStatus, PodSpec,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{OwnerReference, Time};
    use kube::api::ObjectMeta;

    use crate::k8s::metrics_api::{ContainerMetrics, ContainerMetricsUsage};

    fn q(s: &str) -> Quantity {
        Quantity(s.to_string())
    }

    fn assert_f64_near(left: f64, right: f64, epsilon: f64) {
        assert!(
            (left - right).abs() < epsilon,
            "expected ~{right}, got {left}"
        );
    }

    fn named_pod(name: &str) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                ..ObjectMeta::default()
            },
            ..Pod::default()
        }
    }

    fn with_status(mut pod: Pod, status: K8sPodStatus) -> Pod {
        pod.status = Some(status);
        pod
    }

    fn make_pod_with_restarts(name: &str, restarts: &[i32]) -> Pod {
        let statuses = restarts
            .iter()
            .map(|&r| ContainerStatus {
                restart_count: r,
                ..ContainerStatus::default()
            })
            .collect();
        with_status(
            named_pod(name),
            K8sPodStatus {
                container_statuses: Some(statuses),
                ..K8sPodStatus::default()
            },
        )
    }

    fn with_container_state(phase: &str, state: ContainerState) -> Pod {
        with_status(
            named_pod("p"),
            K8sPodStatus {
                phase: Some(phase.to_string()),
                container_statuses: Some(vec![ContainerStatus {
                    state: Some(state),
                    ..ContainerStatus::default()
                }]),
                ..K8sPodStatus::default()
            },
        )
    }

    struct ContainerRes {
        cpu_request: Option<&'static str>,
        mem_request: Option<&'static str>,
        cpu_limit: Option<&'static str>,
        mem_limit: Option<&'static str>,
    }

    const NO_CAPS: ContainerRes = ContainerRes {
        cpu_request: None,
        mem_request: None,
        cpu_limit: None,
        mem_limit: None,
    };

    fn qty_map(cpu: Option<&str>, mem: Option<&str>) -> Option<BTreeMap<String, Quantity>> {
        let mut map = BTreeMap::new();
        if let Some(cpu) = cpu {
            map.insert("cpu".to_string(), q(cpu));
        }
        if let Some(mem) = mem {
            map.insert("memory".to_string(), q(mem));
        }
        if map.is_empty() { None } else { Some(map) }
    }

    fn make_pod_with_resources(name: &str, containers: &[ContainerRes]) -> Pod {
        let containers = containers
            .iter()
            .map(|c| Container {
                name: "app".to_string(),
                resources: Some(ResourceRequirements {
                    requests: qty_map(c.cpu_request, c.mem_request),
                    limits: qty_map(c.cpu_limit, c.mem_limit),
                    ..ResourceRequirements::default()
                }),
                ..Container::default()
            })
            .collect();
        Pod {
            spec: Some(PodSpec {
                containers,
                ..PodSpec::default()
            }),
            ..named_pod(name)
        }
    }

    /// `pod` scheduled on `node`.
    fn on_node(mut pod: Pod, node: &str) -> Pod {
        pod.spec.get_or_insert_default().node_name = Some(node.to_string());
        pod
    }

    fn node(name: &str, allocatable: Option<Resources>) -> NodeMetrics {
        NodeMetrics {
            name: name.to_string(),
            roles: Vec::new(),
            ready: true,
            unschedulable: false,
            problems: Vec::new(),
            kubelet_version: None,
            allocatable,
            usage: None,
            disk: None,
        }
    }

    /// The allocatable of [`NODE`], the node every [`Maps::empty`] has.
    const NODE_ALLOCATABLE: Resources = Resources {
        cpu_millicores: 16_000.0,
        memory_bytes: 64 * 1024 * 1024 * 1024,
    };
    const NODE: &str = "node-a";

    fn sample(name: &str, cpu: &str, memory: &str) -> PodMetricsSample {
        PodMetricsSample {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                ..ObjectMeta::default()
            },
            containers: vec![ContainerMetrics {
                usage: ContainerMetricsUsage {
                    cpu: q(cpu),
                    memory: q(memory),
                },
            }],
        }
    }

    struct Maps {
        pvc: HashMap<String, Capacity>,
        net: HashMap<String, NetSample>,
        prev_net: HashMap<String, NetSample>,
        events: HashMap<String, String>,
        hidden: BTreeSet<WorkloadRef>,
        job_owners: JobOwners,
        nodes: Vec<NodeMetrics>,
    }

    impl Maps {
        fn empty() -> Self {
            Self {
                pvc: HashMap::new(),
                net: HashMap::new(),
                prev_net: HashMap::new(),
                events: HashMap::new(),
                hidden: BTreeSet::new(),
                job_owners: JobOwners::new(),
                nodes: vec![node(NODE, Some(NODE_ALLOCATABLE))],
            }
        }

        fn ctx(&self) -> PodContext<'_> {
            PodContext {
                nodes: &self.nodes,
                pvc: &self.pvc,
                net: &self.net,
                prev_net: &self.prev_net,
                events: &self.events,
                hidden: &self.hidden,
                job_owners: &self.job_owners,
            }
        }
    }

    const GI: u64 = 1024 * 1024 * 1024;

    fn init_container(restart_policy: Option<&str>, cpu_limit: Option<&str>) -> Container {
        Container {
            name: "init".to_string(),
            restart_policy: restart_policy.map(str::to_string),
            resources: Some(ResourceRequirements {
                limits: qty_map(cpu_limit, cpu_limit.map(|_| "1Gi")),
                ..ResourceRequirements::default()
            }),
            ..Container::default()
        }
    }

    fn limited_app(name: &str) -> Pod {
        make_pod_with_resources(
            name,
            &[ContainerRes {
                cpu_limit: Some("1"),
                mem_limit: Some("1Gi"),
                ..NO_CAPS
            }],
        )
    }

    #[test]
    fn pod_limits_ignore_requests() {
        let pod = make_pod_with_resources(
            "hcfs-server",
            &[ContainerRes {
                cpu_request: Some("100m"),
                mem_request: Some("128Mi"),
                cpu_limit: Some("1"),
                mem_limit: Some("1Gi"),
            }],
        );
        let limits = pod_limits(&pod);
        assert_f64_near(limits.cpu_millicores.unwrap(), 1000.0, 1e-9);
        assert_eq!(limits.memory_bytes, Some(GI));

        let requests_only = make_pod_with_resources(
            "worker",
            &[ContainerRes {
                cpu_request: Some("250m"),
                mem_request: Some("256Mi"),
                ..NO_CAPS
            }],
        );
        assert_eq!(pod_limits(&requests_only), Limits::default());
    }

    #[test]
    fn pod_limits_cpu_and_memory_are_independent() {
        let pod = make_pod_with_resources(
            "mixed",
            &[ContainerRes {
                cpu_request: Some("100m"),
                mem_request: Some("512Mi"),
                cpu_limit: Some("2"),
                mem_limit: None,
            }],
        );
        let limits = pod_limits(&pod);
        assert_f64_near(limits.cpu_millicores.unwrap(), 2000.0, 1e-9);
        assert_eq!(limits.memory_bytes, None);
    }

    #[test]
    fn pod_limits_need_every_container_limited() {
        // A sidecar without limits: summing only the app's 1 core would make
        // the whole pod's usage look like a share of the app alone.
        let pod = make_pod_with_resources(
            "with-sidecar",
            &[
                ContainerRes {
                    cpu_limit: Some("1"),
                    mem_limit: Some("1Gi"),
                    ..NO_CAPS
                },
                NO_CAPS,
            ],
        );
        assert_eq!(pod_limits(&pod), Limits::default());
        assert_eq!(pod_limits(&named_pod("no-spec")), Limits::default());
    }

    #[test]
    fn pod_limits_add_native_sidecars_to_the_app() {
        let mut pod = limited_app("with-native-sidecar");
        pod.spec.as_mut().unwrap().init_containers = Some(vec![
            init_container(None, Some("100m")),
            init_container(Some("Always"), Some("500m")),
        ]);
        let limits = pod_limits(&pod);
        assert_f64_near(limits.cpu_millicores.unwrap(), 1500.0, 1e-9);
        assert_eq!(limits.memory_bytes, Some(2 * GI));
    }

    #[test]
    fn pod_limits_cover_the_largest_init_step() {
        // A 4-core migration runs before the app's 1 core and beside the
        // 500m sidecar started first: while it runs, the pod may use 4.5.
        let mut pod = limited_app("migrating");
        pod.spec.as_mut().unwrap().init_containers = Some(vec![
            init_container(Some("Always"), Some("500m")),
            init_container(None, Some("4")),
        ]);
        let limits = pod_limits(&pod);
        assert_f64_near(limits.cpu_millicores.unwrap(), 4500.0, 1e-9);
        assert_eq!(limits.memory_bytes, Some(2 * GI));
    }

    #[test]
    fn pod_limits_are_unlimited_when_any_container_is() {
        for init in [
            init_container(Some("Always"), None),
            init_container(None, None),
            init_container(None, Some("0")),
        ] {
            let mut pod = limited_app("unlimited-init");
            pod.spec.as_mut().unwrap().init_containers = Some(vec![init]);
            assert_eq!(pod_limits(&pod).cpu_millicores, None);
        }

        let zero = make_pod_with_resources(
            "zero",
            &[ContainerRes {
                cpu_limit: Some("0"),
                mem_limit: Some("0"),
                ..NO_CAPS
            }],
        );
        assert_eq!(pod_limits(&zero), Limits::default(), "zero is no limit");
    }

    #[test]
    fn a_pod_level_limit_replaces_the_containers_for_its_resource() {
        // Pod-level CPU caps an otherwise unlimited sidecar; memory keeps
        // the containers' sum.
        let mut pod = make_pod_with_resources(
            "pod-level",
            &[
                ContainerRes {
                    cpu_limit: Some("1"),
                    mem_limit: Some("1Gi"),
                    ..NO_CAPS
                },
                ContainerRes {
                    mem_limit: Some("1Gi"),
                    ..NO_CAPS
                },
            ],
        );
        pod.spec.as_mut().unwrap().resources = Some(ResourceRequirements {
            limits: qty_map(Some("3"), None),
            ..ResourceRequirements::default()
        });

        let limits = pod_limits(&pod);
        assert_f64_near(limits.cpu_millicores.unwrap(), 3000.0, 1e-9);
        assert_eq!(limits.memory_bytes, Some(2 * GI));

        // Over fully limited containers, the pod-level limit still wins.
        let mut pod = limited_app("pod-level-over-limits");
        pod.spec.as_mut().unwrap().resources = Some(ResourceRequirements {
            limits: qty_map(Some("500m"), Some("3Gi")),
            ..ResourceRequirements::default()
        });
        let limits = pod_limits(&pod);
        assert_f64_near(limits.cpu_millicores.unwrap(), 500.0, 1e-9);
        assert_eq!(limits.memory_bytes, Some(3 * GI));
    }

    #[test]
    fn pod_limits_accept_canonical_milli_byte_limits() {
        let pod = make_pod_with_resources(
            "canonical",
            &[ContainerRes {
                mem_limit: Some("1288490188800m"),
                ..NO_CAPS
            }],
        );
        assert_eq!(pod_limits(&pod).memory_bytes, Some(1_288_490_189));
    }

    #[test]
    fn usage_without_a_limit_is_a_share_of_its_node_not_its_request() {
        // hcfs-server: a 1-core request and no CPU limit, bursting to 1.6
        // cores. Against the request that read 160%; the node has room.
        let spec = on_node(
            make_pod_with_resources(
                "hcfs-server",
                &[ContainerRes {
                    cpu_request: Some("1"),
                    mem_request: Some("2Gi"),
                    mem_limit: Some("8Gi"),
                    ..NO_CAPS
                }],
            ),
            NODE,
        );
        let maps = Maps::empty();

        let usage = sample_usage(
            "hcfs-server",
            &sample("hcfs-server", "1600m", "2Gi"),
            &spec,
            &maps.ctx(),
        )
        .expect("usage");

        assert_f64_near(usage.usage.cpu_percent, 10.0, 0.01);
        assert_f64_near(usage.usage.memory_percent, 25.0, 0.01);
        assert_eq!(
            usage.ceiling,
            Ceiling {
                cpu: Bound {
                    amount: 16_000.0,
                    source: CeilingSource::Node,
                },
                memory: Bound::limit(8 * GI),
            }
        );

        // The UI reads the ceiling beside the flattened usage.
        let json = serde_json::to_value(usage).expect("json");
        assert_eq!(json["cpu_percent"], 10.0);
        assert_eq!(json["ceiling"]["cpu"]["amount"], 16_000.0);
        assert_eq!(json["ceiling"]["cpu"]["source"], "node");
        assert_eq!(json["ceiling"]["memory"]["amount"], 8 * GI);
        assert_eq!(json["ceiling"]["memory"]["source"], "limit");
    }

    #[test]
    fn usage_falls_back_to_the_cluster_when_the_node_is_unknown() {
        let mut maps = Maps::empty();
        maps.nodes.push(node("node-b", None));
        maps.nodes.push(node("node-c", Some(NODE_ALLOCATABLE)));
        let spec = make_pod_with_resources(
            "worker",
            &[ContainerRes {
                cpu_limit: Some("4"),
                ..NO_CAPS
            }],
        );
        let sampled = sample("worker", "1600m", "8Gi");

        // Unscheduled, on a node without allocatable, on a node gone from the
        // listing: CPU keeps its limit, memory is a share of both nodes.
        for spec in [
            spec.clone(),
            on_node(spec.clone(), "node-b"),
            on_node(spec, "gone"),
        ] {
            let usage = sample_usage("worker", &sampled, &spec, &maps.ctx()).expect("usage");
            assert_f64_near(usage.usage.cpu_percent, 40.0, 0.01);
            assert_f64_near(usage.usage.memory_percent, 6.25, 0.01);
            assert_eq!(usage.used.memory_bytes, 8 * GI);
            assert_eq!(usage.ceiling.cpu, Bound::limit(4000.0));
            assert_eq!(usage.ceiling.memory.source, CeilingSource::Cluster);
        }
    }

    #[test]
    fn usage_is_a_share_of_limits_not_small_requests() {
        let spec = make_pod_with_resources(
            "hcfs-server",
            &[ContainerRes {
                cpu_request: Some("100m"),
                mem_request: Some("128Mi"),
                cpu_limit: Some("1"),
                mem_limit: Some("1Gi"),
            }],
        );
        let maps = Maps::empty();

        // 90m / 1 core = 9%, 64Mi / 1Gi = 6.25%. Against requests these
        // would look like 90% and 50%.
        let usage = sample_usage(
            "hcfs-server",
            &sample("hcfs-server", "90m", "64Mi"),
            &spec,
            &maps.ctx(),
        )
        .expect("usage");

        assert_f64_near(usage.usage.cpu_percent, 9.0, 0.01);
        assert_f64_near(usage.usage.memory_percent, 6.25, 0.01);
        assert_f64_near(usage.used.cpu_millicores, 90.0, 1e-9);
    }

    #[test]
    fn assemble_pods_keeps_unsampled_pods_and_skips_succeeded() {
        let pending = with_status(
            named_pod("pending"),
            K8sPodStatus {
                phase: Some("Pending".to_string()),
                ..K8sPodStatus::default()
            },
        );
        let done = with_status(
            named_pod("done"),
            K8sPodStatus {
                phase: Some("Succeeded".to_string()),
                ..K8sPodStatus::default()
            },
        );
        let web = on_node(named_pod("web"), NODE);
        let mut maps = Maps::empty();
        maps.events.insert(
            "pending".to_string(),
            "Warning: FailedScheduling".to_string(),
        );

        let pods = assemble_pods(
            &[sample("web", "10m", "1Mi")],
            &[pending, done, web],
            &maps.ctx(),
        );

        let names: Vec<&str> = pods.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["pending", "web"]);
        assert!(pods[0].usage.is_none());
        assert_eq!(pods[0].status, Some(PodStatus::Pending));
        assert_eq!(
            pods[0].last_event.as_deref(),
            Some("Warning: FailedScheduling")
        );
        assert!(pods[1].usage.is_some());
    }

    #[test]
    fn assemble_pods_rates_network_from_two_samples() {
        let start = Instant::now();
        let mut maps = Maps::empty();
        maps.prev_net.insert(
            "web".to_string(),
            NetSample {
                rx_bytes: 0,
                tx_bytes: 0,
                at: start,
            },
        );
        maps.net.insert(
            "web".to_string(),
            NetSample {
                rx_bytes: 2_000,
                tx_bytes: 1_000,
                at: start + std::time::Duration::from_secs(1),
            },
        );

        let pods = assemble_pods(
            &[sample("web", "1m", "1Mi")],
            &[named_pod("web")],
            &maps.ctx(),
        );

        let net = pods[0].net.expect("rate from two samples");
        assert_eq!((net.rx_bytes_per_sec, net.tx_bytes_per_sec), (2_000, 1_000));
    }

    fn owned_pod(kind: &str, owner: &str, hash_label: Option<(&str, &str)>) -> Pod {
        let mut pod = named_pod("p");
        pod.metadata.owner_references = Some(vec![
            OwnerReference {
                kind: "Node".to_string(),
                name: "not-the-controller".to_string(),
                ..OwnerReference::default()
            },
            OwnerReference {
                kind: kind.to_string(),
                name: owner.to_string(),
                controller: Some(true),
                ..OwnerReference::default()
            },
        ]);
        pod.metadata.labels =
            hash_label.map(|(label, hash)| BTreeMap::from([(label.to_string(), hash.to_string())]));
        pod
    }

    fn workload(kind: &str, name: &str) -> WorkloadRef {
        WorkloadRef {
            kind: kind.to_string(),
            name: name.to_string(),
        }
    }

    #[test]
    fn workload_resolves_replica_sets_to_their_rollout_owner() {
        let pod = owned_pod(
            REPLICA_SET,
            "api-7d9f8c6b5",
            Some(("pod-template-hash", "7d9f8c6b5")),
        );
        assert_eq!(
            pod_workload("api-7d9f8c6b5-x2k4p", &pod, &JobOwners::new()),
            workload("Deployment", "api")
        );

        let argo = owned_pod(
            REPLICA_SET,
            "web-6c8d7f",
            Some(("rollouts-pod-template-hash", "6c8d7f")),
        );
        assert_eq!(
            pod_workload("web-6c8d7f-abcde", &argo, &JobOwners::new()),
            workload("Rollout", "web")
        );

        let unrelated = owned_pod(
            REPLICA_SET,
            "manual-rs",
            Some(("pod-template-hash", "7d9f8c6b5")),
        );
        assert_eq!(
            pod_workload("manual-rs-abcde", &unrelated, &JobOwners::new()),
            workload(REPLICA_SET, "manual-rs")
        );
    }

    #[test]
    fn workload_is_the_controller_or_the_pod_itself() {
        let pod = owned_pod("StatefulSet", "db", None);
        assert_eq!(
            pod_workload("db-0", &pod, &JobOwners::new()),
            workload("StatefulSet", "db")
        );

        // A matching hash label only resolves replica sets.
        let job = owned_pod(
            "Job",
            "backup-29000000",
            Some(("pod-template-hash", "29000000")),
        );
        assert_eq!(
            pod_workload("backup-29000000-abcde", &job, &JobOwners::new()),
            workload("Job", "backup-29000000")
        );

        assert_eq!(
            pod_workload("debug", &named_pod("debug"), &JobOwners::new()),
            workload(POD, "debug")
        );
    }

    #[test]
    fn jobs_resolve_to_their_cron_job_when_known() {
        let run = owned_pod(JOB, "backup-29000000", None);
        let cron_job = workload(CRON_JOB, "backup");
        let owners = JobOwners::from([
            ("backup-29000000".to_string(), Some("backup".to_string())),
            ("manual".to_string(), None),
        ]);

        assert_eq!(pod_workload("backup-29000000-x", &run, &owners), cron_job);
        let manual = owned_pod(JOB, "manual", None);
        assert_eq!(
            pod_workload("manual-x", &manual, &owners),
            workload(JOB, "manual")
        );
    }

    fn succeeded(pod: Pod) -> Pod {
        with_status(
            pod,
            K8sPodStatus {
                phase: Some("Succeeded".to_string()),
                ..K8sPodStatus::default()
            },
        )
    }

    #[test]
    fn jobs_of_every_job_owned_pod_are_looked_up() {
        let specs = [
            owned_pod(JOB, "nightly-1", None),
            succeeded(owned_pod(JOB, "nightly-0", None)),
            owned_pod(REPLICA_SET, "api-1", None),
        ];

        assert_eq!(jobs_of(&specs), HashSet::from(["nightly-1", "nightly-0"]));
    }

    #[test]
    fn hidden_cron_jobs_get_no_card_even_when_succeeded_and_sampled() {
        let mut running = owned_pod(JOB, "backup-2", None);
        running.metadata.name = Some("backup-2-x".to_string());
        let mut done = succeeded(owned_pod(JOB, "backup-1", None));
        done.metadata.name = Some("backup-1-x".to_string());
        let mut maps = Maps::empty();
        maps.hidden.insert(workload(CRON_JOB, "backup"));
        for job in ["backup-1", "backup-2"] {
            maps.job_owners
                .insert(job.to_string(), Some("backup".to_string()));
        }

        let pods = assemble_pods(
            &[sample("backup-1-x", "1m", "1Mi")],
            &[running, done],
            &maps.ctx(),
        );

        assert!(pods.is_empty(), "{pods:?}");
    }

    #[test]
    fn hidden_workloads_get_no_card_with_or_without_a_sample() {
        let mut api_sampled = owned_pod(
            REPLICA_SET,
            "api-7d9f8c6b5",
            Some(("pod-template-hash", "7d9f8c6b5")),
        );
        api_sampled.metadata.name = Some("api-a".to_string());
        let mut api_pending = api_sampled.clone();
        api_pending.metadata.name = Some("api-b".to_string());
        let mut maps = Maps::empty();
        maps.hidden.insert(workload("Deployment", "api"));

        let pods = assemble_pods(
            &[
                sample("api-a", "10m", "1Mi"),
                sample("web", "10m", "1Mi"),
                sample("gone", "1m", "1Mi"),
            ],
            &[api_sampled, api_pending, on_node(named_pod("web"), NODE)],
            &maps.ctx(),
        );

        let names: Vec<&str> = pods.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["web"], "a sample whose pod is gone gets no card");
        assert!(pods[0].usage.is_some());
    }

    #[test]
    fn unresolved_runs_of_a_hidden_cron_job_stay_hidden() {
        let mut run = owned_pod(JOB, "backup-29000000", None);
        run.metadata.name = Some("backup-29000000-x".to_string());
        let mut manual = owned_pod(JOB, "migrate-2", None);
        manual.metadata.name = Some("migrate-2-x".to_string());
        let mut maps = Maps::empty();
        maps.hidden.insert(workload(CRON_JOB, "backup"));
        maps.hidden.insert(workload(CRON_JOB, "migrate"));
        // Listed and known to have no cron job: never matched by name.
        maps.job_owners.insert("migrate-2".to_string(), None);

        let pods = assemble_pods(&[], &[run, manual], &maps.ctx());

        let names: Vec<&str> = pods.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["migrate-2-x"]);
    }

    #[test]
    fn cron_job_by_name_needs_a_numeric_run_suffix() {
        assert_eq!(cron_job_by_name("backup-29000000"), Some("backup"));
        assert_eq!(cron_job_by_name("db-backup-1"), Some("db-backup"));
        assert_eq!(cron_job_by_name("backup-x1"), None);
        assert_eq!(cron_job_by_name("backup-"), None);
        assert_eq!(cron_job_by_name("-1"), None);
        assert_eq!(cron_job_by_name("backup"), None);
    }

    proptest::proptest! {
        #[test]
        fn replica_set_owner_inverts_the_controller_naming(
            deployment in "[a-z0-9]([-a-z0-9]{0,20}[a-z0-9])?",
            hash in "[a-z0-9]{1,10}",
        ) {
            let replica_set = format!("{deployment}-{hash}");
            proptest::prop_assert_eq!(
                replica_set_owner(&replica_set, &hash),
                Some(deployment.as_str())
            );
        }

        #[test]
        fn replica_set_owner_never_returns_an_empty_name(
            replica_set in "[-a-z0-9]{0,12}",
            hash in "[a-z0-9]{0,6}",
        ) {
            if let Some(name) = replica_set_owner(&replica_set, &hash) {
                proptest::prop_assert!(!name.is_empty());
                proptest::prop_assert_eq!(format!("{name}-{hash}"), replica_set);
            }
        }
    }

    #[test]
    fn restart_count_sums_containers() {
        let pod = make_pod_with_restarts("web", &[3, 1]);
        assert_eq!(pod_restart_count(&pod), 4);
    }

    #[test]
    fn restart_count_saturates_and_ignores_negative_counts() {
        let pod = make_pod_with_restarts("web", &[i32::MAX, i32::MAX, 5, -3]);
        assert_eq!(pod_restart_count(&pod), u32::MAX);

        let pod = make_pod_with_restarts("web", &[-3, 2]);
        assert_eq!(pod_restart_count(&pod), 2);
    }

    #[test]
    fn restart_count_zero_without_statuses() {
        assert_eq!(pod_restart_count(&named_pod("bare")), 0);
        let empty = with_status(named_pod("empty"), K8sPodStatus::default());
        assert_eq!(pod_restart_count(&empty), 0);
    }

    #[test]
    fn start_time_is_rfc3339_seconds() {
        let ts = k8s_openapi::jiff::Timestamp::from_second(1_700_000_000).unwrap();
        let pod = with_status(
            named_pod("timed"),
            K8sPodStatus {
                start_time: Some(Time(ts)),
                ..K8sPodStatus::default()
            },
        );
        assert_eq!(
            pod_start_time(&pod).as_deref(),
            Some("2023-11-14T22:13:20Z")
        );
        assert_eq!(pod_start_time(&named_pod("unstarted")), None);
    }

    #[test]
    fn status_comes_from_phase() {
        let pod = with_status(
            named_pod("run"),
            K8sPodStatus {
                phase: Some("Running".to_string()),
                ..K8sPodStatus::default()
            },
        );
        assert_eq!(derive_pod_status(&pod), Some(PodStatus::Running));
        assert_eq!(derive_pod_status(&named_pod("no-status")), None);

        let odd = with_status(
            named_pod("odd"),
            K8sPodStatus {
                phase: Some("Evicted?".to_string()),
                ..K8sPodStatus::default()
            },
        );
        assert_eq!(derive_pod_status(&odd), Some(PodStatus::Unknown));
    }

    #[test]
    fn container_reasons_override_phase() {
        let crash = with_container_state(
            "Running",
            ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some("CrashLoopBackOff".to_string()),
                    ..ContainerStateWaiting::default()
                }),
                ..ContainerState::default()
            },
        );
        assert_eq!(derive_pod_status(&crash), Some(PodStatus::CrashLoopBackOff));

        let oom = with_container_state(
            "Running",
            ContainerState {
                terminated: Some(ContainerStateTerminated {
                    reason: Some("OOMKilled".to_string()),
                    ..ContainerStateTerminated::default()
                }),
                ..ContainerState::default()
            },
        );
        assert_eq!(derive_pod_status(&oom), Some(PodStatus::OomKilled));

        let creating = with_container_state(
            "Pending",
            ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some("ContainerCreating".to_string()),
                    ..ContainerStateWaiting::default()
                }),
                ..ContainerState::default()
            },
        );
        assert_eq!(derive_pod_status(&creating), Some(PodStatus::Pending));
    }
}

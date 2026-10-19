//! Per-pod metric assembly from Metrics API samples and pod specs.

use std::collections::{BTreeSet, HashMap, HashSet};

use k8s_openapi::api::core::v1::{Container, Pod, PodStatus as K8sPodStatus};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;

use crate::config::WorkloadRef;
use crate::metrics::{Capacity, NetSample, PodMetrics, PodStatus, PodUsage, Usage, percent};

use super::error::K8sError;
use super::jobs::{CRON_JOB, JOB, JobOwners};
use super::metrics_api::PodMetrics as PodMetricsSample;
use super::quantity::{parse_cpu_quantity, parse_memory_quantity};

/// Cluster-wide inputs shared by every pod card of one poll.
pub(super) struct PodContext<'a> {
    /// Allocatable CPU (cores) and memory (bytes), the fallback base for
    /// a pod without limits or requests.
    pub(super) cluster_cpu: f64,
    pub(super) cluster_mem: u64,
    pub(super) pvc: &'a HashMap<String, Capacity>,
    pub(super) net: &'a HashMap<String, NetSample>,
    pub(super) prev_net: &'a HashMap<String, NetSample>,
    pub(super) events: &'a HashMap<String, String>,
    /// Workloads whose pods get no card, and so no further work.
    pub(super) hidden: &'a BTreeSet<WorkloadRef>,
    pub(super) job_owners: &'a JobOwners,
}

/// One card per pod in the namespace. Pods present in metrics-server get
/// usage; pods only in the spec list (Pending, crash-looping with no
/// samples) still get a card so they are not invisible. Succeeded pods
/// and pods of hidden workloads are skipped.
pub(super) fn assemble_pods(
    samples: &[PodMetricsSample],
    specs: &[Pod],
    ctx: &PodContext<'_>,
) -> Vec<PodMetrics> {
    let spec_by_name: HashMap<&str, (&Pod, WorkloadRef)> = specs
        .iter()
        .filter_map(|pod| {
            let name = pod.metadata.name.as_deref()?;
            Some((name, (pod, pod_workload(name, pod, ctx.job_owners))))
        })
        .collect();
    // A pod without a spec has no known workload, so it is never hidden.
    let hidden = |name: &str| {
        spec_by_name
            .get(name)
            .is_some_and(|(_, workload)| ctx.hidden.contains(workload))
    };

    let mut pods = Vec::with_capacity(samples.len().max(specs.len()));
    let mut seen = HashSet::new();

    for sample in samples {
        let Some(name) = sample.metadata.name.as_deref() else {
            continue;
        };
        if hidden(name) {
            continue;
        }
        let spec = spec_by_name
            .get(name)
            .map(|(pod, workload)| (*pod, workload));
        match sample_usage(name, sample, spec.map(|(pod, _)| pod), ctx) {
            Ok(usage) => {
                seen.insert(name);
                pods.push(pod_card(name, spec, Some(usage), ctx));
            }
            Err(e) => {
                tracing::warn!("skipping pod {name}: {}", crate::error::error_chain(&e));
            }
        }
    }

    for (&name, (spec, workload)) in &spec_by_name {
        if seen.contains(name)
            || hidden(name)
            || derive_pod_status(spec) == Some(PodStatus::Succeeded)
        {
            continue;
        }
        pods.push(pod_card(name, Some((spec, workload)), None, ctx));
    }

    pods.sort_by(|a, b| a.name.cmp(&b.name));
    pods
}

/// `spec` is the pod's spec and workload, when it was in the listing.
fn pod_card(
    name: &str,
    spec: Option<(&Pod, &WorkloadRef)>,
    usage: Option<PodUsage>,
    ctx: &PodContext<'_>,
) -> PodMetrics {
    let (workload, spec) = (spec.map(|(_, w)| w.clone()), spec.map(|(pod, _)| pod));
    let net = match (ctx.net.get(name), ctx.prev_net.get(name)) {
        (Some(now), Some(prev)) => now.rate_since(prev),
        (Some(_) | None, None) | (None, Some(_)) => None,
    };
    PodMetrics {
        name: name.to_string(),
        workload,
        status: spec.and_then(derive_pod_status),
        restart_count: spec.map_or(0, pod_restart_count),
        start_time: spec.and_then(pod_start_time),
        last_event: ctx.events.get(name).cloned(),
        usage,
        net,
        pvc: ctx.pvc.get(name).copied(),
    }
}

/// Usage of one pod from its metrics-server sample, as a share of its
/// limits (else requests, else the cluster's allocatable capacity).
fn sample_usage(
    name: &str,
    sample: &PodMetricsSample,
    spec: Option<&Pod>,
    ctx: &PodContext<'_>,
) -> Result<PodUsage, K8sError> {
    let mut cpu_used = 0.0_f64;
    let mut mem_used = 0_u64;
    for container in &sample.containers {
        cpu_used += parse_cpu_quantity(&container.usage.cpu)?;
        mem_used = mem_used.saturating_add(parse_memory_quantity(&container.usage.memory)?);
    }

    let (cpu_cap, mem_cap) = spec.map_or((None, None), pod_allocations);
    let cpu_base = cpu_cap.unwrap_or(ctx.cluster_cpu);
    let mem_base = mem_cap.unwrap_or(ctx.cluster_mem);
    let disk = ctx.pvc.get(name);

    #[expect(
        clippy::cast_precision_loss,
        reason = "byte counts fit comfortably in f64 for a percentage"
    )]
    let memory_percent = percent(mem_used as f64, mem_base as f64);

    Ok(PodUsage {
        usage: Usage {
            cpu_percent: percent(cpu_used, cpu_base),
            memory_percent,
            disk_percent: disk.map_or(0.0, Capacity::percent),
        },
        cpu_millicores: cpu_used * 1000.0,
        memory_bytes: mem_used,
    })
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

/// A pod's CPU (cores) and memory (bytes) caps: each container's limit,
/// else its request. A resource is `None` unless *every* container caps
/// it: summing only the capped containers would compare the whole pod's
/// usage against part of its budget and overstate the percentage.
pub(super) fn pod_allocations(pod: &Pod) -> (Option<f64>, Option<u64>) {
    let Some(spec) = &pod.spec else {
        return (None, None);
    };
    if spec.containers.is_empty() {
        return (None, None);
    }

    let mut cpu = Some(0.0_f64);
    let mut mem = Some(0_u64);
    for container in &spec.containers {
        let container_cpu =
            container_cap(container, "cpu").and_then(|q| parse_logged(q, parse_cpu_quantity));
        let container_mem =
            container_cap(container, "memory").and_then(|q| parse_logged(q, parse_memory_quantity));
        cpu = cpu.zip(container_cpu).map(|(total, c)| total + c);
        mem = mem
            .zip(container_mem)
            .map(|(total, m)| total.saturating_add(m));
    }
    (cpu, mem)
}

/// The container's limit for `resource`, else its request.
fn container_cap<'a>(container: &'a Container, resource: &str) -> Option<&'a Quantity> {
    let resources = container.resources.as_ref()?;
    resources
        .limits
        .as_ref()
        .and_then(|l| l.get(resource))
        .or_else(|| resources.requests.as_ref().and_then(|r| r.get(resource)))
}

fn parse_logged<T>(
    quantity: &Quantity,
    parse: fn(&Quantity) -> Result<T, super::error::QuantityParseError>,
) -> Option<T> {
    parse(quantity)
        .inspect_err(|e| tracing::warn!("ignoring container cap: {}", crate::error::error_chain(e)))
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
        ContainerState, ContainerStateTerminated, ContainerStateWaiting, ContainerStatus, PodSpec,
        ResourceRequirements,
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
            }
        }

        fn ctx(&self) -> PodContext<'_> {
            PodContext {
                cluster_cpu: 64.0,
                cluster_mem: 256 * 1024 * 1024 * 1024,
                pvc: &self.pvc,
                net: &self.net,
                prev_net: &self.prev_net,
                events: &self.events,
                hidden: &self.hidden,
                job_owners: &self.job_owners,
            }
        }
    }

    #[test]
    fn pod_allocations_prefers_limits_over_requests() {
        let pod = make_pod_with_resources(
            "hcfs-server",
            &[ContainerRes {
                cpu_request: Some("100m"),
                mem_request: Some("128Mi"),
                cpu_limit: Some("1"),
                mem_limit: Some("1Gi"),
            }],
        );
        let (cpu, mem) = pod_allocations(&pod);
        assert_f64_near(cpu.unwrap(), 1.0, 1e-9);
        assert_eq!(mem, Some(1024 * 1024 * 1024));
    }

    #[test]
    fn pod_allocations_uses_requests_when_limits_absent() {
        let pod = make_pod_with_resources(
            "worker",
            &[ContainerRes {
                cpu_request: Some("250m"),
                mem_request: Some("256Mi"),
                ..NO_CAPS
            }],
        );
        let (cpu, mem) = pod_allocations(&pod);
        assert_f64_near(cpu.unwrap(), 0.25, 1e-9);
        assert_eq!(mem, Some(256 * 1024 * 1024));
    }

    #[test]
    fn pod_allocations_cpu_and_memory_caps_are_independent() {
        let pod = make_pod_with_resources(
            "mixed",
            &[ContainerRes {
                cpu_request: Some("100m"),
                mem_request: Some("512Mi"),
                cpu_limit: Some("2"),
                mem_limit: None,
            }],
        );
        let (cpu, mem) = pod_allocations(&pod);
        assert_f64_near(cpu.unwrap(), 2.0, 1e-9);
        assert_eq!(mem, Some(512 * 1024 * 1024));
    }

    #[test]
    fn pod_allocations_need_every_container_capped() {
        // A sidecar without caps: summing only the app's 1 core would make
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
        assert_eq!(pod_allocations(&pod), (None, None));
        assert_eq!(pod_allocations(&named_pod("no-spec")), (None, None));
    }

    #[test]
    fn pod_allocations_accept_canonical_milli_byte_limits() {
        let pod = make_pod_with_resources(
            "canonical",
            &[ContainerRes {
                mem_limit: Some("1288490188800m"),
                ..NO_CAPS
            }],
        );
        assert_eq!(pod_allocations(&pod).1, Some(1_288_490_189));
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
            Some(&spec),
            &maps.ctx(),
        )
        .expect("usage");

        assert_f64_near(usage.usage.cpu_percent, 9.0, 0.01);
        assert_f64_near(usage.usage.memory_percent, 6.25, 0.01);
        assert_f64_near(usage.cpu_millicores, 90.0, 1e-9);
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
        let web = named_pod("web");
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

        let pods = assemble_pods(&[sample("web", "1m", "1Mi")], &[], &maps.ctx());

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
                sample("racing", "1m", "1Mi"),
            ],
            &[api_sampled, api_pending, named_pod("web")],
            &maps.ctx(),
        );

        let names: Vec<&str> = pods.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["racing", "web"], "a pod without a spec is kept");
        assert!(pods[0].workload.is_none());
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

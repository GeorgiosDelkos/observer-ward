//! CPU and memory reserved by pod requests across the whole cluster.
//!
//! The scheduler places a pod only where its requests fit in what is left
//! of a node's allocatable capacity, so this, not usage, says whether the
//! cluster has room. It needs every pod in every namespace, which a
//! namespace-scoped account may not be allowed to list: then the figure is
//! simply absent.

use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::core::v1::{Container, Pod};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::Client;
use kube::api::{Api, ListParams};

use super::CpuMem;
use super::error::QuantityParseError;
use super::quantity::{parse_cpu_quantity, parse_memory_quantity};

/// Time allowed for the cluster-wide pod list. The figure is optional, so
/// a slow list must not push the whole cluster poll past its timeout.
const REQUESTS_BUDGET: Duration = Duration::from_secs(8);

/// Pods that hold node resources: placed on a node and not finished. A
/// pending pod the scheduler could not place holds none, and counting it
/// would overstate a full cluster.
const HOLDING_PODS: &str = "spec.nodeName!=,status.phase!=Succeeded,status.phase!=Failed";

/// The outcome of summing requests across the cluster.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum RequestList {
    Requested(CpuMem),
    /// Listing pods in all namespaces is not allowed; asking again will not
    /// help until the credentials change.
    Forbidden,
    /// A transient failure or timeout, already logged; retry next poll.
    Failed,
}

/// Sum the requests of every pod holding node resources, within
/// [`REQUESTS_BUDGET`]. The list is served from the API server's watch
/// cache (`resourceVersion=0`), which may be slightly stale but spares
/// etcd a full read of every pod on each poll.
pub(super) async fn list_requests(client: &Client) -> RequestList {
    let api: Api<Pod> = Api::all(client.clone());
    let params = ListParams::default().fields(HOLDING_PODS).match_any();
    let Ok(listed) = tokio::time::timeout(REQUESTS_BUDGET, api.list(&params)).await else {
        tracing::warn!("listing pods for requests ran out of time; retrying next poll");
        return RequestList::Failed;
    };
    match listed {
        Ok(list) => RequestList::Requested(list.items.iter().map(pod_requests).sum()),
        Err(kube::Error::Api(status)) if status.is_forbidden() => RequestList::Forbidden,
        Err(e) => {
            tracing::warn!(
                "cannot list pods for requests, retrying next poll: {}",
                crate::error::error_chain(&e)
            );
            RequestList::Failed
        }
    }
}

/// What the scheduler reserves for `pod`, per resource, plus the pod
/// overhead. This follows Kubernetes' `PodRequests`
/// (`k8s.io/component-helpers/resource`):
///
/// - App containers and native sidecars (init containers that restart
///   always) run together, so their requests add up.
/// - Init containers run one at a time, in order, each alongside the
///   sidecars started before it. The pod needs room for the largest of
///   those steps.
///
/// The reservation is the larger of the two. A pod-level request
/// (`spec.resources`, beta in Kubernetes 1.34) replaces the containers'
/// for its resource.
fn pod_requests(pod: &Pod) -> CpuMem {
    let Some(spec) = &pod.spec else {
        return CpuMem::default();
    };

    let mut running: CpuMem = spec.containers.iter().map(container_requests).sum();
    let mut sidecars = CpuMem::default();
    let mut init_peak = CpuMem::default();
    for init in spec.init_containers.iter().flatten() {
        let requests = container_requests(init);
        let step = if init.restart_policy.as_deref() == Some("Always") {
            running = [running, requests].into_iter().sum();
            sidecars = [sidecars, requests].into_iter().sum();
            sidecars
        } else {
            [sidecars, requests].into_iter().sum()
        };
        init_peak = init_peak.max(step);
    }

    let containers = running.max(init_peak);

    let pod_level = spec.resources.as_ref().and_then(|r| r.requests.as_ref());
    let reserved = CpuMem {
        cpu: quantity(pod_level, "cpu", parse_cpu_quantity).unwrap_or(containers.cpu),
        mem: quantity(pod_level, "memory", parse_memory_quantity).unwrap_or(containers.mem),
    };
    [reserved, quantities(spec.overhead.as_ref())]
        .into_iter()
        .sum()
}

fn container_requests(container: &Container) -> CpuMem {
    quantities(
        container
            .resources
            .as_ref()
            .and_then(|r| r.requests.as_ref()),
    )
}

/// The `cpu` and `memory` entries of a resource map. A missing or
/// malformed one counts as zero, as the scheduler counts a missing one.
fn quantities(map: Option<&BTreeMap<String, Quantity>>) -> CpuMem {
    CpuMem {
        cpu: quantity(map, "cpu", parse_cpu_quantity).unwrap_or(0.0),
        mem: quantity(map, "memory", parse_memory_quantity).unwrap_or(0),
    }
}

/// The `resource` entry of a resource map, unless missing or malformed.
fn quantity<T>(
    map: Option<&BTreeMap<String, Quantity>>,
    resource: &str,
    parse: fn(&Quantity) -> Result<T, QuantityParseError>,
) -> Option<T> {
    map.and_then(|m| m.get(resource))
        .and_then(|q| parse(q).ok())
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::core::v1::{PodSpec, ResourceRequirements};

    use super::*;

    fn container(cpu: &str, mem: &str) -> Container {
        Container {
            resources: Some(ResourceRequirements {
                requests: Some(BTreeMap::from([
                    ("cpu".to_string(), Quantity(cpu.to_string())),
                    ("memory".to_string(), Quantity(mem.to_string())),
                ])),
                ..ResourceRequirements::default()
            }),
            ..Container::default()
        }
    }

    fn pod(containers: Vec<Container>, init: Vec<Container>) -> Pod {
        Pod {
            spec: Some(PodSpec {
                containers,
                init_containers: Some(init),
                ..PodSpec::default()
            }),
            ..Pod::default()
        }
    }

    #[test]
    fn running_containers_sum_and_init_containers_take_the_max() {
        let requests = pod_requests(&pod(
            vec![container("250m", "1Gi"), container("250m", "1Gi")],
            vec![container("1", "512Mi"), container("100m", "256Mi")],
        ));

        assert!((requests.cpu - 1.0).abs() < 1e-9, "init needs a whole core");
        assert_eq!(requests.mem, 2 * 1024 * 1024 * 1024, "containers need 2Gi");
    }

    fn sidecar(cpu: &str, mem: &str) -> Container {
        Container {
            restart_policy: Some("Always".to_string()),
            ..container(cpu, mem)
        }
    }

    #[test]
    fn native_sidecars_add_to_the_running_containers() {
        let requests = pod_requests(&pod(
            vec![container("250m", "1Gi")],
            vec![sidecar("100m", "256Mi"), container("100m", "128Mi")],
        ));

        assert!((requests.cpu - 0.35).abs() < 1e-9);
        assert_eq!(requests.mem, (1024 + 256) * 1024 * 1024);
    }

    #[test]
    fn an_init_container_runs_beside_the_sidecars_started_before_it() {
        // The second init container runs with the first sidecar (1.5 cores)
        // but not the second; the app and both sidecars need only 0.7.
        let requests = pod_requests(&pod(
            vec![container("100m", "64Mi")],
            vec![
                sidecar("500m", "64Mi"),
                container("1", "64Mi"),
                sidecar("100m", "64Mi"),
            ],
        ));

        assert!((requests.cpu - 1.5).abs() < 1e-9);
        assert_eq!(requests.mem, 3 * 64 * 1024 * 1024);
    }

    #[test]
    fn a_pod_level_request_replaces_the_containers_for_its_resource() {
        let mut pod = pod(vec![container("250m", "1Gi")], Vec::new());
        if let Some(spec) = pod.spec.as_mut() {
            spec.resources = Some(ResourceRequirements {
                requests: Some(BTreeMap::from([(
                    "cpu".to_string(),
                    Quantity("2".to_string()),
                )])),
                ..ResourceRequirements::default()
            });
        }

        let requests = pod_requests(&pod);

        assert!((requests.cpu - 2.0).abs() < 1e-9);
        assert_eq!(requests.mem, 1024 * 1024 * 1024);
    }

    #[test]
    fn overhead_adds_and_missing_requests_count_as_zero() {
        let mut with_overhead = pod(vec![Container::default()], Vec::new());
        if let Some(spec) = with_overhead.spec.as_mut() {
            spec.overhead = Some(BTreeMap::from([(
                "cpu".to_string(),
                Quantity("100m".to_string()),
            )]));
        }

        let requests = pod_requests(&with_overhead);

        assert!((requests.cpu - 0.1).abs() < 1e-9);
        assert_eq!(requests.mem, 0);
        assert_eq!(pod_requests(&Pod::default()), CpuMem::default());
    }
}

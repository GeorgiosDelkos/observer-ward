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

/// What the scheduler reserves for `pod`: per resource, the larger of its
/// containers' summed requests and its largest init container request,
/// plus the pod overhead. Restartable init containers (sidecars) are
/// counted like other init containers, so a pod with sidecars reads a
/// little low.
fn pod_requests(pod: &Pod) -> CpuMem {
    let Some(spec) = &pod.spec else {
        return CpuMem::default();
    };

    let running: CpuMem = spec.containers.iter().map(container_requests).sum();
    let init = spec
        .init_containers
        .iter()
        .flatten()
        .map(container_requests)
        .fold(CpuMem::default(), |max, c| CpuMem {
            cpu: max.cpu.max(c.cpu),
            mem: max.mem.max(c.mem),
        });
    let reserved = CpuMem {
        cpu: running.cpu.max(init.cpu),
        mem: running.mem.max(init.mem),
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
    let cpu = map
        .and_then(|m| m.get("cpu"))
        .and_then(|q| parse_cpu_quantity(q).ok())
        .unwrap_or(0.0);
    let mem = map
        .and_then(|m| m.get("memory"))
        .and_then(|q| parse_memory_quantity(q).ok())
        .unwrap_or(0);
    CpuMem { cpu, mem }
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

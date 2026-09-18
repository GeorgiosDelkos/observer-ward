//! Per-pod metric assembly from Metrics API samples and pod specs.

use std::collections::HashMap;
use std::time::Instant;

use k8s_openapi::api::core::v1::Pod;

use crate::metrics::{ServerMetrics, ServerStatus};

use super::error::K8sError;
use super::metrics_api::PodMetrics;
use super::quantity::{parse_cpu_quantity, parse_memory_quantity};

#[expect(
    clippy::cast_precision_loss,
    reason = "byte deltas fit comfortably in f64 for rate calc"
)]
pub(super) fn apply_pod_net_rates(
    results: &mut [ServerMetrics],
    net_map: &HashMap<String, (u64, u64)>,
    prev_pod_net: &HashMap<String, (u64, u64)>,
    prev_pod_poll_time: Option<Instant>,
) {
    let Some(prev_time) = prev_pod_poll_time else {
        return;
    };
    let elapsed = Instant::now().duration_since(prev_time).as_secs_f64();
    if elapsed <= 0.0 {
        return;
    }
    for m in results {
        let pod_name = m.server_name.split_once('/').map_or("", |(_, p)| p);
        let Some(&(curr_rx, curr_tx)) = net_map.get(pod_name) else {
            continue;
        };
        let Some(&(prev_rx, prev_tx)) = prev_pod_net.get(pod_name) else {
            continue;
        };
        let rx_rate = curr_rx.saturating_sub(prev_rx) as f64 / elapsed;
        let tx_rate = curr_tx.saturating_sub(prev_tx) as f64 / elapsed;
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "rates from byte deltas are always small positive"
        )]
        {
            m.net_rx_bytes_per_sec = rx_rate as u64;
            m.net_tx_bytes_per_sec = tx_rate as u64;
        }
    }
}

/// Cluster-level inputs for one pod card. Bundled so
/// `build_pod_server_metrics` stays within the positional-argument limit.
pub(super) struct PodMetricCtx<'a> {
    pub(super) cluster_name: &'a str,
    pub(super) cluster_cpu: f64,
    pub(super) cluster_mem: u64,
    pub(super) pvc_map: &'a HashMap<String, (u64, u64)>,
    pub(super) events: &'a HashMap<String, String>,
}

/// Build a `ServerMetrics` for one pod from its metrics and
/// spec data.
#[expect(
    clippy::cast_precision_loss,
    reason = "byte sums fit in f64 for percentage calculations"
)]
pub(super) fn build_pod_server_metrics(
    pm: &PodMetrics,
    pod_index: &HashMap<&str, &Pod>,
    ctx: &PodMetricCtx<'_>,
) -> Result<ServerMetrics, K8sError> {
    let pod_name = pm.metadata.name.as_deref().unwrap_or("unknown");
    let pod = pod_index.get(pod_name).copied();

    let mut cpu_used = 0.0_f64;
    let mut mem_used = 0_u64;

    for c in &pm.containers {
        cpu_used += parse_cpu_quantity(&c.usage.cpu)?;
        mem_used = mem_used.saturating_add(parse_memory_quantity(&c.usage.memory)?);
    }

    let (cpu_pct, mem_pct) =
        compute_pod_percentages(cpu_used, mem_used, pod, ctx.cluster_cpu, ctx.cluster_mem);

    let (pvc_used, pvc_cap) = ctx.pvc_map.get(pod_name).copied().unwrap_or((0, 0));
    let disk_pct = if pvc_cap > 0 {
        pvc_used as f64 / pvc_cap as f64 * 100.0
    } else {
        0.0
    };

    let last_event = ctx.events.get(pod_name).cloned().unwrap_or_default();

    Ok(ServerMetrics {
        server_name: format!("{}/{pod_name}", ctx.cluster_name),
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
/// container limits (then requests) over cluster allocatable.
#[expect(
    clippy::cast_precision_loss,
    reason = "byte sums fit in f64 for percentage calculations"
)]
pub(super) fn compute_pod_percentages(
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
pub(super) fn pod_restart_count(pod: Option<&Pod>) -> u32 {
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
pub(super) fn pod_start_time(pod: Option<&Pod>) -> String {
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
pub(super) fn derive_pod_status(pod: Option<&Pod>) -> String {
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
pub(super) fn container_override_reason(
    status: &k8s_openapi::api::core::v1::PodStatus,
) -> Option<String> {
    let statuses = status.container_statuses.as_ref()?;

    let override_waiting = ["CrashLoopBackOff", "ImagePullBackOff", "ErrImagePull"];
    let override_terminated = ["OOMKilled"];

    for cs in statuses {
        let Some(state) = &cs.state else { continue };

        if let Some(waiting) = &state.waiting
            && let Some(reason) = &waiting.reason
            && override_waiting.contains(&reason.as_str())
        {
            return Some(reason.clone());
        }

        if let Some(terminated) = &state.terminated
            && let Some(reason) = &terminated.reason
            && override_terminated.contains(&reason.as_str())
        {
            return Some(reason.clone());
        }
    }

    None
}

/// Sum a pod's container resource caps (CPU in fractional cores,
/// memory in bytes). Prefers `limits`; if a limit is unset, uses
/// the matching `requests` reservation. CPU and memory are chosen
/// independently. Returns `(0.0, 0)` when neither is set.
pub(super) fn pod_allocations(pod: Option<&Pod>) -> (f64, u64) {
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

        let cpu_q = res
            .limits
            .as_ref()
            .and_then(|l| l.get("cpu"))
            .or_else(|| res.requests.as_ref().and_then(|r| r.get("cpu")));
        let mem_q = res
            .limits
            .as_ref()
            .and_then(|l| l.get("memory"))
            .or_else(|| res.requests.as_ref().and_then(|r| r.get("memory")));

        if let Some(q) = cpu_q
            && let Ok(v) = parse_cpu_quantity(q)
        {
            total_cpu += v;
        }
        if let Some(q) = mem_q
            && let Ok(v) = parse_memory_quantity(q)
        {
            total_mem = total_mem.saturating_add(v);
        }
    }

    (total_cpu, total_mem)
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::*;

    use k8s_openapi::api::core::v1::Pod;
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
    use kube::api::ObjectMeta;

    fn q(s: &str) -> Quantity {
        Quantity(s.to_string())
    }

    fn assert_f64_near(left: f64, right: f64, epsilon: f64) {
        assert!(
            (left - right).abs() < epsilon,
            "expected ~{right}, got {left}"
        );
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

    struct ContainerRes {
        cpu_request: Option<&'static str>,
        mem_request: Option<&'static str>,
        cpu_limit: Option<&'static str>,
        mem_limit: Option<&'static str>,
    }

    fn qty_map(
        cpu: Option<&str>,
        mem: Option<&str>,
    ) -> Option<std::collections::BTreeMap<String, Quantity>> {
        let mut map = std::collections::BTreeMap::new();
        if let Some(cpu) = cpu {
            map.insert("cpu".to_string(), q(cpu));
        }
        if let Some(mem) = mem {
            map.insert("memory".to_string(), q(mem));
        }
        if map.is_empty() { None } else { Some(map) }
    }

    fn make_pod_with_resources(name: &str, containers: &[ContainerRes]) -> Pod {
        use k8s_openapi::api::core::v1::{Container, PodSpec, ResourceRequirements};

        let spec_containers = containers
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
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                ..Default::default()
            },
            spec: Some(PodSpec {
                containers: spec_containers,
                ..PodSpec::default()
            }),
            ..Pod::default()
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
        let (cpu, mem) = pod_allocations(Some(&pod));
        assert_f64_near(cpu, 1.0, 1e-9);
        assert_eq!(mem, 1024 * 1024 * 1024);
    }

    #[test]
    fn pod_allocations_uses_requests_when_limits_absent() {
        let pod = make_pod_with_resources(
            "worker",
            &[ContainerRes {
                cpu_request: Some("250m"),
                mem_request: Some("256Mi"),
                cpu_limit: None,
                mem_limit: None,
            }],
        );
        let (cpu, mem) = pod_allocations(Some(&pod));
        assert_f64_near(cpu, 0.25, 1e-9);
        assert_eq!(mem, 256 * 1024 * 1024);
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
        let (cpu, mem) = pod_allocations(Some(&pod));
        assert_f64_near(cpu, 2.0, 1e-9);
        assert_eq!(mem, 512 * 1024 * 1024);
    }

    #[test]
    fn pod_percent_uses_limits_not_small_requests() {
        let pod = make_pod_with_resources(
            "hcfs-server",
            &[ContainerRes {
                cpu_request: Some("100m"),
                mem_request: Some("128Mi"),
                cpu_limit: Some("1"),
                mem_limit: Some("1Gi"),
            }],
        );
        // 90m / 1 core = 9%, 64Mi / 1Gi = 6.25%. Against requests these
        // would look like 90% and 50%.
        let (cpu, mem) = compute_pod_percentages(
            0.09,
            64 * 1024 * 1024,
            Some(&pod),
            64.0,
            256 * 1024 * 1024 * 1024,
        );
        assert_f64_near(cpu, 9.0, 0.01);
        assert_f64_near(mem, 6.25, 0.01);
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
}

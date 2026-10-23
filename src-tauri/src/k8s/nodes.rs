//! One report per node: health from its conditions, usage from its
//! metrics-server sample against its allocatable capacity, and its root
//! filesystem from kubelet stats. The cluster totals are summed from these
//! reports, so a node's allocatable is parsed once, with one rule for a
//! malformed value.

use std::collections::HashMap;

use k8s_openapi::api::core::v1::{Node, NodeCondition};

use crate::metrics::{Capacity, NodeMetrics, NodeProblem, ResourceUsage, Resources, Usage};

use super::CpuMem;
use super::quantity::{parse_cpu_quantity, parse_memory_quantity};

/// Prefix of the labels that give a node its roles; the role is the rest.
const ROLE_LABEL: &str = "node-role.kubernetes.io/";

/// CPU and memory summed over a cluster's node reports.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct NodeTotals {
    /// Over every node that reports allocatable: what the scheduler can
    /// place pods on.
    pub(super) allocatable: Resources,
    /// CPU and memory in use against the allocatable of the same nodes:
    /// those with both a sample and allocatable.
    pub(super) usage: Usage,
    pub(super) used: Resources,
}

pub(super) fn node_totals(nodes: &[NodeMetrics]) -> NodeTotals {
    let mut allocatable = Resources::default();
    let mut sampled_allocatable = Resources::default();
    let mut used = Resources::default();
    for node in nodes {
        let Some(node_allocatable) = node.allocatable else {
            continue;
        };
        allocatable.add(node_allocatable);
        // `usage` is only set alongside `allocatable`; see `node_metrics`.
        if let Some(usage) = node.usage {
            sampled_allocatable.add(node_allocatable);
            used.add(usage.used);
        }
    }
    NodeTotals {
        allocatable,
        usage: used.share_of(&sampled_allocatable),
        used,
    }
}

/// A report for every named node, sorted by name. `usage` holds each
/// node's metrics-server sample, and `disks` its root filesystem. A node
/// without allocatable gets no usage: there is nothing to measure it by.
pub(super) fn node_metrics(
    nodes: &[Node],
    usage: &HashMap<String, CpuMem>,
    disks: &HashMap<&str, Capacity>,
) -> Vec<NodeMetrics> {
    let mut reports: Vec<NodeMetrics> = nodes
        .iter()
        .filter_map(|node| {
            let name = node.metadata.name.as_deref()?;
            let allocatable = allocatable(node);
            let disk = disks.get(name).copied();
            Some(NodeMetrics {
                name: name.to_string(),
                roles: roles(node),
                ready: condition_true(node, "Ready"),
                unschedulable: node
                    .spec
                    .as_ref()
                    .and_then(|s| s.unschedulable)
                    .unwrap_or(false),
                problems: problems(node),
                kubelet_version: node
                    .status
                    .as_ref()
                    .and_then(|s| s.node_info.as_ref())
                    .map(|info| info.kubelet_version.clone()),
                allocatable,
                usage: usage
                    .get(name)
                    .zip(allocatable)
                    .map(|(used, base)| node_usage(used.resources(), &base, disk)),
                disk,
            })
        })
        .collect();
    reports.sort_by(|a, b| a.name.cmp(&b.name));
    reports
}

/// The node's allocatable CPU and memory, `None` unless it reports both.
/// A malformed quantity is logged and treated as missing: one odd node
/// must not hide the others.
fn allocatable(node: &Node) -> Option<Resources> {
    let allocatable = node.status.as_ref()?.allocatable.as_ref()?;
    let cpu = parse_cpu_quantity(allocatable.get("cpu")?);
    let mem = parse_memory_quantity(allocatable.get("memory")?);
    match (cpu, mem) {
        (Ok(cpu), Ok(mem)) => Some(CpuMem { cpu, mem }.resources()),
        (Err(e), _) | (_, Err(e)) => {
            tracing::warn!(
                "ignoring allocatable of node {}: {}",
                node.metadata.name.as_deref().unwrap_or("unknown"),
                crate::error::error_chain(&e)
            );
            None
        }
    }
}

fn node_usage(used: Resources, allocatable: &Resources, disk: Option<Capacity>) -> ResourceUsage {
    ResourceUsage {
        usage: Usage {
            disk_percent: disk.map_or(0.0, |d| d.percent()),
            ..used.share_of(allocatable)
        },
        used,
    }
}

/// Roles from `node-role.kubernetes.io/<role>` labels, sorted.
fn roles(node: &Node) -> Vec<String> {
    let mut roles: Vec<String> = node
        .metadata
        .labels
        .iter()
        .flatten()
        .filter_map(|(key, _)| key.strip_prefix(ROLE_LABEL))
        .filter(|role| !role.is_empty())
        .map(str::to_string)
        .collect();
    roles.sort();
    roles
}

fn conditions(node: &Node) -> impl Iterator<Item = &NodeCondition> {
    node.status
        .as_ref()
        .and_then(|s| s.conditions.as_deref())
        .unwrap_or_default()
        .iter()
}

fn condition_true(node: &Node, kind: &str) -> bool {
    conditions(node).any(|c| c.type_ == kind && c.status == "True")
}

/// The trouble conditions that are `True`, in a fixed order.
fn problems(node: &Node) -> Vec<NodeProblem> {
    [
        ("MemoryPressure", NodeProblem::MemoryPressure),
        ("DiskPressure", NodeProblem::DiskPressure),
        ("PIDPressure", NodeProblem::PidPressure),
        ("NetworkUnavailable", NodeProblem::NetworkUnavailable),
    ]
    .into_iter()
    .filter(|(kind, _)| condition_true(node, kind))
    .map(|(_, problem)| problem)
    .collect()
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use std::collections::BTreeMap;

    use k8s_openapi::api::core::v1::{NodeSpec, NodeStatus, NodeSystemInfo};
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
    use kube::api::ObjectMeta;

    use super::*;

    fn condition(kind: &str, status: &str) -> NodeCondition {
        NodeCondition {
            type_: kind.to_string(),
            status: status.to_string(),
            ..NodeCondition::default()
        }
    }

    fn node(name: &str, conditions: Vec<NodeCondition>) -> Node {
        let allocatable = BTreeMap::from([
            ("cpu".to_string(), Quantity("4".to_string())),
            ("memory".to_string(), Quantity("8Gi".to_string())),
        ]);
        let labels = BTreeMap::from([
            (format!("{ROLE_LABEL}worker"), String::new()),
            (format!("{ROLE_LABEL}control-plane"), String::new()),
            ("kubernetes.io/os".to_string(), "linux".to_string()),
        ]);
        Node {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                labels: Some(labels),
                ..ObjectMeta::default()
            },
            spec: Some(NodeSpec {
                unschedulable: Some(true),
                ..NodeSpec::default()
            }),
            status: Some(NodeStatus {
                allocatable: Some(allocatable),
                conditions: Some(conditions),
                node_info: Some(NodeSystemInfo {
                    kubelet_version: "v1.31.4".to_string(),
                    ..NodeSystemInfo::default()
                }),
                ..NodeStatus::default()
            }),
        }
    }

    #[test]
    fn node_reports_health_usage_and_disk() {
        let nodes = [
            node(
                "b",
                vec![
                    condition("Ready", "True"),
                    condition("DiskPressure", "True"),
                    condition("MemoryPressure", "False"),
                ],
            ),
            node("a", vec![condition("Ready", "Unknown")]),
        ];
        let usage = HashMap::from([(
            "b".to_string(),
            CpuMem {
                cpu: 1.0,
                mem: 2 * 1024 * 1024 * 1024,
            },
        )]);
        let disks = HashMap::from([(
            "b",
            Capacity {
                used_bytes: 88,
                capacity_bytes: 100,
            },
        )]);

        let reports = node_metrics(&nodes, &usage, &disks);

        let names: Vec<&str> = reports.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);

        let [a, b] = reports.as_slice() else {
            unreachable!("two nodes in, two reports out");
        };
        assert!(!a.ready, "Unknown is not ready");
        assert!(a.usage.is_none() && a.disk.is_none());

        assert!(b.ready && b.unschedulable);
        assert_eq!(b.roles, ["control-plane", "worker"]);
        assert_eq!(b.problems, [NodeProblem::DiskPressure]);
        assert_eq!(b.kubelet_version.as_deref(), Some("v1.31.4"));
        assert_eq!(
            b.allocatable,
            Some(Resources {
                cpu_millicores: 4000.0,
                memory_bytes: 8 * 1024 * 1024 * 1024,
            })
        );
        let usage = b.usage.expect("sampled").usage;
        assert!((usage.cpu_percent - 25.0).abs() < 1e-9);
        assert!((usage.memory_percent - 25.0).abs() < 1e-9);
        assert!((usage.disk_percent - 88.0).abs() < 1e-9);
    }

    #[test]
    fn malformed_allocatable_leaves_the_node_without_a_base_or_usage() {
        let mut bad = node("a", vec![condition("Ready", "True")]);
        if let Some(allocatable) = bad.status.as_mut().and_then(|s| s.allocatable.as_mut()) {
            allocatable.insert("cpu".to_string(), Quantity("lots".to_string()));
        }
        let sampled = HashMap::from([("a".to_string(), CpuMem { cpu: 3.0, mem: 1 })]);

        let reports = node_metrics(&[bad], &sampled, &HashMap::new());

        assert_eq!(reports[0].allocatable, None);
        assert!(reports[0].usage.is_none(), "busy, not 0%: unmeasurable");
        assert!(reports[0].ready);
    }

    #[test]
    fn totals_measure_usage_only_against_sampled_nodes() {
        let nodes = [
            node("sampled", Vec::new()),
            node("unsampled", Vec::new()),
            node("no-allocatable", Vec::new()),
        ];
        let mut nodes = nodes.to_vec();
        if let Some(status) = nodes[2].status.as_mut() {
            status.allocatable = None;
        }
        let gib = 1024 * 1024 * 1024;
        let sampled = HashMap::from([
            (
                "sampled".to_string(),
                CpuMem {
                    cpu: 1.0,
                    mem: 2 * gib,
                },
            ),
            ("no-allocatable".to_string(), CpuMem { cpu: 9.0, mem: 9 }),
        ]);

        let totals = node_totals(&node_metrics(&nodes, &sampled, &HashMap::new()));

        assert_eq!(
            totals.allocatable,
            Resources {
                cpu_millicores: 8000.0,
                memory_bytes: 16 * gib,
            },
            "every node with allocatable"
        );
        assert!((totals.used.cpu_millicores - 1000.0).abs() < 1e-9);
        assert!(
            (totals.usage.cpu_percent - 25.0).abs() < 1e-9,
            "1 of 4 cores"
        );
        assert!((totals.usage.memory_percent - 25.0).abs() < 1e-9);
    }
}

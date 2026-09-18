//! Kubernetes Metrics API types that are not in k8s-openapi.

use std::borrow::Cow;

use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::Resource;
use kube::api::ObjectMeta;
use kube::core::{ClusterResourceScope, NamespaceResourceScope};
use serde::{Deserialize, Serialize};

// -- Custom types for Metrics API (not in k8s-openapi) --

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(super) struct NodeMetrics {
    pub(super) metadata: ObjectMeta,
    pub(super) usage: NodeMetricsUsage,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub(super) struct NodeMetricsUsage {
    pub(super) cpu: Quantity,
    pub(super) memory: Quantity,
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
pub(super) struct PodMetrics {
    pub(super) metadata: ObjectMeta,
    pub(super) containers: Vec<ContainerMetrics>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub(super) struct ContainerMetrics {
    pub(super) usage: ContainerMetricsUsage,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub(super) struct ContainerMetricsUsage {
    pub(super) cpu: Quantity,
    pub(super) memory: Quantity,
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

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::*;

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
}

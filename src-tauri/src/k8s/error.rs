//! Kubernetes backend error types.

/// Failure categories for the Kubernetes backend. Each variant preserves
/// its underlying cause in the source chain (axiom `rust_quality_57`);
/// the poller flattens the chain only when logging.
///
/// `kube::Error` and `kube::config::KubeconfigError` are large (>128
/// bytes), so they are boxed to keep `K8sError` — and therefore every
/// `Result<_, K8sError>` on the happy path — small to move (axiom
/// `rust_quality_151`, clippy `result_large_err`).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum K8sError {
    #[error("failed to read kubeconfig {path}")]
    ReadKubeconfig {
        path: String,
        #[source]
        source: Box<kube::config::KubeconfigError>,
    },
    #[error("failed to read default kubeconfig")]
    ReadDefaultKubeconfig(#[source] Box<kube::config::KubeconfigError>),
    #[error("failed to build kube config for context {context}")]
    BuildConfig {
        context: String,
        #[source]
        source: Box<kube::config::KubeconfigError>,
    },
    #[error("failed to create kube client")]
    CreateClient(#[source] Box<kube::Error>),
    #[error("k8s client is not connected")]
    NotConnected,
    #[error("kubeconfig load task failed")]
    KubeconfigTask,
    #[error("timed out fetching stats for node {node}")]
    NodeStatsTimeout { node: String },
    #[error("failed to list nodes")]
    ListNodes(#[source] Box<kube::Error>),
    #[error("failed to list events in namespace {namespace}")]
    ListEvents {
        namespace: String,
        #[source]
        source: Box<kube::Error>,
    },
    #[error("failed to list node metrics (is metrics-server installed?)")]
    ListNodeMetrics(#[source] Box<kube::Error>),
    #[error("failed to list pod metrics in namespace {namespace}")]
    ListPodMetrics {
        namespace: String,
        #[source]
        source: Box<kube::Error>,
    },
    #[error("failed to list pods in namespace {namespace}")]
    ListPods {
        namespace: String,
        #[source]
        source: Box<kube::Error>,
    },
    #[error("failed to build stats request for node {node}")]
    BuildStatsRequest {
        node: String,
        #[source]
        source: http::Error,
    },
    #[error("failed to fetch stats for node {node}")]
    FetchNodeStats {
        node: String,
        #[source]
        source: Box<kube::Error>,
    },
    #[error(transparent)]
    Quantity(#[from] QuantityParseError),
}

/// Failure categories for parsing Kubernetes resource `Quantity` values.
/// Carries the offending value and underlying numeric-parse cause as
/// typed fields rather than a formatted string (axiom `rust_quality_63`).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum QuantityParseError {
    #[error("invalid cpu quantity {value}")]
    Cpu {
        value: String,
        #[source]
        source: std::num::ParseFloatError,
    },
    #[error("invalid memory quantity {value}")]
    MemoryInt {
        value: String,
        #[source]
        source: std::num::ParseIntError,
    },
    #[error("invalid memory quantity {value}")]
    MemoryFloat {
        value: String,
        #[source]
        source: std::num::ParseFloatError,
    },
    #[error("memory quantity {value} overflows u64")]
    MemoryOverflow { value: String },
}

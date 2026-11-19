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
pub(crate) enum K8sError {
    #[error("failed to read kubeconfig {path}")]
    ReadKubeconfig {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// Deliberately carries no source: serde's "invalid type" errors quote
    /// the offending input, so a mis-picked private key or `.env` file would
    /// be echoed into the UI and logs. Only the YAML position is kept.
    #[error("{path} is not a valid kubeconfig{position}")]
    NotAKubeconfig { path: String, position: String },
    #[error("failed to load kubeconfig {path}")]
    LoadKubeconfig {
        path: String,
        #[source]
        source: Box<kube::config::KubeconfigError>,
    },
    #[error("context {context} not found in kubeconfig (available: {available})")]
    UnknownContext { context: String, available: String },
    #[error("failed to build kube config for context {context}")]
    BuildConfig {
        context: String,
        #[source]
        source: Box<kube::config::KubeconfigError>,
    },
    #[error("failed to create kube client")]
    CreateClient(#[source] Box<kube::Error>),
    /// The kubeconfig's exec credential plugin ran and failed, with what it printed to
    /// stderr. Stands in for kube's error, whose message dumps the command's environment.
    #[error("credential plugin failed: {detail}")]
    CredentialPlugin { detail: String },
    #[error("context {context} has no credential plugin to log in with")]
    NoCredentialPlugin { context: String },
    #[error("credential plugin has no command")]
    NoPluginCommand,
    #[error("failed to start credential plugin")]
    StartCredentialPlugin(#[source] std::io::Error),
    #[error("login did not finish within {} minutes", limit.as_secs() / 60)]
    LoginTimeout { limit: std::time::Duration },
    #[error("kubeconfig load task failed")]
    KubeconfigTask(#[source] tokio::task::JoinError),
    #[error("timed out fetching stats for node {node}")]
    NodeStatsTimeout { node: String },
    #[error("failed to list namespaces")]
    ListNamespaces(#[source] Box<kube::Error>),
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
    #[error("listing namespace {namespace} failed")]
    NamespaceTask {
        namespace: String,
        #[source]
        source: tokio::task::JoinError,
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

impl K8sError {
    /// Whether signing in again through the credential plugin may fix this.
    pub(crate) fn needs_login(&self) -> bool {
        matches!(self, Self::CredentialPlugin { .. })
    }
}

/// Failure categories for parsing Kubernetes resource `Quantity` values.
/// Carries the offending value and underlying numeric-parse cause as
/// typed fields rather than a formatted string (axiom `rust_quality_63`).
#[derive(Debug, thiserror::Error)]
pub(crate) enum QuantityParseError {
    #[error("invalid quantity {value}: unknown suffix")]
    Suffix { value: String },
    #[error("invalid quantity {value}")]
    Number {
        value: String,
        #[source]
        source: std::num::ParseFloatError,
    },
    #[error("quantity {value} is negative")]
    Negative { value: String },
    #[error("quantity {value} overflows u64")]
    Overflow { value: String },
}

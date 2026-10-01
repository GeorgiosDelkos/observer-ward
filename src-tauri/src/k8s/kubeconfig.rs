//! Kubeconfig loading and validation, shared by the poller's connect path
//! and the add-server form so both agree on what a usable kubeconfig is.

use kube::config::Kubeconfig;
use serde::Serialize;

use super::K8sError;

/// What the add-server form needs from a kubeconfig: which contexts exist
/// and which one kubectl would pick by default.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct KubeconfigSummary {
    pub(crate) contexts: Vec<String>,
    pub(crate) current_context: Option<String>,
}

/// Read the kubeconfig at `path` (tilde-expanded), or the default
/// (`$KUBECONFIG` / `~/.kube/config`) when `path` is `None`.
///
/// Blocking file IO: async callers go through [`validate_server`] or
/// [`inspect`], which move it onto the blocking pool.
pub(crate) fn load(path: Option<&str>) -> Result<Kubeconfig, K8sError> {
    let Some(path) = path else {
        return Kubeconfig::read()
            .map_err(|source| K8sError::ReadDefaultKubeconfig(Box::new(source)));
    };

    let path = crate::config::expand_tilde(path);
    Kubeconfig::read_from(&path).map_err(|source| K8sError::ReadKubeconfig {
        path,
        source: Box::new(source),
    })
}

fn summarize(kubeconfig: &Kubeconfig) -> KubeconfigSummary {
    KubeconfigSummary {
        contexts: kubeconfig.contexts.iter().map(|c| c.name.clone()).collect(),
        current_context: kubeconfig.current_context.clone(),
    }
}

/// Fail when `context` is not defined in `kubeconfig`. The error lists the
/// contexts that do exist, since a typo is the usual cause.
fn ensure_context(kubeconfig: &Kubeconfig, context: &str) -> Result<(), K8sError> {
    if kubeconfig.contexts.iter().any(|c| c.name == context) {
        return Ok(());
    }

    let available: Vec<&str> = kubeconfig
        .contexts
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    let available = if available.is_empty() {
        "none".to_string()
    } else {
        available.join(", ")
    };

    Err(K8sError::UnknownContext {
        context: context.to_string(),
        available,
    })
}

/// Check that a k8s server entry can at least build a client config: the
/// kubeconfig is readable YAML and defines `context`. Does not contact the
/// cluster, so adding a server works offline and never hangs the form.
pub(crate) async fn validate_server(path: Option<String>, context: String) -> Result<(), K8sError> {
    tokio::task::spawn_blocking(move || {
        let kubeconfig = load(path.as_deref())?;
        ensure_context(&kubeconfig, &context)
    })
    .await
    .map_err(|_| K8sError::KubeconfigTask)?
}

/// Load the kubeconfig at `path` and list its contexts.
pub(crate) async fn inspect(path: Option<String>) -> Result<KubeconfigSummary, K8sError> {
    tokio::task::spawn_blocking(move || load(path.as_deref()).map(|kc| summarize(&kc)))
        .await
        .map_err(|_| K8sError::KubeconfigTask)?
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::{inspect, load, validate_server};
    use crate::error::error_chain;

    const TWO_CONTEXTS: &str = r#"
apiVersion: v1
kind: Config
clusters:
- name: "hippius"
  cluster:
    server: "https://example.invalid/k8s/clusters/c-1"
users:
- name: "hippius"
  user:
    token: "not-a-real-token"
contexts:
- name: "hippius"
  context:
    user: "hippius"
    cluster: "hippius"
- name: "staging"
  context:
    user: "hippius"
    cluster: "hippius"
current-context: "hippius"
"#;

    fn write_kubeconfig(dir: &tempfile::TempDir, contents: &str) -> String {
        let path = dir.path().join("kubeconfig.yaml");
        std::fs::write(&path, contents).expect("write test kubeconfig");
        path.display().to_string()
    }

    #[test]
    fn load_missing_file_names_the_path() {
        // The reported bug: a one-character typo in the saved path made
        // every poll fail. The error must say which path was tried.
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("hippius1-oct.yaml").display().to_string();

        let err = load(Some(&missing)).expect_err("missing file must fail");

        let rendered = error_chain(&err);
        assert!(
            rendered.starts_with(&format!("failed to read kubeconfig {missing}")),
            "{rendered}"
        );
        assert!(rendered.contains("No such file or directory"), "{rendered}");
    }

    #[test]
    fn load_rejects_malformed_yaml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_kubeconfig(&dir, "contexts: [unterminated");

        let err = load(Some(&path)).expect_err("malformed yaml must fail");

        let rendered = error_chain(&err);
        assert!(
            rendered.starts_with(&format!("failed to read kubeconfig {path}")),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn inspect_lists_contexts_and_current() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_kubeconfig(&dir, TWO_CONTEXTS);

        let summary = inspect(Some(path)).await.expect("valid kubeconfig");

        assert_eq!(summary.contexts, vec!["hippius", "staging"]);
        assert_eq!(summary.current_context.as_deref(), Some("hippius"));
    }

    #[tokio::test]
    async fn validate_accepts_defined_context() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_kubeconfig(&dir, TWO_CONTEXTS);

        validate_server(Some(path), "staging".to_string())
            .await
            .expect("staging is defined");
    }

    #[tokio::test]
    async fn validate_rejects_unknown_context_and_lists_available() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_kubeconfig(&dir, TWO_CONTEXTS);

        let err = validate_server(Some(path), "prod".to_string())
            .await
            .expect_err("prod is not defined");

        assert_eq!(
            err.to_string(),
            "context prod not found in kubeconfig (available: hippius, staging)"
        );
    }

    #[tokio::test]
    async fn validate_reports_none_when_kubeconfig_has_no_contexts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_kubeconfig(&dir, "apiVersion: v1\nkind: Config\n");

        let err = validate_server(Some(path), "hippius".to_string())
            .await
            .expect_err("no contexts defined");

        assert_eq!(
            err.to_string(),
            "context hippius not found in kubeconfig (available: none)"
        );
    }
}

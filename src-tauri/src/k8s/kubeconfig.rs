//! Kubeconfig loading and validation, shared by the poller's connect path
//! and the add-server form so both agree on what a usable kubeconfig is.

use kube::Config;
use kube::config::{KubeConfigOptions, Kubeconfig, KubeconfigError};
use serde::Serialize;

use super::K8sError;

/// What the add-server form needs from a kubeconfig: which contexts exist
/// and which one kubectl would pick by default.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct KubeconfigSummary {
    pub(crate) contexts: Vec<String>,
    pub(crate) current_context: Option<String>,
}

const DEFAULT_KUBECONFIG_LABEL: &str = "default kubeconfig ($KUBECONFIG or ~/.kube/config)";

/// Read the kubeconfig at `path` (tilde-expanded), or the default
/// (`$KUBECONFIG` / `~/.kube/config`) when `path` is `None`.
///
/// Blocking file IO: async callers go through [`validate_server`] or
/// [`inspect`], which move it onto the blocking pool.
pub(crate) fn load(path: Option<&str>) -> Result<Kubeconfig, K8sError> {
    let Some(path) = path else {
        return Kubeconfig::read()
            .map_err(|source| classify_load_error(source, DEFAULT_KUBECONFIG_LABEL));
    };

    let path = crate::config::expand_tilde(path);
    Kubeconfig::read_from(&path).map_err(|source| classify_load_error(source, &path))
}

/// Map kube's load errors onto ours. kube's messages embed their cause and
/// also return it from `source()`, so they would render twice; and its
/// YAML errors quote file contents, so they are reduced to a position.
fn classify_load_error(source: KubeconfigError, path: &str) -> K8sError {
    match source {
        KubeconfigError::ReadConfig(io, file) => K8sError::ReadKubeconfig {
            path: file.display().to_string(),
            source: io,
        },
        KubeconfigError::Parse(yaml) | KubeconfigError::InvalidStructure(yaml) => {
            let position = yaml.location().map_or_else(String::new, |at| {
                format!(
                    " (YAML error at line {}, column {})",
                    at.line(),
                    at.column()
                )
            });
            K8sError::NotAKubeconfig {
                path: path.to_string(),
                position,
            }
        }
        other => K8sError::LoadKubeconfig {
            path: path.to_string(),
            source: Box::new(other),
        },
    }
}

/// Build the client config for `context` exactly as the poller does. Only
/// resolves names, parses URLs and reads CA files: no network calls and no
/// exec auth plugins run here.
pub(crate) async fn build_config(
    kubeconfig: Kubeconfig,
    context: &str,
) -> Result<Config, K8sError> {
    let options = KubeConfigOptions {
        context: Some(context.to_string()),
        ..Default::default()
    };
    Config::from_custom_kubeconfig(kubeconfig, &options)
        .await
        .map_err(|source| K8sError::BuildConfig {
            context: context.to_string(),
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

/// Check that a k8s server entry can build the same client config the
/// poller will: readable kubeconfig, a defined context, and a context whose
/// cluster, server URL and CA resolve. Does not contact the cluster, so
/// adding a server works offline and never hangs the form.
pub(crate) async fn validate_server(path: Option<String>, context: String) -> Result<(), K8sError> {
    let checked_context = context.clone();
    let kubeconfig = tokio::task::spawn_blocking(move || {
        let kubeconfig = load(path.as_deref())?;
        // Checked first so a typo reports the available contexts rather
        // than kube's bare "failed to load current context".
        ensure_context(&kubeconfig, &checked_context)?;
        Ok::<_, K8sError>(kubeconfig)
    })
    .await
    .map_err(|_| K8sError::KubeconfigTask)??;

    build_config(kubeconfig, &context).await.map(drop)
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
    fn load_missing_file_names_the_path_once_with_an_io_cause() {
        // The reported bug: a one-character typo in the saved path made
        // every poll fail. The error must say which path was tried.
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("hippius1-oct.yaml").display().to_string();

        let err = load(Some(&missing)).expect_err("missing file must fail");

        assert_eq!(
            err.to_string(),
            format!("failed to read kubeconfig {missing}")
        );
        let kind = std::error::Error::source(&err)
            .and_then(|cause| cause.downcast_ref::<std::io::Error>())
            .map(std::io::Error::kind);
        assert_eq!(kind, Some(std::io::ErrorKind::NotFound));
        assert_eq!(error_chain(&err).matches(&missing).count(), 1);
    }

    #[test]
    fn load_reports_malformed_yaml_position() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_kubeconfig(&dir, "contexts: [unterminated");

        let err = load(Some(&path)).expect_err("malformed yaml must fail");

        let rendered = error_chain(&err);
        assert!(
            rendered.starts_with(&format!(
                "{path} is not a valid kubeconfig (YAML error at line"
            )),
            "{rendered}"
        );
    }

    #[test]
    fn load_never_echoes_contents_of_a_non_kubeconfig_file() {
        // serde's "invalid type: string ..." quotes the whole file, which
        // for a mis-picked private key would put the key on screen.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_kubeconfig(
            &dir,
            "-----BEGIN OPENSSH PRIVATE KEY-----\nSECRETKEYMATERIAL\n-----END OPENSSH PRIVATE KEY-----\n",
        );

        let err = load(Some(&path)).expect_err("a private key is not a kubeconfig");

        let rendered = error_chain(&err);
        assert!(!rendered.contains("SECRETKEYMATERIAL"), "{rendered}");
        assert!(!rendered.contains("BEGIN OPENSSH"), "{rendered}");
        assert!(
            rendered.starts_with(&format!("{path} is not a valid kubeconfig")),
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
    async fn validate_rejects_context_whose_cluster_is_missing() {
        // Passes the name check but could never connect; the poller would
        // show it offline forever.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_kubeconfig(
            &dir,
            "contexts:\n- name: dangling\n  context:\n    cluster: gone\n",
        );

        let err = validate_server(Some(path), "dangling".to_string())
            .await
            .expect_err("cluster gone is not defined");

        let rendered = error_chain(&err);
        assert!(
            rendered.starts_with("failed to build kube config for context dangling"),
            "{rendered}"
        );
        assert!(rendered.contains("gone"), "{rendered}");
    }

    #[tokio::test]
    async fn validate_rejects_cluster_without_server_url() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_kubeconfig(
            &dir,
            "clusters:\n- name: c\n  cluster: {}\n\
             contexts:\n- name: ctx\n  context:\n    cluster: c\n",
        );

        let err = validate_server(Some(path), "ctx".to_string())
            .await
            .expect_err("cluster has no server url");

        assert!(
            error_chain(&err).contains("cluster url is missing"),
            "{}",
            error_chain(&err)
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

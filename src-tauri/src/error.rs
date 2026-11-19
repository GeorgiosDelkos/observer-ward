//! Edge helper for rendering a typed error together with its full
//! [`std::error::Error::source`] chain.

/// Render `err` and every `source()` cause as a single `"top: next: root"`
/// line.
///
/// Typed errors deliberately keep their underlying cause in the source
/// chain instead of flattening it into `Display` (axiom
/// `rust_quality_57_error_source_chain`). This helper re-flattens the
/// chain only at the *edges* of the program — `tracing` log lines and the
/// `String` returned across the Tauri command boundary to the frontend —
/// where a single human-readable string is wanted and the structured
/// value is no longer needed. The chain is finite by construction:
/// `source()` returns `None` at the deepest cause.
///
/// Some dependencies (kube's `KubeconfigError`, for one) both embed their
/// cause in `Display` and return it from `source()`. A cause whose text the
/// rendering already ends with is skipped so it is not printed twice.
///
/// A failed kubeconfig credential plugin is rendered as what it printed to
/// stderr: kube's own message for it is the `Debug` print of the command,
/// with every environment variable set on it (credentials, for some
/// plugins), and of the raw output bytes. kube wraps that failure in
/// errors whose `Display` embeds it (`Error::Auth`, and `Error::Service`
/// on a token refresh), so the rendering stops at the first link that
/// contains its text. Output kube could not parse is kept out the same
/// way: the parser may quote it, and it would hold the token.
pub(crate) fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let links = || std::iter::successors(Some(err), |link| link.source());
    let plugin = links().find_map(plugin_run);

    let mut rendered = String::new();
    for link in links() {
        let text = link.to_string();
        if let Some((message, detail)) = &plugin
            && text.contains(message.as_str())
        {
            let failure = crate::k8s::K8sError::CredentialPlugin {
                detail: detail.clone(),
            };
            push_link(&mut rendered, &failure.to_string());
            break;
        }
        if !rendered.ends_with(&text) {
            push_link(&mut rendered, &text);
        }
    }
    rendered
}

fn push_link(rendered: &mut String, text: &str) {
    if !rendered.is_empty() {
        rendered.push_str(": ");
    }
    rendered.push_str(text);
}

/// kube's message for a credential plugin run that failed or printed
/// something other than a credential, and what to show instead, when
/// `link` is that run.
fn plugin_run(link: &(dyn std::error::Error + 'static)) -> Option<(String, String)> {
    let run = link.downcast_ref::<kube::client::AuthError>()?;
    let detail = match run {
        kube::client::AuthError::AuthExecRun { out, .. } => crate::k8s::stderr_summary(&out.stderr),
        kube::client::AuthError::AuthExecParse(_) => {
            "its output is not an ExecCredential".to_string()
        }
        _ => return None,
    };
    Some((run.to_string(), detail))
}

#[cfg(test)]
mod tests {
    use super::error_chain;

    #[derive(Debug, thiserror::Error)]
    #[error("root cause")]
    struct Root;

    /// Mimics kube's style: the cause is in `Display` and in `source()`.
    #[derive(Debug, thiserror::Error)]
    #[error("parse failed: {0}")]
    struct Embedding(#[source] Root);

    #[derive(Debug, thiserror::Error)]
    #[error("loading config")]
    struct Top(#[source] Embedding);

    #[test]
    fn renders_each_cause_once_even_when_embedded() {
        assert_eq!(
            error_chain(&Top(Embedding(Root))),
            "loading config: parse failed: root cause"
        );
    }

    #[test]
    fn renders_a_failed_credential_plugin_without_its_environment() {
        use std::os::unix::process::ExitStatusExt;

        #[derive(Debug, thiserror::Error)]
        #[error("failed to list pods")]
        struct ListPods(#[source] kube::Error);

        // kube renders this as the Debug print of the command, env included.
        let failed = || kube::client::AuthError::AuthExecRun {
            cmd: "AWS_SECRET_ACCESS_KEY=\"hunter2\" \"aws-iam-authenticator\"".to_string(),
            out: Box::new(std::process::Output {
                status: std::process::ExitStatus::from_raw(1 << 8),
                stdout: b"{\"token\":\"hunter3\"}".to_vec(),
                stderr: b"session expired\n".to_vec(),
            }),
        };

        let rendered = error_chain(&ListPods(kube::Error::Auth(failed())));
        assert_eq!(
            rendered,
            "failed to list pods: credential plugin failed: session expired"
        );

        // A refresh on a cached client fails inside the HTTP service, which
        // kube reports as `Error::Service` with the same text embedded.
        let rendered = error_chain(&ListPods(kube::Error::Service(Box::new(failed()))));
        assert_eq!(
            rendered,
            "failed to list pods: credential plugin failed: session expired"
        );
    }

    #[test]
    fn renders_plain_chains_unchanged() {
        #[derive(Debug, thiserror::Error)]
        #[error("outer")]
        struct Outer(#[source] Root);

        assert_eq!(error_chain(&Outer(Root)), "outer: root cause");
    }
}

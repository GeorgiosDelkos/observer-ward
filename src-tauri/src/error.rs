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
pub(crate) fn error_chain(err: &dyn std::error::Error) -> String {
    let mut rendered = err.to_string();
    let mut cause = err.source();
    while let Some(source) = cause {
        let text = source.to_string();
        if !rendered.ends_with(&text) {
            rendered.push_str(": ");
            rendered.push_str(&text);
        }
        cause = source.source();
    }
    rendered
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
    fn renders_plain_chains_unchanged() {
        #[derive(Debug, thiserror::Error)]
        #[error("outer")]
        struct Outer(#[source] Root);

        assert_eq!(error_chain(&Outer(Root)), "outer: root cause");
    }
}

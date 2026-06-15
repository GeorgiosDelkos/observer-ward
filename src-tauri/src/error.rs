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
pub(crate) fn error_chain(err: &dyn std::error::Error) -> String {
    let mut rendered = err.to_string();
    let mut cause = err.source();
    while let Some(source) = cause {
        rendered.push_str(": ");
        rendered.push_str(&source.to_string());
        cause = source.source();
    }
    rendered
}

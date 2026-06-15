//! Read-only Grafana alert ingestion: fetches the currently-active
//! alerts from a Grafana instance's Alertmanager-compatible API and
//! maps them onto the app's `Alert` domain type.
//!
//! Transport (reqwest) and wire parsing (`serde_json` over the
//! Alertmanager v2 schema) are kept separate so the parser is unit
//! testable without a network: `fetch_alerts` does I/O and delegates the
//! body to the pure `parse_alerts`.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::metrics::{Alert, AlertSeverity, AlertState};

/// Keychain service name under which Grafana tokens are stored, keyed by
/// the connection's `name`.
pub const KEYCHAIN_SERVICE: &str = "observer-ward.grafana";

/// Failure categories for Grafana alert ingestion. Each variant keeps
/// its underlying cause in the source chain (mirrors `ConfigError`),
/// so the poll loop can render it with `error::error_chain` at the edge.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GrafanaError {
    #[error("no Grafana API token stored for connection '{name}'")]
    MissingToken { name: String },
    #[error("keychain access failed")]
    Keychain(#[source] keyring_core::Error),
    #[error("failed to build the Grafana HTTP client")]
    Client(#[source] reqwest::Error),
    #[error("request to Grafana failed")]
    Http(#[source] reqwest::Error),
    #[error("Grafana returned HTTP status {code}")]
    Status { code: u16 },
    #[error("failed to parse the Grafana alert response")]
    Parse(#[source] serde_json::Error),
}

/// Alertmanager v2 `GettableAlert` — only the fields the app uses.
/// `#[serde(default)]` on the maps and strings tolerates instances that
/// omit optional members.
#[derive(Debug, Deserialize)]
struct GettableAlert {
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
    #[serde(rename = "startsAt", default)]
    starts_at: String,
    #[serde(rename = "generatorURL")]
    generator_url: Option<String>,
    #[serde(default)]
    fingerprint: String,
    #[serde(default)]
    status: AlertStatus,
}

#[derive(Debug, Deserialize, Default)]
struct AlertStatus {
    /// One of "active", "suppressed", "unprocessed".
    #[serde(default)]
    state: String,
}

fn to_alert(raw: GettableAlert) -> Alert {
    let name = raw
        .labels
        .get("alertname")
        .cloned()
        .unwrap_or_else(|| "(unnamed)".to_string());
    let severity = AlertSeverity::from_label(raw.labels.get("severity").map(String::as_str));
    // Only "suppressed" means silenced/inhibited; "active" and the
    // transient "unprocessed" are both treated as actively firing.
    let state = if raw.status.state == "suppressed" {
        AlertState::Suppressed
    } else {
        AlertState::Active
    };
    Alert {
        fingerprint: raw.fingerprint,
        name,
        severity,
        state,
        summary: raw.annotations.get("summary").cloned().unwrap_or_default(),
        description: raw
            .annotations
            .get("description")
            .cloned()
            .unwrap_or_default(),
        starts_at: raw.starts_at,
        labels: raw.labels,
        generator_url: raw.generator_url,
    }
}

/// Parse an Alertmanager v2 `/alerts` JSON array body into domain alerts.
///
/// # Errors
///
/// Returns [`GrafanaError::Parse`] if `body` is not a JSON array of
/// Alertmanager v2 alert objects.
pub fn parse_alerts(body: &str) -> Result<Vec<Alert>, GrafanaError> {
    let raw: Vec<GettableAlert> = serde_json::from_str(body).map_err(GrafanaError::Parse)?;
    Ok(raw.into_iter().map(to_alert).collect())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_active_alert() {
        let body = r#"[
            {
                "labels": {"alertname": "HighCPU", "severity": "critical", "instance": "web-1"},
                "annotations": {"summary": "CPU above 90%", "description": "web-1 at 95%"},
                "startsAt": "2026-06-15T10:00:00.000Z",
                "generatorURL": "https://grafana.internal/alerting/view",
                "fingerprint": "abc123",
                "status": {"state": "active"}
            }
        ]"#;
        let alerts = parse_alerts(body).expect("parse");
        assert_eq!(alerts.len(), 1);
        let a = &alerts[0];
        assert_eq!(a.fingerprint, "abc123");
        assert_eq!(a.name, "HighCPU");
        assert_eq!(a.severity, AlertSeverity::Critical);
        assert_eq!(a.state, AlertState::Active);
        assert_eq!(a.summary, "CPU above 90%");
        assert_eq!(a.description, "web-1 at 95%");
        assert_eq!(
            a.generator_url.as_deref(),
            Some("https://grafana.internal/alerting/view")
        );
        assert_eq!(a.labels.get("instance").map(String::as_str), Some("web-1"));
    }

    #[test]
    fn suppressed_state_maps_to_suppressed() {
        let body = r#"[{"labels": {"alertname": "X"}, "status": {"state": "suppressed"}}]"#;
        let alerts = parse_alerts(body).expect("parse");
        assert_eq!(alerts[0].state, AlertState::Suppressed);
    }

    #[test]
    fn unprocessed_state_maps_to_active() {
        let body = r#"[{"labels": {"alertname": "X"}, "status": {"state": "unprocessed"}}]"#;
        let alerts = parse_alerts(body).expect("parse");
        assert_eq!(alerts[0].state, AlertState::Active);
    }

    #[test]
    fn missing_severity_label_is_unknown() {
        let body = r#"[{"labels": {"alertname": "X"}, "status": {"state": "active"}}]"#;
        let alerts = parse_alerts(body).expect("parse");
        assert_eq!(alerts[0].severity, AlertSeverity::Unknown);
    }

    #[test]
    fn missing_annotations_become_empty_strings() {
        let body = r#"[{"labels": {"alertname": "X"}, "status": {"state": "active"}}]"#;
        let alerts = parse_alerts(body).expect("parse");
        assert_eq!(alerts[0].summary, "");
        assert_eq!(alerts[0].description, "");
    }

    #[test]
    fn missing_alertname_falls_back() {
        let body = r#"[{"labels": {"severity": "warning"}, "status": {"state": "active"}}]"#;
        let alerts = parse_alerts(body).expect("parse");
        assert_eq!(alerts[0].name, "(unnamed)");
    }

    #[test]
    fn empty_array_is_no_alerts() {
        let alerts = parse_alerts("[]").expect("parse");
        assert!(alerts.is_empty());
    }

    #[test]
    fn invalid_json_errors() {
        assert!(parse_alerts("not json").is_err());
    }

    #[test]
    fn parses_multiple_alerts_independently() {
        let body = r#"[
            {
                "labels": {"alertname": "HighCPU", "severity": "critical"},
                "fingerprint": "fp1",
                "status": {"state": "active"}
            },
            {
                "labels": {"alertname": "DiskWarn", "severity": "warning"},
                "fingerprint": "fp2",
                "status": {"state": "suppressed"}
            }
        ]"#;
        let alerts = parse_alerts(body).expect("parse");
        assert_eq!(alerts.len(), 2);
        assert_eq!(alerts[0].fingerprint, "fp1");
        assert_eq!(alerts[0].severity, AlertSeverity::Critical);
        assert_eq!(alerts[0].state, AlertState::Active);
        assert_eq!(alerts[1].fingerprint, "fp2");
        assert_eq!(alerts[1].severity, AlertSeverity::Warning);
        assert_eq!(alerts[1].state, AlertState::Suppressed);
    }
}

# Grafana Alert Ingestion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Observer Ward pull the currently-firing alerts from a self-hosted Grafana on each poll cycle and surface them in the tray app (alert list, tray-icon severity, native notifications), read-only.

**Architecture:** A new `grafana_backend.rs` module (parallel to `k8s_backend.rs`/`ssh_backend.rs`) fetches `GET /api/alertmanager/grafana/api/v2/alerts` with a Bearer service-account token, parses the Alertmanager v2 response into domain `Alert` values, and the existing poll loop emits a new `alerts-update` Tauri event. The token lives in the OS keychain (never in `config.json`). Alert severity folds into the existing tray-level logic, and newly-firing alerts fire native notifications using the established previous-state dedup pattern.

**Tech Stack:** Rust + Tauri 2, `reqwest` (rustls TLS, matching `kube`), `keyring` v4 (`apple-native` store), `serde`/`serde_json`, `thiserror`; vanilla JS frontend (no build step, no JS test harness).

---

## File Structure

**Rust (`src-tauri/`):**
- `Cargo.toml` — add `reqwest` (direct) and `keyring` dependencies.
- `src/metrics.rs` — MODIFY: add alert domain types (`AlertSeverity`, `AlertState`, `Alert`, `AlertsUpdate`) and pure helpers (`severity_to_level`, `worst_alert_level`, `newly_firing`). These live beside `ServerMetrics`/`MetricLevel` because they map onto `MetricLevel` and are the cross-process event payload.
- `src/config.rs` — MODIFY: add `GrafanaConfig` struct and an `Option<GrafanaConfig>` field on `AppConfig`.
- `src/grafana_backend.rs` — CREATE: `GrafanaError`, Alertmanager v2 wire DTOs, pure `parse_alerts`, keychain `read_token`, and the `GrafanaBackend` HTTP client. This is the only file doing I/O.
- `src/lib.rs` — MODIFY: register the module; add Tauri commands `set_grafana_token`, `has_grafana_token`, `delete_grafana_token`, `open_url`; register them in `invoke_handler`.
- `src/poller.rs` — MODIFY: add Grafana polling to the loop, tray fold-in, and alert notifications.

**Frontend (`ui/`):**
- `index.html` — MODIFY: add an alerts container in the server list area and Grafana rows in the settings panel.
- `app.js` — MODIFY: render alerts, listen for `alerts-update`, load/save Grafana settings + token, open `generatorURL`.
- `styles.css` — MODIFY: alert-row styling.

**Docs:**
- `README.md` — MODIFY: document the Grafana integration.

**Decisions baked in (from the spec + confirmation):**
- Poll cadence: reuse the existing foreground/background interval (the whole loop already sleeps that long; no separate timer).
- Notifications: fire only when an alert *starts* firing (no "resolved" notification), matching the existing escalate-only notification model. `prev_alert_fingerprints` is updated every cycle even when notifications are disabled, so toggling them on does not replay the backlog.
- macOS-only process launching for `open_url` (consistent with the existing `run_in_terminal` code).

---

## Task 1: Add dependencies

**Files:**
- Modify: `src-tauri/Cargo.toml:15-34` (the `[dependencies]` table)

- [ ] **Step 1: Add the two dependencies**

In `src-tauri/Cargo.toml`, add these lines at the end of the `[dependencies]` table (after the `thiserror = "2"` line on line 34):

```toml
# Direct HTTP client for the Grafana alerts API. rustls-tls (not the
# default native-tls) matches kube's TLS backend so the build links one
# TLS stack, not two. default-features = false drops the unused native-tls
# default; a plain GET needs none of the dropped features.
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls"] }
# OS keychain for the Grafana service-account token (a secret, so it is
# never written to config.json). apple-native selects the macOS Keychain
# backend; add windows-native / a secret-service feature when porting.
keyring = { version = "4", features = ["apple-native"] }
```

- [ ] **Step 2: Verify it resolves and builds**

Run: `cd src-tauri && cargo build`
Expected: PASS (compiles). If `keyring` reports an unknown feature `apple-native`, check the keyring v4 docs for the current macOS store feature name and fix it here before continuing — this build step is the fast feedback for that.

- [ ] **Step 3: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock
git commit -m "build: add reqwest and keyring deps for Grafana integration"
```

---

## Task 2: Alert domain types and pure helpers in `metrics.rs`

**Files:**
- Modify: `src-tauri/src/metrics.rs:1` (imports) and append new types after `MetricsUpdate` (around line 85)
- Test: `src-tauri/src/metrics.rs` (`#[cfg(test)]` module, append tests)

- [ ] **Step 1: Write the failing tests**

Append these tests inside the existing `mod tests { ... }` block in `src-tauri/src/metrics.rs` (before its closing `}` on line 424):

```rust
    #[test]
    fn severity_from_label_parses_known_values() {
        assert_eq!(AlertSeverity::from_label(Some("critical")), AlertSeverity::Critical);
        assert_eq!(AlertSeverity::from_label(Some("warning")), AlertSeverity::Warning);
        assert_eq!(AlertSeverity::from_label(Some("info")), AlertSeverity::Info);
        // Case-insensitive.
        assert_eq!(AlertSeverity::from_label(Some("CRITICAL")), AlertSeverity::Critical);
        // Unknown string and missing label both fall back to Unknown.
        assert_eq!(AlertSeverity::from_label(Some("page")), AlertSeverity::Unknown);
        assert_eq!(AlertSeverity::from_label(None), AlertSeverity::Unknown);
    }

    #[test]
    fn severity_to_level_maps_to_tray_levels() {
        assert_eq!(severity_to_level(AlertSeverity::Critical), MetricLevel::Crit);
        assert_eq!(severity_to_level(AlertSeverity::Warning), MetricLevel::Warn);
        assert_eq!(severity_to_level(AlertSeverity::Info), MetricLevel::Ok);
        assert_eq!(severity_to_level(AlertSeverity::Unknown), MetricLevel::Ok);
    }

    fn make_alert(fingerprint: &str, severity: AlertSeverity, state: AlertState) -> Alert {
        Alert {
            fingerprint: fingerprint.to_string(),
            name: "Test".to_string(),
            severity,
            state,
            summary: String::new(),
            description: String::new(),
            starts_at: String::new(),
            labels: std::collections::BTreeMap::new(),
            generator_url: None,
        }
    }

    #[test]
    fn worst_alert_level_picks_highest_active() {
        let alerts = vec![
            make_alert("a", AlertSeverity::Warning, AlertState::Active),
            make_alert("b", AlertSeverity::Critical, AlertState::Active),
        ];
        assert_eq!(worst_alert_level(&alerts), MetricLevel::Crit);
    }

    #[test]
    fn worst_alert_level_skips_suppressed() {
        let alerts = vec![make_alert("a", AlertSeverity::Critical, AlertState::Suppressed)];
        assert_eq!(worst_alert_level(&alerts), MetricLevel::Ok);
    }

    #[test]
    fn worst_alert_level_empty_is_ok() {
        assert_eq!(worst_alert_level(&[]), MetricLevel::Ok);
    }

    #[test]
    fn newly_firing_returns_only_new_active_alerts() {
        let mut prev = std::collections::HashSet::new();
        prev.insert("known".to_string());
        let alerts = vec![
            make_alert("known", AlertSeverity::Critical, AlertState::Active),
            make_alert("fresh", AlertSeverity::Warning, AlertState::Active),
            make_alert("muted", AlertSeverity::Critical, AlertState::Suppressed),
        ];
        let fired: Vec<&str> = newly_firing(&prev, &alerts)
            .iter()
            .map(|a| a.fingerprint.as_str())
            .collect();
        assert_eq!(fired, vec!["fresh"]);
    }

    #[test]
    fn alert_serializes_enums_lowercase() {
        let alert = make_alert("fp", AlertSeverity::Critical, AlertState::Active);
        let json = serde_json::to_string(&alert).expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(v["severity"], "critical");
        assert_eq!(v["state"], "active");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd src-tauri && cargo test --lib metrics::tests`
Expected: FAIL — compile errors, `cannot find type AlertSeverity`/`Alert`/function `severity_to_level` etc.

- [ ] **Step 3: Add the imports and types**

Change the import line at the top of `src-tauri/src/metrics.rs` (line 1) from:

```rust
use serde::{Deserialize, Serialize};
```

to:

```rust
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
```

Then insert the following after the `MetricsUpdate` struct (after line 85, before the `#[cfg(test)]` block):

```rust
/// Severity of a Grafana alert, derived from the conventional
/// `severity` label. A missing or unrecognized label maps to
/// `Unknown` so an alert is never silently dropped for lacking the
/// label.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AlertSeverity {
    Critical,
    Warning,
    Info,
    Unknown,
}

impl AlertSeverity {
    /// Parse the `severity` label value, case-insensitively.
    #[must_use]
    pub fn from_label(value: Option<&str>) -> Self {
        match value.map(str::to_ascii_lowercase).as_deref() {
            Some("critical") => AlertSeverity::Critical,
            Some("warning") => AlertSeverity::Warning,
            Some("info") => AlertSeverity::Info,
            _ => AlertSeverity::Unknown,
        }
    }
}

/// Whether an alert is actively firing or suppressed (silenced or
/// inhibited in Grafana). Suppressed alerts are shown but do not raise
/// the tray level or fire notifications.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AlertState {
    Active,
    Suppressed,
}

/// A single Grafana alert as displayed by the app. `fingerprint` is
/// Grafana's stable per-alert identity, used as the notification dedup
/// key across poll cycles.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Alert {
    pub fingerprint: String,
    pub name: String,
    pub severity: AlertSeverity,
    pub state: AlertState,
    pub summary: String,
    pub description: String,
    pub starts_at: String,
    pub labels: BTreeMap<String, String>,
    pub generator_url: Option<String>,
}

/// Event payload sent to the frontend via `app.emit("alerts-update", ...)`.
/// `source_error` is `Some` when the fetch failed, so the UI can show an
/// "unreachable" state instead of a stale list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertsUpdate {
    pub alerts: Vec<Alert>,
    pub source_error: Option<String>,
}

/// Map an alert severity onto the tray-icon metric level so Grafana
/// alerts and self-collected metrics share one visual scale.
#[must_use]
pub fn severity_to_level(severity: AlertSeverity) -> MetricLevel {
    match severity {
        AlertSeverity::Critical => MetricLevel::Crit,
        AlertSeverity::Warning => MetricLevel::Warn,
        AlertSeverity::Info | AlertSeverity::Unknown => MetricLevel::Ok,
    }
}

/// Worst tray level across the active (non-suppressed) alerts.
#[must_use]
pub fn worst_alert_level(alerts: &[Alert]) -> MetricLevel {
    alerts
        .iter()
        .filter(|a| a.state == AlertState::Active)
        .map(|a| severity_to_level(a.severity))
        .max()
        .unwrap_or(MetricLevel::Ok)
}

/// Active alert fingerprints present now but absent from `prev` — the
/// set that should raise a fresh notification this cycle. Suppressed
/// alerts never appear here.
#[must_use]
pub fn newly_firing<'a>(prev: &HashSet<String>, alerts: &'a [Alert]) -> Vec<&'a Alert> {
    alerts
        .iter()
        .filter(|a| a.state == AlertState::Active && !prev.contains(&a.fingerprint))
        .collect()
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd src-tauri && cargo test --lib metrics::tests`
Expected: PASS (all metrics tests, old and new).

- [ ] **Step 5: Lint**

Run: `cd src-tauri && cargo clippy --all-targets --all-features -- -D warnings`
Expected: PASS (no warnings).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/metrics.rs
git commit -m "feat: add Grafana alert domain types and helpers"
```

---

## Task 3: `GrafanaConfig` on `AppConfig`

**Files:**
- Modify: `src-tauri/src/config.rs:4-33` (`AppConfig` + `Default`) and add `GrafanaConfig`
- Test: `src-tauri/src/config.rs` (`#[cfg(test)]` module)

- [ ] **Step 1: Write the failing tests**

Find the `#[cfg(test)] mod tests` block in `src-tauri/src/config.rs` and append these tests inside it:

```rust
    #[test]
    fn config_without_grafana_defaults_to_none() {
        let json = r#"{ "foreground_poll_secs": 10, "servers": [] }"#;
        let config: AppConfig = serde_json::from_str(json).expect("deserialize");
        assert!(config.grafana.is_none());
    }

    #[test]
    fn grafana_config_roundtrips_with_default_verify_tls() {
        let json = r#"{
            "foreground_poll_secs": 10,
            "servers": [],
            "grafana": { "name": "home", "url": "https://grafana.internal", "enabled": true }
        }"#;
        let config: AppConfig = serde_json::from_str(json).expect("deserialize");
        let grafana = config.grafana.expect("grafana present");
        assert_eq!(grafana.name, "home");
        assert_eq!(grafana.url, "https://grafana.internal");
        assert!(grafana.enabled);
        // verify_tls defaults to true when omitted.
        assert!(grafana.verify_tls);
    }

    #[test]
    fn default_config_has_no_grafana() {
        assert!(AppConfig::default().grafana.is_none());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd src-tauri && cargo test --lib config::tests`
Expected: FAIL — `no field grafana on type AppConfig`.

- [ ] **Step 3: Add the field and struct**

In `src-tauri/src/config.rs`, add the field to `AppConfig` (after the `notifications_enabled` field on line 13):

```rust
    #[serde(default)]
    pub grafana: Option<GrafanaConfig>,
```

Add `grafana: None,` to the `Default` impl (after `notifications_enabled: false,` on line 30):

```rust
            grafana: None,
```

Add the new struct and its default helper after the `AppConfig` `Default` impl (after line 33):

```rust
/// Connection details for a single Grafana instance whose alerts the
/// app displays. The API token is NOT stored here — it lives in the OS
/// keychain, keyed by `name` (see `grafana_backend::read_token`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GrafanaConfig {
    /// Display label and keychain key for this connection.
    pub name: String,
    /// Base URL, e.g. `https://grafana.internal` (no trailing path).
    pub url: String,
    /// Verify TLS certificates. Defaults to true; set false only for a
    /// self-signed instance you trust.
    #[serde(default = "default_verify_tls")]
    pub verify_tls: bool,
    #[serde(default)]
    pub enabled: bool,
}

fn default_verify_tls() -> bool {
    true
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd src-tauri && cargo test --lib config::tests`
Expected: PASS.

- [ ] **Step 5: Lint**

Run: `cd src-tauri && cargo clippy --all-targets --all-features -- -D warnings`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/config.rs
git commit -m "feat: add optional GrafanaConfig to AppConfig"
```

---

## Task 4: Alertmanager v2 parsing in `grafana_backend.rs`

**Files:**
- Create: `src-tauri/src/grafana_backend.rs`
- Modify: `src-tauri/src/lib.rs:1-6` (module declarations)

- [ ] **Step 1: Create the module with error type, DTOs, and parser**

Create `src-tauri/src/grafana_backend.rs` with this content:

```rust
//! Read-only Grafana alert ingestion: fetches the currently-active
//! alerts from a Grafana instance's Alertmanager-compatible API and
//! maps them onto the app's `Alert` domain type.
//!
//! Transport (reqwest) and wire parsing (serde_json over the
//! Alertmanager v2 schema) are kept separate so the parser is unit
//! testable without a network: `fetch_alerts` does I/O and delegates the
//! body to the pure `parse_alerts`.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::config::GrafanaConfig;
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
    Keychain(#[source] keyring::Error),
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
/// Returns [`serde_json::Error`] if `body` is not a JSON array of
/// Alertmanager v2 alert objects.
pub fn parse_alerts(body: &str) -> Result<Vec<Alert>, serde_json::Error> {
    let raw: Vec<GettableAlert> = serde_json::from_str(body)?;
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
        assert_eq!(a.generator_url.as_deref(), Some("https://grafana.internal/alerting/view"));
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
}
```

- [ ] **Step 2: Register the module**

In `src-tauri/src/lib.rs`, add the module declaration. Change lines 1-6 from:

```rust
mod config;
mod error;
mod k8s_backend;
mod metrics;
mod poller;
mod ssh_backend;
```

to (keep alphabetical-ish grouping consistent with the file):

```rust
mod config;
mod error;
mod grafana_backend;
mod k8s_backend;
mod metrics;
mod poller;
mod ssh_backend;
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cd src-tauri && cargo test --lib grafana_backend::tests`
Expected: PASS (8 parser tests).

- [ ] **Step 4: Lint**

Run: `cd src-tauri && cargo clippy --all-targets --all-features -- -D warnings`
Expected: PASS. (`GrafanaError` variants and `KEYCHAIN_SERVICE` are not used yet; they are `pub`, so no dead-code warning. If clippy flags `Client`/`Http`/`Status` as unused, that resolves in Task 5 — but `pub` items are exempt.)

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/grafana_backend.rs src-tauri/src/lib.rs
git commit -m "feat: parse Grafana Alertmanager v2 alerts"
```

---

## Task 5: Keychain token read + `GrafanaBackend` HTTP client

**Files:**
- Modify: `src-tauri/src/grafana_backend.rs` (append `read_token` and `GrafanaBackend`)
- Test: `src-tauri/src/grafana_backend.rs` (extend `#[cfg(test)]`)

- [ ] **Step 1: Write the failing tests**

Append these tests inside the `mod tests` block in `src-tauri/src/grafana_backend.rs`:

```rust
    #[test]
    fn backend_builds_and_matches_config() {
        let cfg = GrafanaConfig {
            name: "home".to_string(),
            url: "https://grafana.internal".to_string(),
            verify_tls: true,
            enabled: true,
        };
        let backend = GrafanaBackend::new(&cfg, "token".to_string()).expect("build");
        assert!(backend.matches_config(&cfg));

        let changed = GrafanaConfig {
            url: "https://other.internal".to_string(),
            ..cfg.clone()
        };
        assert!(!backend.matches_config(&changed));
    }

    #[test]
    fn alerts_url_has_no_double_slash() {
        let cfg = GrafanaConfig {
            name: "home".to_string(),
            url: "https://grafana.internal/".to_string(), // trailing slash
            verify_tls: true,
            enabled: true,
        };
        let backend = GrafanaBackend::new(&cfg, "token".to_string()).expect("build");
        assert_eq!(
            backend.alerts_url(),
            "https://grafana.internal/api/alertmanager/grafana/api/v2/alerts"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cd src-tauri && cargo test --lib grafana_backend::tests`
Expected: FAIL — `cannot find type GrafanaBackend`.

- [ ] **Step 3: Implement `read_token` and `GrafanaBackend`**

Append to `src-tauri/src/grafana_backend.rs`, after the `parse_alerts` function and before the `#[cfg(test)]` block:

```rust
/// Read the stored API token for the connection named `name` from the OS
/// keychain.
///
/// # Errors
///
/// Returns [`GrafanaError::MissingToken`] if no token has been stored for
/// this connection, or [`GrafanaError::Keychain`] if the platform
/// keychain cannot be accessed.
pub fn read_token(name: &str) -> Result<String, GrafanaError> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, name).map_err(GrafanaError::Keychain)?;
    match entry.get_password() {
        Ok(token) => Ok(token),
        Err(keyring::Error::NoEntry) => Err(GrafanaError::MissingToken {
            name: name.to_string(),
        }),
        Err(source) => Err(GrafanaError::Keychain(source)),
    }
}

/// HTTP client bound to one Grafana instance. Owns its `reqwest::Client`,
/// base URL, and the token (held in memory only, loaded from the keychain
/// at construction). Cached in the `Poller` and reused across cycles.
pub struct GrafanaBackend {
    base_url: String,
    verify_tls: bool,
    token: String,
    client: reqwest::Client,
}

impl GrafanaBackend {
    /// Build a client for `config` using the already-resolved `token`.
    ///
    /// # Errors
    ///
    /// Returns [`GrafanaError::Client`] if the HTTP client cannot be built
    /// (e.g. the TLS backend fails to initialize).
    pub fn new(config: &GrafanaConfig, token: String) -> Result<Self, GrafanaError> {
        let client = reqwest::Client::builder()
            // verify_tls == false opts out of certificate validation for a
            // trusted self-signed instance; default config keeps it on.
            .danger_accept_invalid_certs(!config.verify_tls)
            .build()
            .map_err(GrafanaError::Client)?;
        Ok(Self {
            base_url: config.url.clone(),
            verify_tls: config.verify_tls,
            token,
            client,
        })
    }

    /// True if this backend was built for the same endpoint as `config`.
    /// The token is intentionally excluded: a token change is handled by
    /// rebuilding from the keychain, not compared here.
    #[must_use]
    pub fn matches_config(&self, config: &GrafanaConfig) -> bool {
        self.base_url == config.url && self.verify_tls == config.verify_tls
    }

    fn alerts_url(&self) -> String {
        format!(
            "{}/api/alertmanager/grafana/api/v2/alerts",
            self.base_url.trim_end_matches('/')
        )
    }

    /// Fetch the currently active alerts from Grafana.
    ///
    /// # Errors
    ///
    /// Returns [`GrafanaError::Http`] on transport failure,
    /// [`GrafanaError::Status`] on a non-2xx response (e.g. 401 for a bad
    /// token), or [`GrafanaError::Parse`] if the body is not valid
    /// Alertmanager v2 JSON.
    pub async fn fetch_alerts(&self) -> Result<Vec<Alert>, GrafanaError> {
        let response = self
            .client
            .get(self.alerts_url())
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(GrafanaError::Http)?;
        let status = response.status();
        if !status.is_success() {
            return Err(GrafanaError::Status {
                code: status.as_u16(),
            });
        }
        let body = response.text().await.map_err(GrafanaError::Http)?;
        parse_alerts(&body).map_err(GrafanaError::Parse)
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cd src-tauri && cargo test --lib grafana_backend::tests`
Expected: PASS.

> If `keyring::Error::NoEntry` is reported as an unknown variant, check the keyring v4 error enum and use the variant that represents "no credential found" — the build error names the available variants.

- [ ] **Step 5: Lint**

Run: `cd src-tauri && cargo clippy --all-targets --all-features -- -D warnings`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/grafana_backend.rs
git commit -m "feat: add keychain token read and Grafana HTTP client"
```

---

## Task 6: Tauri commands for token storage and URL opening

**Files:**
- Modify: `src-tauri/src/lib.rs` (add four commands; register them)

- [ ] **Step 1: Add the keychain commands**

In `src-tauri/src/lib.rs`, add these commands after the `copy_to_clipboard` command (after line 258):

```rust
#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned parameters"
)]
fn set_grafana_token(name: String, token: String) -> Result<(), String> {
    let entry = keyring::Entry::new(grafana_backend::KEYCHAIN_SERVICE, &name)
        .map_err(|e| format!("keychain error: {e}"))?;
    entry
        .set_password(&token)
        .map_err(|e| format!("keychain write failed: {e}"))
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned parameters"
)]
fn has_grafana_token(name: String) -> bool {
    // Returns false on any error (missing token or keychain failure); the
    // UI only needs "is it configured", and never reads the secret back.
    grafana_backend::read_token(&name).is_ok()
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned parameters"
)]
fn delete_grafana_token(name: String) -> Result<(), String> {
    let entry = keyring::Entry::new(grafana_backend::KEYCHAIN_SERVICE, &name)
        .map_err(|e| format!("keychain error: {e}"))?;
    match entry.delete_credential() {
        // Deleting a token that was never stored is a no-op success.
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("keychain delete failed: {e}")),
    }
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "tauri::command macro requires owned parameters"
)]
fn open_url(url: String) -> Result<(), String> {
    // Only http(s) — refuse file://, custom schemes, or app launches.
    // The URL is passed to `open` as a single argv entry (no shell), so
    // query-string characters cannot be interpreted as shell syntax;
    // scheme validation is the only check needed.
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("url must be http(s)".to_string());
    }
    std::process::Command::new("open")
        .arg(&url)
        .spawn()
        .map_err(|e| format!("failed to open url: {e}"))?;
    Ok(())
}
```

- [ ] **Step 2: Register the commands**

In `src-tauri/src/lib.rs`, add the four commands to the `invoke_handler` list (inside `tauri::generate_handler![ ... ]`, after `copy_to_clipboard,` on line 409):

```rust
            set_grafana_token,
            has_grafana_token,
            delete_grafana_token,
            open_url,
```

- [ ] **Step 3: Build and lint**

Run: `cd src-tauri && cargo build && cargo clippy --all-targets --all-features -- -D warnings`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/lib.rs
git commit -m "feat: add Grafana token keychain commands and open_url"
```

---

## Task 7: Wire Grafana polling into the poll loop

**Files:**
- Modify: `src-tauri/src/poller.rs` (imports, `Poller` fields, `new`, `run`, `update_tray_icon`, add `poll_grafana`/`notify_new_alerts`/`send_alert_notification`)

- [ ] **Step 1: Extend imports**

In `src-tauri/src/poller.rs`, change the `crate::metrics` import (lines 21-24) from:

```rust
use crate::metrics::{
    classify_level, has_restarts, worst_level, MetricLevel, MetricsUpdate, ServerMetrics,
    ServerStatus,
};
```

to:

```rust
use crate::grafana_backend::{read_token, GrafanaBackend};
use crate::metrics::{
    classify_level, has_restarts, newly_firing, severity_to_level, worst_alert_level, worst_level,
    Alert, AlertSeverity, AlertState, AlertsUpdate, MetricLevel, MetricsUpdate, ServerMetrics,
    ServerStatus,
};
```

- [ ] **Step 2: Add fields to `Poller`**

In the `Poller` struct (lines 42-53), add two fields after `prev_levels` (line 50):

```rust
    grafana_backend: Option<GrafanaBackend>,
    prev_alert_fingerprints: HashSet<String>,
```

- [ ] **Step 3: Initialize the new fields in `Poller::new`**

In `Poller::new` (the `Self { ... }` literal, lines 63-74), add after `prev_levels: HashMap::new(),` (line 71):

```rust
            grafana_backend: None,
            prev_alert_fingerprints: HashSet::new(),
```

- [ ] **Step 4: Add the Grafana methods**

Add these three methods inside `impl Poller`, after `check_and_notify`/`send_notification` (after line 392, before `cleanup_removed_backends`):

```rust
    /// Poll Grafana for active alerts. Returns `None` when no Grafana
    /// connection is configured or it is disabled (in which case any
    /// cached backend and notification state are cleared). Network and
    /// auth failures are returned as an `AlertsUpdate` carrying a
    /// `source_error`, never as a panic.
    async fn poll_grafana(
        &mut self,
        grafana: &Option<crate::config::GrafanaConfig>,
    ) -> Option<AlertsUpdate> {
        let Some(cfg) = grafana.as_ref().filter(|c| c.enabled) else {
            self.grafana_backend = None;
            self.prev_alert_fingerprints.clear();
            return None;
        };

        let needs_rebuild = self
            .grafana_backend
            .as_ref()
            .is_none_or(|b| !b.matches_config(cfg));
        if needs_rebuild {
            let token = match read_token(&cfg.name) {
                Ok(token) => token,
                Err(e) => {
                    return Some(AlertsUpdate {
                        alerts: Vec::new(),
                        source_error: Some(crate::error::error_chain(&e)),
                    });
                }
            };
            match GrafanaBackend::new(cfg, token) {
                Ok(backend) => self.grafana_backend = Some(backend),
                Err(e) => {
                    return Some(AlertsUpdate {
                        alerts: Vec::new(),
                        source_error: Some(crate::error::error_chain(&e)),
                    });
                }
            }
        }

        let Some(backend) = self.grafana_backend.as_ref() else {
            return None;
        };
        match tokio::time::timeout(COLLECT_TIMEOUT, backend.fetch_alerts()).await {
            Ok(Ok(alerts)) => Some(AlertsUpdate {
                alerts,
                source_error: None,
            }),
            Ok(Err(e)) => Some(AlertsUpdate {
                alerts: Vec::new(),
                source_error: Some(crate::error::error_chain(&e)),
            }),
            Err(_) => Some(AlertsUpdate {
                alerts: Vec::new(),
                source_error: Some("timed out fetching Grafana alerts".to_string()),
            }),
        }
    }

    fn notify_new_alerts(&mut self, enabled: bool, alerts: &[Alert]) {
        if enabled {
            for alert in newly_firing(&self.prev_alert_fingerprints, alerts) {
                self.send_alert_notification(alert);
            }
        }
        // Track the current active set even when notifications are
        // disabled, so re-enabling them does not replay the whole backlog
        // as "new". (Deliberately stronger than check_and_notify, which
        // resets on recovery.)
        self.prev_alert_fingerprints = alerts
            .iter()
            .filter(|a| a.state == AlertState::Active)
            .map(|a| a.fingerprint.clone())
            .collect();
    }

    fn send_alert_notification(&self, alert: &Alert) {
        use tauri_plugin_notification::NotificationExt;

        let severity = match alert.severity {
            AlertSeverity::Critical => "CRITICAL",
            AlertSeverity::Warning => "WARNING",
            AlertSeverity::Info => "INFO",
            AlertSeverity::Unknown => "ALERT",
        };
        let title = format!("Grafana {severity}: {}", alert.name);
        let body = if alert.summary.is_empty() {
            alert.name.clone()
        } else {
            alert.summary.clone()
        };

        if let Err(e) = self
            .app_handle
            .notification()
            .builder()
            .title(&title)
            .body(&body)
            .show()
        {
            tracing::warn!("failed to send alert notification: {e}");
        }
    }
```

- [ ] **Step 5: Change `update_tray_icon` to fold in alert level**

In `src-tauri/src/poller.rs`, change the `update_tray_icon` signature (line 280) from:

```rust
    fn update_tray_icon(&mut self, metrics: &[ServerMetrics]) {
```

to:

```rust
    fn update_tray_icon(&mut self, metrics: &[ServerMetrics], alerts: &[Alert]) {
```

And change the `let level = worst_level(metrics);` line (line 285) to take the worse of metrics and alerts:

```rust
        let level = worst_level(metrics).max(worst_alert_level(alerts));
```

- [ ] **Step 6: Wire it into `run`**

In `src-tauri/src/poller.rs`, in the `run` loop, capture the Grafana config from the snapshot. After the `let notifications_enabled = config.notifications_enabled;` line (line 110), add:

```rust
            let grafana_cfg = config.grafana.clone();
```

Then replace the block that currently reads (lines 127-128):

```rust
            self.check_and_notify(notifications_enabled, &update.servers);
            self.update_tray_icon(&update.servers);
```

with:

```rust
            self.check_and_notify(notifications_enabled, &update.servers);

            let alerts = if let Some(alerts_update) = self.poll_grafana(&grafana_cfg).await {
                if let Err(e) = self.app_handle.emit("alerts-update", &alerts_update) {
                    tracing::warn!("failed to emit alerts-update: {e}");
                }
                self.notify_new_alerts(notifications_enabled, &alerts_update.alerts);
                alerts_update.alerts
            } else {
                Vec::new()
            };

            self.update_tray_icon(&update.servers, &alerts);
```

- [ ] **Step 7: Build, test, lint**

Run: `cd src-tauri && cargo test && cargo clippy --all-targets --all-features -- -D warnings`
Expected: PASS. (`is_none_or` requires Rust 1.82+; the crate's MSRV is 1.91 per `Cargo.toml`, so it is available.)

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/poller.rs
git commit -m "feat: poll Grafana alerts in the poll loop with tray and notifications"
```

---

## Task 8: Frontend DOM — settings rows and alerts container

**Files:**
- Modify: `ui/index.html`

- [ ] **Step 1: Add the alerts container above the server list**

In `ui/index.html`, immediately before the `<!-- Server List -->` comment (line 101), add:

```html
    <!-- Grafana Alerts (populated by alerts-update events) -->
    <div id="alerts-section" class="alerts-section hidden">
      <div class="alerts-header">
        <span class="alerts-title">GRAFANA ALERTS</span>
        <span id="alerts-status" class="alerts-status"></span>
      </div>
      <div id="alerts-list" class="alerts-list"></div>
    </div>
```

- [ ] **Step 2: Add Grafana rows to the settings panel**

In `ui/index.html`, inside `.settings-body`, after the `notifications-toggle` `.settings-row` block (after line 151, before `.settings-actions` on line 152), add:

```html
        <div class="settings-divider"></div>
        <div class="settings-row">
          <label for="grafana-enabled">Grafana alerts</label>
          <label class="toggle-switch">
            <input type="checkbox" id="grafana-enabled">
            <span class="toggle-slider"></span>
          </label>
        </div>
        <div class="settings-row">
          <label for="grafana-url">Grafana URL</label>
          <input type="text" id="grafana-url"
                 placeholder="https://grafana.internal">
        </div>
        <div class="settings-row">
          <label for="grafana-verify-tls">Verify TLS</label>
          <label class="toggle-switch">
            <input type="checkbox" id="grafana-verify-tls" checked>
            <span class="toggle-slider"></span>
          </label>
        </div>
        <div class="settings-row">
          <label for="grafana-token">API token</label>
          <input type="password" id="grafana-token"
                 placeholder="(stored; leave blank to keep)">
        </div>
```

- [ ] **Step 3: Verify it renders without errors**

Run: `cargo tauri dev` (from the repo root). Open Settings.
Expected: the new Grafana rows appear; no console errors. (Inputs are not wired yet — that is Task 9.)

- [ ] **Step 4: Commit**

```bash
git add ui/index.html
git commit -m "feat: add Grafana alerts container and settings UI to HTML"
```

---

## Task 9: Frontend logic — render alerts, listen, settings

**Files:**
- Modify: `ui/app.js`

> No JS test harness exists in this project; verification is manual via `cargo tauri dev`.

- [ ] **Step 1: Add DOM refs and an alert severity helper**

In `ui/app.js`, after the settings DOM refs block (after line 891, the `notificationsToggle` line), add:

```js
const alertsSection = document.getElementById("alerts-section");
const alertsList = document.getElementById("alerts-list");
const alertsStatus = document.getElementById("alerts-status");
const grafanaEnabledToggle = document.getElementById("grafana-enabled");
const grafanaUrlInput = document.getElementById("grafana-url");
const grafanaVerifyTlsToggle = document.getElementById("grafana-verify-tls");
const grafanaTokenInput = document.getElementById("grafana-token");

const GRAFANA_CONN_NAME = "default";

function alertSeverityClass(severity) {
  if (severity === "critical") {
    return "sev-crit";
  }
  if (severity === "warning") {
    return "sev-warn";
  }
  return "sev-info";
}
```

- [ ] **Step 2: Add the alerts renderer and event handler**

In `ui/app.js`, after the `handleMetricsUpdate` function (after line 1102), add:

```js
// ── Grafana Alerts ────────────────────────────

let grafanaConfigured = false;

function renderAlertRow(alert) {
  const sevClass = alertSeverityClass(alert.severity);
  const suppressed = alert.state === "suppressed" ? " suppressed" : "";
  const age = formatAge(alert.starts_at);
  const ageHtml = age ? `<span class="alert-age">${age}</span>` : "";
  const summary = alert.summary || alert.name;
  const url = alert.generator_url || "";
  return `
    <div class="alert-row ${sevClass}${suppressed}"
         data-alert-url="${escapeHtml(url)}">
      <span class="alert-dot"></span>
      <div class="alert-text">
        <span class="alert-name">${escapeHtml(alert.name)}</span>
        <span class="alert-summary">${escapeHtml(summary)}</span>
      </div>
      ${ageHtml}
    </div>`;
}

function handleAlertsUpdate(event) {
  const payload = event.payload;
  if (!payload) {
    return;
  }

  // Hide the whole section unless Grafana is configured + enabled.
  if (!grafanaConfigured) {
    alertsSection.classList.add("hidden");
    resizeToContent();
    return;
  }
  alertsSection.classList.remove("hidden");

  if (payload.source_error) {
    alertsStatus.textContent = "unreachable";
    alertsStatus.classList.add("error");
    alertsList.innerHTML =
      `<div class="alerts-empty">${escapeHtml(payload.source_error)}</div>`;
    resizeToContent();
    return;
  }

  alertsStatus.classList.remove("error");
  const alerts = Array.isArray(payload.alerts) ? payload.alerts : [];
  // Critical first, then warning, then the rest; suppressed sink to the bottom.
  const rank = { critical: 0, warning: 1, info: 2, unknown: 3 };
  alerts.sort((a, b) => {
    const sa = a.state === "suppressed" ? 10 : 0;
    const sb = b.state === "suppressed" ? 10 : 0;
    return (sa + (rank[a.severity] ?? 3)) - (sb + (rank[b.severity] ?? 3));
  });

  const active = alerts.filter((a) => a.state !== "suppressed").length;
  alertsStatus.textContent = active > 0 ? `${active} firing` : "all clear";

  if (alerts.length === 0) {
    alertsList.innerHTML = '<div class="alerts-empty">No active alerts</div>';
  } else {
    alertsList.innerHTML = alerts.map(renderAlertRow).join("");
  }
  resizeToContent();
}
```

- [ ] **Step 3: Track configured state from config and wire click-through**

In `ui/app.js`, in the `init` function, after `servers = config.servers;` (line 1109), add:

```js
    grafanaConfigured = !!(config.grafana && config.grafana.enabled);
    if (!grafanaConfigured) {
      alertsSection.classList.add("hidden");
    }
```

Then register the event listener: in `init`, after the `await listen("metrics-update", handleMetricsUpdate);` line (line 1120), add:

```js
  await listen("alerts-update", handleAlertsUpdate);
```

Add a click handler for alert rows. After the existing `serverListEl.addEventListener("click", ...)` block (after line 1174), add:

```js
alertsList.addEventListener("click", (e) => {
  const row = e.target.closest(".alert-row");
  if (!row) {
    return;
  }
  const url = row.dataset.alertUrl;
  if (url) {
    invoke("open_url", { url }).catch((err) =>
      console.error("Failed to open alert URL:", err));
  }
});
```

- [ ] **Step 4: Load Grafana settings when opening Settings**

In `ui/app.js`, in `openSettings`, after `notificationsToggle.checked = config.notifications_enabled ?? false;` (line 900), add:

```js
    const grafana = config.grafana || null;
    grafanaEnabledToggle.checked = !!(grafana && grafana.enabled);
    grafanaUrlInput.value = grafana ? grafana.url : "";
    grafanaVerifyTlsToggle.checked = grafana ? grafana.verify_tls !== false : true;
    grafanaTokenInput.value = "";
    try {
      const hasToken = await invoke("has_grafana_token", { name: GRAFANA_CONN_NAME });
      grafanaTokenInput.placeholder = hasToken
        ? "(stored; leave blank to keep)"
        : "paste service-account token";
    } catch (err) {
      console.error("Failed to check Grafana token:", err);
    }
```

- [ ] **Step 5: Save Grafana settings**

In `ui/app.js`, in `saveSettings`, inside the first `try` block, after `config.notifications_enabled = notificationsToggle.checked;` (line 949), add:

```js
    const grafanaEnabled = grafanaEnabledToggle.checked;
    const grafanaUrl = grafanaUrlInput.value.trim();
    if (grafanaEnabled && grafanaUrl) {
      config.grafana = {
        name: GRAFANA_CONN_NAME,
        url: grafanaUrl,
        verify_tls: grafanaVerifyTlsToggle.checked,
        enabled: true,
      };
    } else if (grafanaUrl) {
      // Keep the connection details but disabled.
      config.grafana = {
        name: GRAFANA_CONN_NAME,
        url: grafanaUrl,
        verify_tls: grafanaVerifyTlsToggle.checked,
        enabled: false,
      };
    } else {
      config.grafana = null;
    }
    grafanaConfigured = grafanaEnabled && !!grafanaUrl;
```

Then, still in `saveSettings`, after the `save_config_cmd` invoke succeeds (after line 950, inside the same `try`), add token persistence:

```js
    const tokenValue = grafanaTokenInput.value.trim();
    if (tokenValue) {
      await invoke("set_grafana_token", {
        name: GRAFANA_CONN_NAME,
        token: tokenValue,
      });
    }
```

And hide the alerts section immediately if Grafana was turned off:

```js
    if (!grafanaConfigured) {
      alertsSection.classList.add("hidden");
    }
```

- [ ] **Step 6: Manual verification**

Run: `cargo tauri dev`. In Settings, enable Grafana, enter your URL, paste a service-account token, save.
Expected: within one poll cycle the "GRAFANA ALERTS" section appears; firing alerts list; a bad URL/token shows "unreachable"; clicking a row opens Grafana in the browser; disabling and saving hides the section.

- [ ] **Step 7: Commit**

```bash
git add ui/app.js
git commit -m "feat: render Grafana alerts and wire settings in frontend"
```

---

## Task 10: Frontend styles

**Files:**
- Modify: `ui/styles.css`

- [ ] **Step 1: Add alert styles**

Append to `ui/styles.css`:

```css
/* ── Grafana Alerts ─────────────────────────── */

.alerts-section {
  margin: 0 0 8px;
  padding: 0 12px;
}

.alerts-section.hidden {
  display: none;
}

.alerts-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin: 4px 0;
}

.alerts-title {
  font-size: 10px;
  letter-spacing: 1px;
  color: rgba(0, 255, 240, 0.7);
}

.alerts-status {
  font-size: 10px;
  color: rgba(255, 255, 255, 0.5);
}

.alerts-status.error {
  color: rgba(255, 45, 111, 0.9);
}

.alerts-empty {
  font-size: 11px;
  color: rgba(255, 255, 255, 0.4);
  padding: 4px 0;
}

.alert-row {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 8px;
  margin: 4px 0;
  border-radius: 6px;
  background: rgba(255, 255, 255, 0.03);
  border-left: 2px solid transparent;
  cursor: pointer;
}

.alert-row:hover {
  background: rgba(255, 255, 255, 0.06);
}

.alert-row.suppressed {
  opacity: 0.5;
}

.alert-dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  flex-shrink: 0;
}

.alert-row.sev-crit {
  border-left-color: #ff2d6f;
}
.alert-row.sev-crit .alert-dot {
  background: #ff2d6f;
}
.alert-row.sev-warn {
  border-left-color: #ffb800;
}
.alert-row.sev-warn .alert-dot {
  background: #ffb800;
}
.alert-row.sev-info {
  border-left-color: #00fff0;
}
.alert-row.sev-info .alert-dot {
  background: #00fff0;
}

.alert-text {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-width: 0;
}

.alert-name {
  font-size: 12px;
  color: rgba(255, 255, 255, 0.9);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.alert-summary {
  font-size: 10px;
  color: rgba(255, 255, 255, 0.5);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.alert-age {
  font-size: 10px;
  color: rgba(255, 255, 255, 0.4);
  flex-shrink: 0;
}

.settings-divider {
  height: 1px;
  background: rgba(255, 255, 255, 0.08);
  margin: 8px 0;
}
```

- [ ] **Step 2: Manual verification**

Run: `cargo tauri dev`. Confirm alert rows are color-coded by severity, suppressed rows are dimmed, and the layout matches the existing dark neon theme.

- [ ] **Step 3: Commit**

```bash
git add ui/styles.css
git commit -m "style: add Grafana alert row styling"
```

---

## Task 11: Documentation

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Document the feature**

In `README.md`, add a bullet to the Features list (after the "SSH server monitoring" bullet, line 10):

```markdown
- **Grafana alerts** -- pulls currently-firing alerts from a self-hosted Grafana and shows them in the tray, with severity-colored rows and native notifications
```

And add a new subsection under Usage, after the "Adding an SSH server" section (after line 92):

```markdown
### Connecting Grafana alerts

1. In Grafana, create a service-account token (Administration -> Users and access -> Service accounts) with permission to read alerts.
2. In Observer Ward, open Settings (gear icon) and enable **Grafana alerts**.
3. Enter your Grafana **URL** (e.g. `https://grafana.internal`) and paste the **API token**.
4. Save. Active alerts appear within one poll cycle.

The token is stored in the OS keychain, never in `config.json`. Observer Ward only reads alerts (it never modifies or silences them) and polls outbound, so it works from a laptop that Grafana cannot reach directly. Alerts must be Grafana-managed (the integration reads `/api/alertmanager/grafana/api/v2/alerts`).
```

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: document Grafana alert integration"
```

---

## Task 12: Full verification pass

- [ ] **Step 1: Full test + lint + format**

Run:
```bash
cd src-tauri
cargo fmt -- --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```
Expected: all PASS.

- [ ] **Step 2: End-to-end manual check**

Run `cargo tauri dev` and verify against the spec:
- Configure Grafana with a valid token → firing alerts list within one cycle.
- A critical Grafana alert turns the tray icon to the critical state even when self-collected metrics are green.
- A newly-firing alert produces one native notification (with notifications enabled); it does not re-fire on subsequent cycles.
- Bad token (401) or wrong URL → "unreachable" status, no notification spam, app stays responsive.
- Disable Grafana in Settings → alerts section hides, tray reflects only self-metrics.
- Clicking an alert opens its Grafana URL in the browser.

- [ ] **Step 3: Final commit if any fixups were needed**

```bash
git add -A
git commit -m "chore: Grafana alert ingestion verification fixups"
```

---

## Self-Review

**Spec coverage:**
- Pull from `/api/alertmanager/grafana/api/v2/alerts` with Bearer token → Task 5 (`fetch_alerts`).
- Separate `alerts-update` event, alerts not in `ServerMetrics` → Tasks 2 (types) + 7 (emit).
- Data model (`GrafanaConfig`, `Alert`, `AlertSeverity`, `AlertState`, `AlertsUpdate`) → Tasks 2 + 3.
- Token in OS keychain, not config → Tasks 5 (`read_token`) + 6 (commands); `config.json` stores only `GrafanaConfig`.
- Tray fold-in (worst of metrics + alerts) → Task 7 (`update_tray_icon`, `worst_alert_level`).
- Notification on newly-firing fingerprints, dedup → Tasks 2 (`newly_firing`) + 7 (`notify_new_alerts`).
- Suppressed alerts shown but not escalating → Tasks 2 (`worst_alert_level`/`newly_firing` filter) + 9 (dimmed rows).
- UI alerts panel + click-through + settings → Tasks 8, 9, 10.
- Typed `GrafanaError` with source chain, backoff/`source_error` resilience → Tasks 4 + 7.
- `unprocessed`→Active, `suppressed`→Suppressed state mapping → Task 4.
- Parser fixtures (firing/suppressed/missing severity/missing annotations/missing alertname/empty/invalid) → Task 4.
- README → Task 11.

**Placeholder scan:** No TBD/TODO; every code step shows complete code; every command has expected output.

**Type consistency:** `severity_to_level`, `worst_alert_level`, `newly_firing`, `parse_alerts`, `read_token`, `GrafanaBackend::{new,matches_config,alerts_url,fetch_alerts}`, `KEYCHAIN_SERVICE`, and event names `alerts-update` are referenced consistently across Tasks 2, 4, 5, 7, 9. Serde enum names (`critical`/`warning`/`info`/`unknown`, `active`/`suppressed`) match between Rust (`#[serde(rename_all = "lowercase")]`) and the JS in Task 9.

**Known integration risks flagged for the implementer** (surface at build time, not silently):
- `keyring` macOS store feature name (`apple-native`) — Task 1 build catches a wrong name.
- `keyring::Error::NoEntry` variant name — Tasks 5/6 build catches it.
- The exact live Alertmanager v2 JSON shape — capture a real sample from the user's Grafana and add it as an extra fixture in Task 4 if any field differs.

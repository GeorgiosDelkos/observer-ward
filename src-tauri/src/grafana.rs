//! Read-only Grafana alert ingestion: fetches the currently-active
//! alerts from a Grafana instance's Alertmanager-compatible API and
//! maps them onto the app's `Alert` domain type.
//!
//! Transport (reqwest) and wire parsing (`serde_json` over the
//! Alertmanager v2 schema) are kept separate so the parser is unit
//! testable without a network: `fetch_alerts` does I/O and delegates the
//! body to the pure `parse_alerts`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use reqwest::{StatusCode, Url};
use serde::Deserialize;

use crate::config::GrafanaConfig;
use crate::metrics::{Alert, AlertSeverity, AlertState, AlertsUpdate};

/// Keychain service name under which Grafana tokens are stored, keyed by
/// the connection's `name`.
const KEYCHAIN_SERVICE: &str = "observer-ward.grafana";

/// Ceiling on the alerts response body. A few hundred alerts are tens of
/// KB; this bounds memory if the endpoint misbehaves.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Failure categories for Grafana alert ingestion. Each variant keeps
/// its underlying cause in the source chain, so the poll loop can render
/// it with `error::error_chain` at the edge.
#[derive(Debug, thiserror::Error)]
pub(crate) enum GrafanaError {
    #[error("no Grafana API token stored for connection '{name}'")]
    MissingToken { name: String },
    #[error("keychain access failed")]
    Keychain(#[source] keyring::Error),
    #[error("keychain task failed")]
    KeychainTask(#[source] tokio::task::JoinError),
    #[error("invalid Grafana URL {url}")]
    InvalidUrl {
        url: String,
        #[source]
        source: url::ParseError,
    },
    #[error("Grafana URL {url} must use http or https")]
    UnsupportedScheme { url: String },
    #[error("failed to build the Grafana HTTP client")]
    Client(#[source] reqwest::Error),
    #[error("request to Grafana failed")]
    Http(#[source] reqwest::Error),
    #[error("Grafana returned HTTP status {code}")]
    Status { code: u16 },
    #[error("Grafana response exceeded {limit} bytes")]
    BodyTooLarge { limit: usize },
    #[error("failed to parse the Grafana alert response")]
    Parse(#[source] serde_json::Error),
}

// -- Keychain --------------------------------------------------------------

fn keychain_entry(name: &str) -> Result<keyring::Entry, GrafanaError> {
    keyring::Entry::new(KEYCHAIN_SERVICE, name).map_err(GrafanaError::Keychain)
}

/// Read the stored API token for the connection named `name`.
///
/// Blocking: the macOS Keychain may show a prompt. Call from a blocking
/// context.
///
/// # Errors
///
/// [`GrafanaError::MissingToken`] if none is stored, or
/// [`GrafanaError::Keychain`] if the keychain cannot be accessed.
pub(crate) fn read_token(name: &str) -> Result<String, GrafanaError> {
    match keychain_entry(name)?.get_password() {
        Ok(token) => Ok(token),
        Err(keyring::Error::NoEntry) => Err(GrafanaError::MissingToken {
            name: name.to_string(),
        }),
        Err(source) => Err(GrafanaError::Keychain(source)),
    }
}

/// Store `token` for `name`, replacing any previous one. Blocking.
///
/// # Errors
///
/// [`GrafanaError::Keychain`] if the keychain cannot be written.
pub(crate) fn store_token(name: &str, token: &str) -> Result<(), GrafanaError> {
    keychain_entry(name)?
        .set_password(token)
        .map_err(GrafanaError::Keychain)
}

/// Remove the token for `name`. Removing a token that was never stored
/// succeeds. Blocking.
///
/// # Errors
///
/// [`GrafanaError::Keychain`] if the keychain cannot be written.
pub(crate) fn delete_token(name: &str) -> Result<(), GrafanaError> {
    match keychain_entry(name)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(source) => Err(GrafanaError::Keychain(source)),
    }
}

/// Whether a token is stored for `name`, without returning it. Blocking.
///
/// # Errors
///
/// [`GrafanaError::Keychain`] if the keychain cannot be read.
pub(crate) fn has_token(name: &str) -> Result<bool, GrafanaError> {
    match read_token(name) {
        Ok(_) => Ok(true),
        Err(GrafanaError::MissingToken { .. }) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Bumped whenever the app writes or deletes a token, so the poller knows
/// its cached copy is stale without reading the keychain every cycle.
#[derive(Debug, Default)]
pub(crate) struct TokenEpoch(AtomicU64);

impl TokenEpoch {
    pub(crate) fn bump(&self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }

    fn current(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

// -- Wire format -----------------------------------------------------------

/// Alertmanager v2 `GettableAlert`, only the fields the app uses.
/// `#[serde(default)]` tolerates instances that omit optional members.
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
    #[serde(default)]
    state: WireAlertState,
}

#[derive(Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum WireAlertState {
    #[default]
    Active,
    Suppressed,
    Unprocessed,
    /// A state newer than this code; shown as firing rather than hidden.
    #[serde(other)]
    Other,
}

/// Grafana's fingerprint, or a stable stand-in built from the labels
/// (which identify an alert in Alertmanager). Without the stand-in every
/// fingerprint-less alert would share the key `""` and only the first
/// would ever notify.
fn fingerprint(raw: &GettableAlert) -> String {
    if !raw.fingerprint.is_empty() {
        return raw.fingerprint.clone();
    }
    let labels: Vec<String> = raw.labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    format!("labels:{}", labels.join(","))
}

fn to_alert(raw: GettableAlert) -> Alert {
    let fingerprint = fingerprint(&raw);
    let name = raw
        .labels
        .get("alertname")
        .cloned()
        .unwrap_or_else(|| "(unnamed)".to_string());
    let severity = AlertSeverity::from_label(raw.labels.get("severity").map(String::as_str));
    // Only "suppressed" means silenced/inhibited; anything else is
    // treated as actively firing.
    let state = match raw.status.state {
        WireAlertState::Suppressed => AlertState::Suppressed,
        WireAlertState::Active | WireAlertState::Unprocessed | WireAlertState::Other => {
            AlertState::Active
        }
    };
    let mut annotations = raw.annotations;
    Alert {
        fingerprint,
        name,
        severity,
        state,
        summary: annotations.remove("summary").unwrap_or_default(),
        description: annotations.remove("description").unwrap_or_default(),
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
fn parse_alerts(body: &[u8]) -> Result<Vec<Alert>, GrafanaError> {
    let raw: Vec<GettableAlert> = serde_json::from_slice(body).map_err(GrafanaError::Parse)?;
    Ok(raw.into_iter().map(to_alert).collect())
}

// -- HTTP client -----------------------------------------------------------

/// HTTP client bound to one Grafana instance and token.
struct GrafanaBackend {
    alerts_url: Url,
    verify_tls: bool,
    token: String,
    client: reqwest::Client,
}

impl GrafanaBackend {
    fn new(config: &GrafanaConfig, token: String) -> Result<Self, GrafanaError> {
        let alerts_url = alerts_url(&config.url)?;
        let client = reqwest::Client::builder()
            // verify_tls == false opts out of certificate validation for a
            // trusted self-signed instance; default config keeps it on.
            .danger_accept_invalid_certs(!config.verify_tls)
            .connect_timeout(Duration::from_secs(10))
            // Covers the whole exchange, body included.
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(GrafanaError::Client)?;
        Ok(Self {
            alerts_url,
            verify_tls: config.verify_tls,
            token,
            client,
        })
    }

    fn matches(&self, config: &GrafanaConfig, token: &str) -> bool {
        alerts_url(&config.url).is_ok_and(|url| url == self.alerts_url)
            && self.verify_tls == config.verify_tls
            && self.token == token
    }

    async fn fetch_alerts(&self) -> Result<Vec<Alert>, GrafanaError> {
        let mut response = self
            .client
            .get(self.alerts_url.clone())
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

        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(GrafanaError::Http)? {
            if body.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
                return Err(GrafanaError::BodyTooLarge {
                    limit: MAX_BODY_BYTES,
                });
            }
            body.extend_from_slice(&chunk);
        }
        parse_alerts(&body)
    }
}

/// The Grafana-embedded Alertmanager alerts endpoint (note the
/// `/api/alertmanager/grafana/...` prefix, not the raw Alertmanager
/// `/api/v2/alerts` path). Only http(s) is accepted: the request carries
/// a bearer token.
fn alerts_url(base: &str) -> Result<Url, GrafanaError> {
    let joined = format!(
        "{}/api/alertmanager/grafana/api/v2/alerts",
        base.trim().trim_end_matches('/')
    );
    let url = Url::parse(&joined).map_err(|source| GrafanaError::InvalidUrl {
        url: base.to_string(),
        source,
    })?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        _ => Err(GrafanaError::UnsupportedScheme {
            url: base.to_string(),
        }),
    }
}

/// Check that `base` can form the alerts URL, so a bad URL is rejected
/// when saved rather than failing every poll.
///
/// # Errors
///
/// [`GrafanaError::InvalidUrl`] or [`GrafanaError::UnsupportedScheme`].
pub(crate) fn validate_url(base: &str) -> Result<(), GrafanaError> {
    alerts_url(base).map(drop)
}

// -- Poll-loop source ------------------------------------------------------

/// The token last read from the keychain, and for which connection name
/// and epoch, so it is re-read only when something changed.
struct CachedToken {
    name: String,
    epoch: u64,
    token: String,
}

/// Owns everything the poll loop needs to fetch alerts: the cached token,
/// the HTTP backend, and the epoch shared with the token commands.
pub(crate) struct AlertSource {
    epoch: Arc<TokenEpoch>,
    token: Option<CachedToken>,
    backend: Option<GrafanaBackend>,
}

impl AlertSource {
    pub(crate) fn new(epoch: Arc<TokenEpoch>) -> Self {
        Self {
            epoch,
            token: None,
            backend: None,
        }
    }

    /// Fetch active alerts. `None` when no connection is configured or it
    /// is disabled. Failures come back as an update with `source_error`
    /// set, so the UI can show "unreachable" instead of "all clear".
    pub(crate) async fn poll(&mut self, config: Option<&GrafanaConfig>) -> Option<AlertsUpdate> {
        let Some(config) = config.filter(|c| c.enabled) else {
            self.token = None;
            self.backend = None;
            return None;
        };

        let update = match self.fetch(config).await {
            Ok(alerts) => AlertsUpdate {
                alerts,
                source_error: None,
            },
            Err(e) => {
                if let GrafanaError::Status { code } = e
                    && (code == StatusCode::UNAUTHORIZED || code == StatusCode::FORBIDDEN)
                {
                    // The token may have been rotated outside the app.
                    self.token = None;
                }
                AlertsUpdate {
                    alerts: Vec::new(),
                    source_error: Some(crate::error::error_chain(&e)),
                }
            }
        };
        Some(update)
    }

    async fn fetch(&mut self, config: &GrafanaConfig) -> Result<Vec<Alert>, GrafanaError> {
        let token = self.token(&config.name).await?;

        let backend = match self.backend.take() {
            Some(backend) if backend.matches(config, &token) => backend,
            Some(_) | None => GrafanaBackend::new(config, token)?,
        };
        let result = backend.fetch_alerts().await;
        self.backend = Some(backend);
        result
    }

    /// The token for `name`, from the cache when the epoch is unchanged.
    async fn token(&mut self, name: &str) -> Result<String, GrafanaError> {
        let epoch = self.epoch.current();
        if let Some(cached) = &self.token
            && cached.name == name
            && cached.epoch == epoch
        {
            return Ok(cached.token.clone());
        }

        self.token = None;
        let owned_name = name.to_string();
        let token = tokio::task::spawn_blocking(move || read_token(&owned_name))
            .await
            .map_err(GrafanaError::KeychainTask)??;
        self.token = Some(CachedToken {
            name: name.to_string(),
            epoch,
            token: token.clone(),
        });
        Ok(token)
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Vec<Alert> {
        parse_alerts(body.as_bytes()).expect("parse")
    }

    fn config(url: &str) -> GrafanaConfig {
        GrafanaConfig {
            name: "home".to_string(),
            url: url.to_string(),
            verify_tls: true,
            enabled: true,
        }
    }

    #[test]
    fn parses_one_active_alert() {
        let alerts = parse(
            r#"[
            {
                "labels": {"alertname": "HighCPU", "severity": "critical", "instance": "web-1"},
                "annotations": {"summary": "CPU above 90%", "description": "web-1 at 95%"},
                "startsAt": "2026-06-15T10:00:00.000Z",
                "generatorURL": "https://grafana.internal/alerting/view",
                "fingerprint": "abc123",
                "status": {"state": "active"}
            }
        ]"#,
        );
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
    fn alert_states_map_to_active_or_suppressed() {
        let state = |s: &str| {
            let body =
                format!(r#"[{{"labels": {{"alertname": "X"}}, "status": {{"state": "{s}"}}}}]"#);
            parse(&body)[0].state
        };
        assert_eq!(state("active"), AlertState::Active);
        assert_eq!(state("suppressed"), AlertState::Suppressed);
        assert_eq!(state("unprocessed"), AlertState::Active);
        assert_eq!(state("some-future-state"), AlertState::Active);
    }

    #[test]
    fn missing_fields_get_defaults() {
        let alerts = parse(r#"[{"labels": {"severity": "warning"}}]"#);
        let a = &alerts[0];
        assert_eq!(a.name, "(unnamed)");
        assert_eq!(a.state, AlertState::Active);
        assert_eq!(a.summary, "");
        assert_eq!(a.description, "");

        let alerts = parse(r#"[{"labels": {"alertname": "X"}}]"#);
        assert_eq!(alerts[0].severity, AlertSeverity::Unknown);
    }

    #[test]
    fn fingerprintless_alerts_get_distinct_label_keys() {
        let alerts = parse(
            r#"[
            {"labels": {"alertname": "A", "instance": "web-1"}},
            {"labels": {"alertname": "A", "instance": "web-2"}},
            {"labels": {"alertname": "A", "instance": "web-1"}, "fingerprint": ""}
        ]"#,
        );
        assert_ne!(alerts[0].fingerprint, alerts[1].fingerprint);
        assert_eq!(alerts[0].fingerprint, alerts[2].fingerprint);
        assert!(!alerts[0].fingerprint.is_empty());
    }

    #[test]
    fn empty_and_invalid_bodies() {
        assert!(parse("[]").is_empty());
        assert!(parse_alerts(b"not json").is_err());
    }

    #[test]
    fn parses_multiple_alerts_independently() {
        let alerts = parse(
            r#"[
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
        ]"#,
        );
        assert_eq!(alerts.len(), 2);
        assert_eq!(alerts[0].fingerprint, "fp1");
        assert_eq!(alerts[0].severity, AlertSeverity::Critical);
        assert_eq!(alerts[0].state, AlertState::Active);
        assert_eq!(alerts[1].fingerprint, "fp2");
        assert_eq!(alerts[1].severity, AlertSeverity::Warning);
        assert_eq!(alerts[1].state, AlertState::Suppressed);
    }

    #[test]
    fn backend_matches_endpoint_tls_and_token() {
        let cfg = config("https://grafana.internal");
        let backend = GrafanaBackend::new(&cfg, "token".to_string()).expect("build");

        assert!(backend.matches(&cfg, "token"));
        assert!(backend.matches(&config("https://grafana.internal/"), "token"));
        assert!(!backend.matches(&cfg, "rotated"));
        assert!(!backend.matches(&config("https://other.internal"), "token"));
        let insecure = GrafanaConfig {
            verify_tls: false,
            ..cfg
        };
        assert!(!backend.matches(&insecure, "token"));
    }

    #[test]
    fn alerts_url_is_joined_once_and_requires_http() {
        let url = alerts_url("https://grafana.internal/").expect("valid");
        assert_eq!(
            url.as_str(),
            "https://grafana.internal/api/alertmanager/grafana/api/v2/alerts"
        );

        assert!(matches!(
            alerts_url("file:///etc/passwd"),
            Err(GrafanaError::UnsupportedScheme { .. })
        ));
        assert!(matches!(
            alerts_url("grafana.internal"),
            Err(GrafanaError::InvalidUrl { .. })
        ));
    }

    #[tokio::test]
    async fn disabled_or_missing_config_yields_no_update() {
        let mut source = AlertSource::new(Arc::default());
        assert!(source.poll(None).await.is_none());

        let disabled = GrafanaConfig {
            enabled: false,
            ..config("https://grafana.internal")
        };
        assert!(source.poll(Some(&disabled)).await.is_none());
    }

    #[tokio::test]
    async fn cached_token_is_reused_until_the_epoch_moves() {
        let epoch = Arc::new(TokenEpoch::default());
        let mut source = AlertSource::new(Arc::clone(&epoch));
        source.token = Some(CachedToken {
            name: "home".to_string(),
            epoch: epoch.current(),
            token: "cached".to_string(),
        });

        let token = source.token("home").await.expect("served from cache");
        assert_eq!(token, "cached");

        epoch.bump();
        assert!(
            source
                .token
                .as_ref()
                .is_some_and(|c| c.epoch != epoch.current()),
            "a bump must make the cached token stale"
        );
    }
}

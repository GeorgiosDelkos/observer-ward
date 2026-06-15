# Grafana Alert Ingestion — Design Spec

**Date:** 2026-06-15
**Status:** Approved for planning
**Component:** Observer Ward (Tauri 2 + vanilla JS tray app)

## Problem

Observer Ward is today a stateless self-collector: each poll cycle it gathers
CPU/mem/disk/net metrics from configured SSH servers and Kubernetes clusters,
emits them to the frontend, updates the tray icon, fires native notifications on
threshold crossings, and discards the data. It has no awareness of the alerts the
user's existing Grafana already produces.

The user runs a self-hosted Grafana whose unified alerting already fires webhooks
to a custom service. They want Observer Ward to **display those Grafana alerts in
the tray app**, alongside its own self-collected metrics — turning the menu-bar
app into a lightweight alert console.

## Constraints that shaped the design

- **Roaming laptop.** Observer Ward runs on a personal machine that roams and
  sleeps. Grafana generally cannot reach it for inbound delivery, so a local
  webhook receiver is the wrong model. The app must **pull** (outbound) from
  Grafana on its own poll cadence.
- **Grafana-managed alerts.** The alerts are defined in Grafana (unified
  alerting) and flow through Grafana's internal Alertmanager to the webhook
  contact point. The Alertmanager-compatible read API therefore returns exactly
  the set that drives the webhook, with silences honored.
- **The token is a secret.** Unlike the SSH key *paths* already in `config.json`,
  a Grafana service-account token is a credential and must not be persisted in
  plaintext config.
- **Existing patterns are strong.** Typed `thiserror` enums (`config.rs`),
  per-server failure backoff (`poller.rs`), tray-level mapping via `MetricLevel`,
  and dedup-via-previous-state for notifications (`prev_levels`) are all in place
  and should be reused rather than reinvented.

## Out of scope (YAGNI)

- Writing or silencing alerts back to Grafana (read-only integration).
- The Prometheus-compatible rules endpoint (`/api/prometheus/grafana/...`).
- Datasource-managed (Prometheus/Mimir) alert rules — only Grafana-managed
  alerts are covered.
- Any inbound webhook receiver / local HTTP server.
- Persisting alert history to disk.

## Grafana API

**Endpoint:** `GET {base_url}/api/alertmanager/grafana/api/v2/alerts`
**Auth:** `Authorization: Bearer <service-account-token>`
**Returns:** a JSON array of Alertmanager v2 `GettableAlert` objects. Relevant
fields per alert:

- `labels` — map; includes `alertname` and (by convention) `severity`.
- `annotations` — map; includes `summary` / `description` by convention.
- `startsAt` — RFC 3339 timestamp string.
- `generatorURL` — link back into Grafana for this alert.
- `fingerprint` — stable per-alert identity; used as the dedup key.
- `status.state` — one of `unprocessed`, `active`, `suppressed`
  (`suppressed` = silenced or inhibited).

> **Implementation note:** capture a real JSON response from the user's Grafana
> as the canonical test fixture before finalizing the parser. The field set above
> is the documented Alertmanager v2 contract; the live instance is the source of
> truth for optional/missing fields.

The Bearer token is created in Grafana under **Administration → Users and access
→ Service accounts**.

## Architecture & data flow

A new backend module `grafana_backend.rs`, parallel to `k8s_backend.rs` and
`ssh_backend.rs`. The poll loop in `poller.rs` gains one extra concurrent task per
cycle: when Grafana is configured and enabled, fetch active alerts, parse them,
and emit a **new** Tauri event `alerts-update` — separate from `metrics-update`.

```
poll cycle:
  ├─ poll_all_servers()  → metrics-update  (existing)
  └─ poll_grafana()      → alerts-update   (new)
        update tray icon = worst_of(self-metrics level, alert severity level)
        notify on newly-firing alert fingerprints
```

Alerts are **not** modeled as a `ServerConfig` variant and **not** crammed into
`ServerMetrics` — they are a distinct kind of signal with a distinct payload.

## Data model

```rust
// config.rs — new optional section on AppConfig
pub struct GrafanaConfig {
    pub name: String,        // keychain reference + display label
    pub url: String,         // base URL, e.g. https://grafana.internal
    pub verify_tls: bool,    // allow opt-out for self-signed (default true)
    pub enabled: bool,
}
// AppConfig gains: #[serde(default)] pub grafana: Option<GrafanaConfig>

// grafana_backend.rs / metrics.rs — new alert types
pub enum AlertSeverity { Critical, Warning, Info, Unknown }  // from "severity" label
pub enum AlertState { Active, Suppressed }                    // suppressed = silenced
// status.state mapping: "suppressed" => Suppressed; "active" AND "unprocessed"
// => Active (unprocessed = firing but not yet routed; treated as active for
// display and tray purposes).

pub struct Alert {
    pub fingerprint: String,
    pub name: String,                       // "alertname" label, fallback "(unnamed)"
    pub severity: AlertSeverity,
    pub state: AlertState,
    pub summary: String,                    // annotations.summary, fallback ""
    pub description: String,                // annotations.description, fallback ""
    pub starts_at: String,
    pub labels: BTreeMap<String, String>,
    pub generator_url: Option<String>,
}

// Event payload to the frontend (serde, lowercase enums to match metrics.rs)
pub struct AlertsUpdate {
    pub alerts: Vec<Alert>,
    pub source_error: Option<String>,       // Some(_) => "Grafana unreachable" in UI
}
```

**Ownership / mutability:** `GrafanaBackend` owns its `reqwest::Client`, base URL,
and the token (loaded from keychain at construction, kept in memory only). It is
cached in the `Poller` like the SSH/k8s backends and reused across cycles. The
`prev_alert_fingerprints: HashSet<String>` lives on the `Poller` and is the only
mutable cross-cycle state for alert-notification dedup.

## Secret handling

The service-account token is stored in the **OS keychain** via the `keyring`
crate, keyed by the Grafana connection `name`. `config.json` stores only the
`GrafanaConfig` (no token). New Tauri commands:

- `set_grafana_token(name, token)` — write to keychain.
- `has_grafana_token(name)` — bool, so the UI can show "configured" without
  reading the secret back.
- `delete_grafana_token(name)` — on disconnect/removal.

`GrafanaBackend::new` reads the token from the keychain; a missing token surfaces
as a typed `GrafanaError::MissingToken` and the UI prompts to (re)enter it.

## Tray + notifications

- Add `severity_to_level(AlertSeverity) -> MetricLevel` (Critical→Crit,
  Warning→Warn, Info/Unknown→Ok). Compute the worst active-alert level and fold
  it into `update_tray_icon` so the tray reflects `worst_of(self-metric level,
  alert level)`. Suppressed (silenced) alerts do not raise the tray level.
- Notification dedup mirrors `prev_levels`: keep `prev_alert_fingerprints`. Fire a
  native notification for each fingerprint newly present this cycle that was
  absent last cycle; optionally fire a "resolved" notification for fingerprints
  that dropped out. Gated by the existing `notifications_enabled` flag.

## UI (frontend, vanilla JS)

- New "Alerts" section in the popover, rendered from the `alerts-update` event:
  each row shows a severity-colored dot, alert name, summary, age (from
  `starts_at`), and key labels. Click opens `generator_url` in the browser
  (reuse the existing terminal/browser-open command pattern).
- When `source_error` is `Some`, show a compact "Grafana unreachable" banner
  instead of a stale list.
- Settings panel: add/edit the Grafana connection (URL, verify-TLS toggle, enable
  toggle) and a token field that calls `set_grafana_token` (never reads it back).

## Error handling & resilience

`GrafanaError` is a typed `#[non_exhaustive]` `thiserror` enum following the
`ConfigError` pattern (variants carry their source in the chain, not a
`format!`-built string):

- `MissingToken` — no token in keychain for this connection.
- `Http { source: reqwest::Error }` — transport/connect failure.
- `Status { code: u16 }` — non-2xx (e.g. 401 bad token, 404 wrong path).
- `Parse { source: serde_json::Error }` — unexpected response shape.

Polling failures surface as `AlertsUpdate.source_error` (UI shows "unreachable")
and the connection participates in the **same failure-backoff** the servers use
(`BACKOFF_THRESHOLD` consecutive failures → back off `BACKOFF_DURATION`), so a
laptop that is offline or off-VPN does not spam notifications or hammer Grafana.

## Testing

- Parser unit tests over captured Alertmanager-v2 JSON fixtures:
  firing/`active`, `suppressed` (silenced), missing `severity` label
  (→ `Unknown`), missing annotations (→ empty strings), missing `alertname`
  (→ fallback), empty array (no alerts).
- `severity_to_level` threshold mapping tests.
- Notification-dedup logic: a fingerprint present two cycles in a row fires once;
  a new fingerprint fires; a dropped fingerprint optionally fires "resolved".
- Backoff behavior reuses the existing poller test patterns.
- Tests colocated per module under `#[cfg(test)]`, per project convention.

## New dependencies

- `reqwest` (async, rustls TLS) — HTTP client for the Grafana API. Likely already
  present transitively via `kube`; confirm and pin during implementation.
- `keyring` — OS keychain access for the token.

## Open items for the implementation plan

- Confirm `reqwest` is reachable as a direct dependency (vs. pulling it in fresh).
- Capture a real Alertmanager-v2 JSON sample from the user's Grafana as the
  canonical fixture.
- Decide poll cadence for Grafana: reuse the existing foreground/background
  interval, or a fixed minimum to avoid rate-limiting the Grafana API.

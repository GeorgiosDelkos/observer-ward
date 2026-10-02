# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Changed

- The `metrics-update` payload is typed per server kind (`kind`, `status`, `metrics` or `error`), with pods nested inside their cluster
- `save_config_cmd` is replaced by `save_settings`, which never touches the server list; terminal commands take a server name instead of connection details
- Config changes are validated (new names unique and without `/`, changed poll intervals in range); the `poll_interval_secs` alias is gone
- Grafana token is cached and re-read only after a change or a 401/403
- CI runs the full `cargo deny check` (licenses, bans, sources, advisories)

### Fixed

- SSH host-key check failed open when `known_hosts` was unreadable or had an unparseable entry for the host
- Tray click and blur grace windows used the wall clock and could swallow clicks after a clock step
- Memory limits stored as milli-bytes (e.g. `1.2Gi` canonicalised by the API server) were ignored
- Pod "last event" could be stale: events are now paged and compared across all timestamp fields
- Pod threshold notifications re-fired every poll
- Fingerprint-less Grafana alerts shared one dedup key
- Pods with only some containers capped showed inflated CPU/memory percentages
- An unparseable config file could be overwritten by defaults on the next save

## [0.1.0] - 2026-09-18

First tagged shape of the app: a macOS menu-bar dashboard for Kubernetes, SSH hosts, and Grafana alerts.

### Added

- Kubernetes cluster and per-pod metrics (Metrics API, restarts, PVC, events)
- SSH host metrics over key-based auth (`top` / `free` / `df` / `/proc/net/dev`)
- Grafana firing-alert ingestion; token in the OS keychain
- Tray popover with refined dark UI, dual poll intervals, autostart, threshold notifications
- In-app remove with confirmation; expanded pod lists capped to three visible rows and sorted A–Z

### Fixed

- macOS 27 tray left-click swallowed by an attached status-item menu; popover now opens on click, Quit lives in the footer
- Expand `~/` in SSH key and kubeconfig paths
- Keep Grafana tray severity across consecutive fetch errors
- Crit tray icon is not replaced by a lifetime pod restart count
- SSH add form no longer blocked by a hidden Kubernetes namespace field
- Window height includes the Grafana alerts section
- IPv6 SSH addresses are bracketed for both polling and Open Terminal

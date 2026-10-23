# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- Night Glass popover: overview (attention list, cluster rings with requested capacity, node matrix, namespace rows, hosts), cluster page (sparkline history, nodes with conditions and their pods, namespaces), and namespace page (pods grouped by workload with a ready count); native macOS blur behind the window, bundled Geist fonts
- Add form picks namespaces from the cluster (`list_namespaces` command) and can watch another namespace of an existing cluster
- UI unit tests for the model and formatting, run in CI with `node --test`
- Per-node reports: ready, cordoned, pressure conditions, roles, kubelet version, allocatable, and CPU/memory/disk usage; a node at a threshold now notifies on its own and colours the tray (one-node clusters count once, as the cluster)
- Requested CPU and memory across the cluster, against allocatable, when pods can be listed in all namespaces
- Each pod reports the node it runs on
- `unwatch_namespace` command
- The pod sheet shows what each usage bar is measured against, for example `1.6c of 16.0c · node` (pod `usage.ceiling.cpu` and `.memory`, each `{ amount, source }` with source `limit`, `node` or `cluster`)
- Pod-level resources (`spec.resources`, Kubernetes 1.34) count as the pod's limits and requests

### Changed

- A Kubernetes entry is one cluster watching several namespaces (`namespaces`, each with its own `hidden`); entries sharing a kubeconfig context merge on load, and adding a known context adds its namespace to that cluster. Nodes are polled once per cluster instead of once per namespace
- `metrics-update` cluster payload: `nodes`, `namespaces` (each with `pods`), and `requested` replace `pods` and `node_count`; notification labels are `cluster node <name>` and `cluster/namespace/pod`
- `hide_workload`, `show_workload` and `open_pod_logs` take a `namespace`
- The `metrics-update` payload is typed per server kind (`kind`, `status`, `metrics` or `error`), with pods nested inside their cluster
- `save_config_cmd` is replaced by `save_settings`, which never touches the server list; terminal commands take a server name instead of connection details
- Config changes are validated (new names unique and without `/`, changed poll intervals in range); the `poll_interval_secs` alias is gone
- Grafana token is cached and re-read only after a change or a 401/403
- Grafana URLs must be https (http only for loopback), so the token never crosses the network in cleartext
- A host key that cannot be written to `known_hosts` is refused instead of accepted unrecorded
- CI runs the full `cargo deny check` (licenses, bans, sources, advisories)

### Fixed

- A pod's CPU and memory percent no longer uses its requests as the ceiling. A pod without a limit is measured against its node, so a busy pod with no limit no longer reads 100% or more and raises a false crit
- Pod limits and cluster requests follow Kubernetes' `PodLimits` and `PodRequests`. Native sidecars add to the app containers, a running init container is measured against its own step, and a zero limit is no limit
- SSH host-key check failed open when `known_hosts` was unreadable or had an unparseable entry for the host, or named the host only in a line russh skips (tabs, wildcards, `@revoked`)
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

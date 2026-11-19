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
- Kubeconfig exec credential plugins (kubectl-bao for OpenBao, aws, gke-gcloud-auth-plugin) work when the app starts from Finder or at login: they get the login shell's PATH, unless the kubeconfig sets one
- `k8s_login` command and a Log in button on a cluster whose credential plugin failed: it runs the plugin interactively (an OpenBao or OIDC sign-in in the browser), then polls the cluster at once, past any backoff. An offline report carries `needs_login`; such a cluster is not polled again until the login, so a plugin that ignores `spec.interactive` (kubectl-bao today) starts at most one browser sign-in per expiry. A poll that times out on a cluster with a credential plugin also offers the login
- A cluster whose credentials may not list nodes (a namespace-scoped account) shows its watched namespaces instead of going offline; its `metrics-update` payload then has no totals, `disk`, `net`, `requested` or `nodes`, and a pod without a limit has no usage rather than 0%

### Changed

- A Kubernetes entry is one cluster watching several namespaces (`namespaces`, each with its own `hidden`); entries sharing a kubeconfig context merge on load, and adding a known context adds its namespace to that cluster. Nodes are polled once per cluster instead of once per namespace
- `metrics-update` cluster payload: `nodes`, `namespaces` (each with `pods`), and `requested` replace `pods` and `node_count`; notification labels are `cluster node <name>` and `cluster/namespace/pod`
- `hide_workload`, `show_workload` and `open_pod_logs` take a `namespace`
- The `metrics-update` payload is typed per server kind (`kind`, `status`, `metrics` or `error`), with pods nested inside their cluster
- `save_config_cmd` is replaced by `save_settings`, which never touches the server list; terminal commands take a server name instead of connection details
- Config changes are validated (new names unique and without `/`, changed poll intervals in range); the `poll_interval_secs` alias is gone
- Grafana token is cached and re-read only after a change or a 401/403
- Polls run exec credential plugins non-interactively (`interactiveMode: Never`), so a background poll never opens a browser to sign in, and a failed plugin reports what it printed instead of kube's dump of its command and environment
- Grafana URLs must be https (http only for loopback), so the token never crosses the network in cleartext
- A host key that cannot be written to `known_hosts` is refused instead of accepted unrecorded
- CI runs the full `cargo deny check` (licenses, bans, sources, advisories)
- kube 5.1 (was 3.0), k8s-openapi 0.29 (was 0.27) and dirs 7 (was 6). kube parses kubeconfig files with serde_saphyr instead of the deprecated serde_yaml
- Tauri 2.12 (was 2.11), which brings tray-icon 0.25.1 and the dialog, notification, clipboard and autostart plugins up to date
- Grafana requests use reqwest 0.13 (was 0.12), the version Tauri already links, and verify certificates against the macOS trust store instead of the bundled webpki roots, so a CA trusted in Keychain Access now works
- TLS (kube, Grafana) uses aws-lc-rs, which russh already links, instead of ring, so the build carries one crypto library
- Rust toolchain pinned to 1.99.1 (was 1.98.0), which picks up the 1.98.1 fix for a vtable miscompile that crashed boxed async trait objects on aarch64-apple-darwin, and the 1.99.1 fix for x86_64 `extern "C"` functions returning `bool`

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

### Security

- russh 0.65.2 (was 0.60.3): fixes RUSTSEC-2026-0492 to -0495 and the moderate and low russh advisories, including GHSA-9264-jvgf-6w2f (key-exchange callback ran before the host key was checked). Host keys are checked against `known_hosts` as before; a host certificate is refused
- crypto-bigint 0.7.11 fixes RUSTSEC-2026-0387 (secret-dependent timing in RSA and ECDSA arithmetic)
- rsa 0.10.0 (via russh) fixes RUSTSEC-2023-0071 (Marvin timing side-channel); its `deny.toml` ignore is gone
- tokio 1.54 (was 1.52) fixes RUSTSEC-2026-0386 (`AsyncFd::new` violated I/O safety)
- async-executor 1.14.1 replaces the yanked 1.14.0

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

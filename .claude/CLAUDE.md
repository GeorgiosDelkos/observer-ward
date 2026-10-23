# Observer Ward

Tray-based Kubernetes and SSH monitoring dashboard built with Tauri 2 + vanilla JS.

## Quick Reference

| Component | Path | Language |
|-----------|------|----------|
| Backend | `src-tauri/src/` | Rust |
| Frontend | `ui/` | JS/HTML/CSS |
| Config file | `~/Library/Application Support/observer-ward/config.json` (macOS) | JSON |
| Tauri config | `src-tauri/tauri.conf.json` | JSON |

## Build & Test

```sh
cd src-tauri
cargo test                                          # run all tests
cargo fmt --all -- --check                          # format check
cargo clippy --all-targets --all-features -- -D warnings  # lint

# from repo root (not src-tauri):
cargo tauri dev                                     # run in dev mode
cargo tauri build                                   # production build
```

## Architecture

**Backend modules:**

- `lib.rs` -- crate root, Tauri builder, `run()`
- `commands.rs` -- Tauri IPC commands
- `tray.rs` -- tray icon, popover show/hide, blur grace
- `terminal.rs` -- open ssh/kubectl in Warp or Terminal.app
- `config.rs` -- `AppConfig`/`ServerConfig` models, `ConfigStore` (validated, serialized writes), JSON persistence
- `error.rs` -- error-chain formatting at the Tauri command boundary
- `metrics.rs` -- `ServerReport` (per-kind `Health<HostMetrics|ClusterMetrics>`, clusters hold `nodes` and `namespaces` with their pods), levels, alert types
- `poller.rs` -- async poll loop with failure tracking and backoff
- `k8s.rs` + `k8s/` -- one backend per cluster: Metrics API + kubelet stats, per-node reports (`k8s/nodes.rs`), cluster-wide requests (`k8s/requests.rs`), pods per watched namespace
- `ssh.rs` + `ssh/` -- SSH remote command parsing (top/free/df/proc)
- `grafana.rs` -- Grafana Alertmanager firing alerts; token in Keychain

**Frontend:** Vanilla JS as native ES modules, no build step. `ui/js/model.js` holds state and derived health (no DOM); `view.js` renders the overview, cluster and namespace pages as HTML strings with `data-action` attributes that `app.js` dispatches; `forms.js` owns the add and settings pages (a metrics update never re-renders a form). Receives `metrics-update`, `poll-start`, and `alerts-update` events. Pure modules are tested with `node --test tests/ui/*.test.mjs`.

**Design ("Night Glass"):** structure (row heights, window inset, corners, controls) on a 4px grid and inner spacing on an even 2px scale; all text on one 22px edge (8px page inset + 14px card padding); concentric corners (window 24, card 16, inner list 10, control 8); borders as inset box-shadows so they never shift the text edge; colour only for state (mint/amber/coral) and iris for interactive. Native macOS blur comes from `windowEffects` (`hudWindow`) in `tauri.conf.json`. Fonts are bundled in `ui/fonts/` because the CSP only allows `'self'`. Verify layout changes visually before calling them done.

Poll intervals: foreground (popover open, default 10s) and background (hidden, default 300s). Do not attach an NSMenu to the tray icon (macOS 27 swallows left-click). Remove confirmation is an in-window overlay, never `window.confirm`. A `K8sTarget` is one cluster (kubeconfig + context) watching several namespaces; legacy one-namespace entries merge on load (`AppConfig::merge_clusters`). Pods are hidden by owning workload per namespace (`WatchedNamespace.hidden`), filtered in the poller so hidden pods never reach the tray, notifier, or UI.

**Data flow:** Poll loop -> collect metrics per server -> emit Tauri event -> frontend re-renders.

## Conventions

- Strict clippy: `unwrap`, `panic`, `todo`, `dbg!`, `print` are denied
- `tracing` for logging (no `println!`)
- `ServerConfig` is a tagged enum (`type` field in JSON)
- Metric thresholds: green < 60%, amber 60-85%, red >= 85%
- No `..` imports. `super` is allowed inside a concept module (`k8s/`, `ssh/`) for siblings. No wildcard matches on crate enums.
- Tests colocated in each module (`#[cfg(test)]`)

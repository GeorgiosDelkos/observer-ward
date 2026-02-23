# Observer Ward

Tray-based Kubernetes and SSH monitoring dashboard built with Tauri 2 + vanilla JS.

## Quick Reference

| Component | Path | Language |
|-----------|------|----------|
| Backend | `src-tauri/src/` | Rust |
| Frontend | `ui/` | JS/HTML/CSS |
| Config file | `~/.config/observer-ward/config.json` | JSON |
| Tauri config | `src-tauri/tauri.conf.json` | JSON |

## Build & Test

```sh
cd src-tauri
cargo test                                          # run all tests
cargo fmt -- --check                                # format check
cargo clippy --all-targets --all-features -- -D warnings  # lint
cargo tauri dev                                     # run in dev mode (from repo root)
cargo tauri build                                   # production build (from repo root)
```

## Architecture

**Backend modules:**

- `lib.rs` -- Tauri setup, tray icon, window management, commands
- `config.rs` -- `AppConfig`/`ServerConfig` models, JSON persistence
- `metrics.rs` -- `ServerMetrics`/`ServerStatus` data types
- `poller.rs` -- async poll loop with failure tracking and backoff
- `k8s_backend.rs` -- Kubernetes Metrics API + kubelet stats
- `ssh_backend.rs` -- SSH remote command parsing (top/free/df/proc)

**Frontend:** Single-page vanilla JS app. No build step. State in module-level variables, renders server/pod cards with color-coded metric bars. Receives `metrics-update` and `poll-start` events from the backend.

**Data flow:** Poll loop -> collect metrics per server -> emit Tauri event -> frontend re-renders.

## Conventions

- Strict clippy: `unwrap`, `panic`, `todo`, `dbg!`, `print` are denied
- `tracing` for logging (no `println!`)
- `ServerConfig` is a tagged enum (`type` field in JSON)
- Metric thresholds: green < 60%, amber 60-85%, red >= 85%
- No relative imports, no wildcard matches
- Tests colocated in each module (`#[cfg(test)]`)

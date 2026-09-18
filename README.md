# Observer Ward

A macOS menu-bar dashboard for Kubernetes clusters, SSH hosts, and Grafana alerts. Built with [Tauri](https://tauri.app/) 2 and vanilla JavaScript.

Observer Ward lives in the menu bar (no Dock icon). It polls your infrastructure on a foreground/background schedule and shows CPU, memory, disk, network, pod health, and firing Grafana alerts in a compact popover.

## Features

- **Kubernetes** — cluster and per-pod metrics via the Metrics API (restarts, age, PVC usage, recent events)
- **SSH hosts** — CPU, memory, disk, and network over key-based SSH
- **Grafana alerts** — currently firing alerts, severity-colored rows, native notifications
- **Tray-native** — click the tray icon to open, click away to dismiss, Quit in the popover footer
- **Failure backoff** — unreachable targets back off so a dead host does not spam polls
- **Connection pooling** — SSH and Kubernetes clients are reused across cycles
- **Dual poll intervals** — faster while the popover is open, slower in the background
- **Launch at login** — optional autostart
- **Refined dark UI** — color-coded thresholds (green &lt; 60%, amber 60–85%, red ≥ 85%)

## Requirements

- macOS (Apple Silicon and Intel)
- [Rust](https://rustup.rs/) (stable; this repo pins 1.95.0 via `src-tauri/rust-toolchain.toml`)
- [Tauri CLI](https://v2.tauri.app/) v2
- Xcode Command Line Tools: `xcode-select --install`

The frontend is static files in `ui/`. Node.js is not required.

## Install from source

```sh
git clone https://github.com/GeorgiosDelkos/observer-ward.git
cd observer-ward
cargo install tauri-cli --version "^2"
cargo tauri build --bundles dmg
```

The disk image is written to `src-tauri/target/release/bundle/dmg/`. Drag **Observer Ward.app** to Applications.

This build is ad-hoc signed. On first launch, Control-click the app and choose **Open**, or:

```sh
xattr -cr "/Applications/Observer Ward.app"
```

### Development

```sh
cargo tauri dev
```

Edits under `ui/` are picked up on the next window load (there is no bundler or hot-reload server).

## Usage

Click the tray icon to open the popover. Click outside it to dismiss.

### Add a Kubernetes cluster

1. Click **+**
2. Leave **Type** on **Kubernetes**
3. Fill in **Name**, **Context**, and **Namespace**. Leave kubeconfig blank to use `~/.kube/config`.
4. Click **Add**

The cluster needs [Metrics Server](https://github.com/kubernetes-sigs/metrics-server) and a kubeconfig that can read nodes, pods, events, and the metrics API.

Expanded pod lists show three pods at a time (scroll for the rest) and are sorted A–Z.

### Add an SSH server

1. Click **+** and choose **SSH**
2. Fill in **Name**, **Host**, **Port**, **User**, and **Key path** (for example `~/.ssh/id_ed25519`)
3. Click **Add**

Password auth is not supported. The remote host must provide `top`, `free`, `df`, and `/proc/net/dev`.

### Grafana alerts

1. In Grafana, create a service-account token that can read alerts.
2. Open **Settings**, enable **Grafana alerts**, enter the base URL, and paste the token.
3. Save. Firing alerts appear on the next poll.

The token is stored in the macOS Keychain, never in `config.json`. Observer Ward only reads alerts (it never silences them). Alerts must be Grafana-managed; the client calls `/api/alertmanager/grafana/api/v2/alerts`.

To rotate the token, paste the new value in Settings and Save. The poller re-reads the keychain each cycle.

### Settings

Footer **Settings**:

- **Foreground interval** — poll period while the popover is open (5–120 s, default 10)
- **Background interval** — poll period while hidden (30–600 s, default 300)
- **Launch at login**
- **Threshold alerts** — native notifications when a metric crosses amber/red
- **Grafana** — enable, URL, TLS verification, API token

### Remove a cluster or server

Click **remove** on the card (or right-click → Remove). Confirm in the in-app dialog. This cannot be undone; add the target again if you still need it.

## Configuration

`~/.config/observer-ward/config.json`

```json
{
  "foreground_poll_secs": 10,
  "background_poll_secs": 300,
  "notifications_enabled": false,
  "grafana": {
    "name": "default",
    "url": "https://grafana.internal",
    "verify_tls": true,
    "enabled": true
  },
  "servers": [
    {
      "type": "k8s",
      "name": "production",
      "context": "prod-ctx",
      "namespace": "default",
      "kubeconfig": null
    },
    {
      "type": "ssh",
      "name": "web-server-1",
      "host": "10.0.1.50",
      "port": 22,
      "user": "deploy",
      "key_path": "~/.ssh/id_ed25519"
    }
  ]
}
```

`poll_interval_secs` is still accepted as an alias for `foreground_poll_secs`. The Grafana token is not in this file.

## Architecture

```
observer-ward/
├── src-tauri/              # Rust / Tauri
│   └── src/
│       ├── lib.rs              # App setup, tray, commands
│       ├── config.rs           # Config models and persistence
│       ├── error.rs            # Error-chain formatting
│       ├── metrics.rs          # Metric and alert types
│       ├── poller.rs           # Poll loop, backoff, tray icon
│       ├── k8s_backend.rs      # Kubernetes Metrics API + kubelet stats
│       ├── ssh_backend.rs      # SSH remote command parsing
│       └── grafana_backend.rs  # Grafana Alertmanager alerts
└── ui/                     # Vanilla JS / HTML / CSS
    ├── index.html
    ├── app.js
    └── styles.css
```

The poll loop collects every server in parallel, then emits Tauri events. The UI re-renders cards from those events.

## Development

```sh
cd src-tauri
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo deny check advisories
```

Clippy denies `unwrap`, `panic`, `todo`, and `dbg!` in production code. Logging uses `tracing`.

## Security

See [SECURITY.md](SECURITY.md). Report vulnerabilities via GitHub private advisories.

## License

[MIT](LICENSE)

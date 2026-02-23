# Observer Ward

A lightweight tray-based monitoring dashboard for Kubernetes clusters and SSH servers. Built with [Tauri](https://tauri.app/) and vanilla JavaScript.

Observer Ward lives in your menu bar, polling your infrastructure at configurable intervals and displaying real-time CPU, memory, disk, and network metrics in a compact popover window.

## Features

- **Kubernetes monitoring** -- cluster-level and per-pod metrics via the Metrics API
- **SSH server monitoring** -- collects CPU, memory, disk, and network stats over SSH
- **Tray-native** -- no dock icon, click the tray to open, click away to dismiss
- **Pod details** -- status badges, restart counts, age, PVC usage, recent events
- **Failure backoff** -- unreachable servers back off automatically to avoid noise
- **Connection pooling** -- reuses SSH and Kubernetes connections across poll cycles
- **Configurable poll interval** -- 5 to 300 seconds
- **Launch at login** -- optional autostart via system integration
- **Dark neon UI** -- Tron-inspired theme with color-coded metric thresholds

## Screenshots

<!-- TODO: add screenshots -->

## Prerequisites

- **Rust** (stable, via [rustup](https://rustup.rs/))
- **Node.js** 18+ (for Tauri CLI)
- **Tauri CLI** v2

### Platform-specific

**macOS:** Xcode Command Line Tools

```sh
xcode-select --install
```

**Linux:** system dependencies for Tauri -- see the [Tauri prerequisites guide](https://v2.tauri.app/start/prerequisites/).

## Installation

### From source

```sh
git clone https://github.com/GeorgiosDelkos/observer-ward.git
cd observer-ward
cargo install tauri-cli --version "^2"
cargo tauri build
```

The built application bundle is in `src-tauri/target/release/bundle/`.

### Development

```sh
cargo tauri dev
```

This starts the app in development mode with hot-reload for the frontend.

## Usage

### Adding a Kubernetes cluster

1. Click the tray icon to open the popover
2. Click the **+** button
3. Select **Kubernetes** as the server type
4. Fill in:
   - **Name** -- display label for this cluster
   - **Context** -- kubectl context name (required)
   - **Namespace** -- namespace to monitor pods in (required)
   - **Kubeconfig** -- path to kubeconfig file (leave blank for `~/.kube/config`)
5. Click **Add**

Requirements:
- The [Metrics Server](https://github.com/kubernetes-sigs/metrics-server) must be installed in the cluster
- The kubeconfig must have permissions to read nodes, pods, events, and the metrics API

### Adding an SSH server

1. Click **+** and select **SSH**
2. Fill in:
   - **Name** -- display label
   - **Host** -- hostname or IP
   - **Port** -- SSH port (default: 22)
   - **User** -- SSH username
   - **Key path** -- path to private key (e.g., `~/.ssh/id_ed25519`)
3. Click **Add**

Requirements:
- Key-based authentication (password auth is not supported)
- The remote server must have `top`, `free`, `df`, and `/proc/net/dev` available (standard on Linux)

### Settings

Click the gear icon in the footer to adjust:

- **Poll interval** -- how often to collect metrics (5--300 seconds, default: 30)
- **Launch at login** -- start Observer Ward automatically on system boot

### Removing a server

Right-click any server card and select **Remove**.

## Configuration

Configuration is stored at:

```
~/.config/observer-ward/config.json
```

Example:

```json
{
  "poll_interval_secs": 30,
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

## Architecture

```
observer-ward/
├── src-tauri/          # Rust backend (Tauri)
│   └── src/
│       ├── lib.rs          # App setup, tray, Tauri commands
│       ├── config.rs       # Configuration models and persistence
│       ├── metrics.rs      # Metric data types
│       ├── poller.rs       # Poll loop orchestration with backoff
│       ├── k8s_backend.rs  # Kubernetes metrics collection
│       └── ssh_backend.rs  # SSH metrics collection
└── ui/                 # Frontend (vanilla JS/HTML/CSS)
    ├── index.html
    ├── app.js
    └── styles.css
```

The backend spawns an async poll loop that collects metrics from all configured servers in parallel, then emits Tauri events to the frontend. The frontend renders metric cards with color-coded bars (green < 60%, amber 60--85%, red >= 85%).

## Development

### Running tests

```sh
cd src-tauri
cargo test
```

### Linting

```sh
cd src-tauri
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

The project enforces strict clippy lints including denying `unwrap`, `panic`, `todo`, and `dbg!` in production code.

## License

MIT

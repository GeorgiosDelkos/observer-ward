# Observer Ward

A macOS menu-bar dashboard for Kubernetes clusters, SSH hosts, and Grafana alerts. Built with [Tauri](https://tauri.app/) 2 and vanilla JavaScript.

Observer Ward lives in the menu bar (no Dock icon). It polls your infrastructure on a foreground/background schedule and shows CPU, memory, disk, network, pod health, and firing Grafana alerts in a compact popover.

## Features

- **Kubernetes** — one entry per cluster watching any number of namespaces: cluster totals and requested capacity, per-node health and usage, and per-pod metrics grouped by workload (restarts, age, PVC usage, recent events)
- **SSH hosts** — CPU, memory, disk, and network over key-based SSH
- **Grafana alerts** — firing alerts in the "needs attention" list, native notifications
- **Tray-native** — click the tray icon to open, click away to dismiss, Quit in the popover footer
- **Failure backoff** — unreachable targets back off so a dead host does not spam polls
- **Connection pooling** — SSH and Kubernetes clients are reused across cycles
- **Dual poll intervals** — faster while the popover is open, slower in the background
- **Launch at login** — optional autostart
- **Night Glass UI** — dark glass over native macOS blur, drill-down pages (overview, cluster, namespace), a "needs attention" list, color-coded thresholds (green &lt; 60%, amber 60–85%, red ≥ 85%)

## Requirements

- macOS (Apple Silicon and Intel)
- [Rust](https://rustup.rs/) (stable; this repo pins 1.98.0 via `rust-toolchain.toml`)
- [Tauri CLI](https://v2.tauri.app/) v2
- Xcode Command Line Tools: `xcode-select --install`

The frontend is static files in `ui/` (native ES modules, no bundler). Node.js is only needed to run the UI unit tests.

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

The popover has three levels, and the back button (or Escape) goes up one:

- **Overview** — anything that needs attention first, then a card per cluster and per SSH host. A cluster card shows CPU, memory, and disk as rings (the faint arc is what pods have requested), one cell per node coloured by health (click one to open it), and a row per watched namespace.
- **Cluster** — CPU, memory, and disk over the recent polls, then **Nodes** (ready, cordoned, pressure, roles, kubelet version, allocatable, usage, and which of your pods run there) or **Namespaces**.
- **Namespace** — pods grouped by the workload that owns them, with a ready count per workload. Click a pod for its usage against its limits, node, volume, network, last event, **Logs**, and **Hide**.

**Needs attention** lists offline sources, nodes that are not ready or under pressure, pods that are failing or restarted in the last hour, anything at 85% or more, and firing Grafana alerts. Each item opens the page that shows it.

Double-click a cluster or host name to give it a display name. The **⋯** menus hold copy, terminal, and remove actions.

### Add a Kubernetes cluster

1. Click **+** and keep **Kubernetes**
2. Name the cluster. Leave kubeconfig blank to use `~/.kube/config`, or click **Browse** to pick a file. **Context** suggests the kubeconfig's contexts and is prefilled with its current one.
3. Pick one or more namespaces from the list (or type one and press Enter), then click **Add**

Adding checks the kubeconfig can be read and the context resolves to a cluster, so a mistyped path or context is reported in the form rather than saved. If a cluster goes offline later, its card shows why; **⋯ → Copy error** copies the full message.

A cluster is one entry however many namespaces it watches. Choosing a context that is already configured adds the namespaces to that cluster (the form says which). To watch another namespace later, use **+ Watch another namespace** on the cluster page; to stop watching one, use **⋯ → Stop watching** on its page (a cluster keeps at least one; remove the cluster to drop the last).

The cluster needs [Metrics Server](https://github.com/kubernetes-sigs/metrics-server) and a kubeconfig that can read nodes, pods, events, and the metrics API (plus `list` on Jobs, to hide CronJobs). With `list` on pods in all namespaces, the cluster also shows how much CPU and memory its pods request; with `list` on namespaces, the add form offers them. Without either, that part is left out.

### Hide pods

Open a pod and click **Hide**. This hides the workload that owns the pod (its Deployment, StatefulSet, DaemonSet, CronJob, or Argo Rollout; a pod with no controller is hidden by name), so it stays hidden across restarts and rollouts. Hidden pods are left out of the list, the tray icon, and threshold notifications; Grafana alerts about them still fire. Nothing is deleted from the cluster.

Workloads are hidden per namespace. The namespace page then shows **N hidden workloads**; click it and choose **Show** to track one again. Hidden pods are skipped as soon as they are listed, so no per-pod work is done for them.

CronJob pods are matched to their CronJob through one list of the namespace's Jobs, refreshed only when a new run appears. Without permission to list Jobs, runs of a hidden CronJob are matched by their default name (`<cronjob>-<digits>`) instead.

### Add an SSH host

1. Click **+** and choose **SSH host**
2. Fill in **Name**, **Host**, **Port**, **User**, and **Private key** (for example `~/.ssh/id_ed25519`)
3. Click **Add**

Password auth is not supported. The remote host must provide `top`, `free`, `df`, and `/proc/net/dev`.

### Grafana alerts

1. In Grafana, create a service-account token that can read alerts.
2. Open **Settings**, turn on **Show firing alerts**, enter the base URL, and paste the token.
3. Save. Firing alerts appear under **Needs attention** on the next poll.

The token is stored in the macOS Keychain, never in `config.json`. Observer Ward only reads alerts (it never silences them). Alerts must be Grafana-managed; the client calls `/api/alertmanager/grafana/api/v2/alerts`. The URL must use `https://` (plain `http://` is accepted only for `localhost` and loopback addresses), because every request carries the API token.

To rotate the token, paste the new value in Settings and Save. The poller re-reads the keychain each cycle.

### Settings

The gear in the footer opens Settings:

- **While open** — poll period while the popover is open (5–120 s, default 10)
- **While hidden** — poll period while it is hidden (30–600 s, default 300)
- **Open at login**
- **Notify at thresholds** — native notifications when a metric crosses amber/red
- **Grafana alerts** — on/off, URL, API token, TLS verification

### Remove a cluster or host

Use **⋯ → Remove** on the cluster page or the host card and confirm. This cannot be undone; add the target again if you still need it.

## Configuration

`~/Library/Application Support/observer-ward/config.json` on macOS (`~/.config/observer-ward/config.json` on Linux)

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
      "kubeconfig": null,
      "namespaces": [
        { "name": "default" },
        { "name": "s3", "hidden": [{ "kind": "CronJob", "name": "sweep" }] }
      ]
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

Files from before clusters held several namespaces have one `namespace` (and `hidden`) per entry; entries that share a kubeconfig and context are merged into the first on load, which keeps its name, and the file is rewritten in the new shape on the next save.

Every change made through the app is validated: new server names must be unique and must not contain `/`, a changed namespace list must be non-empty with unique names that do not contain `/`, and changed intervals must be 5–120 s (`foreground_poll_secs`) and 30–600 s (`background_poll_secs`). Existing entries that break these rules still load and do not block other edits. If the file cannot be read or parsed, the app starts with defaults and first copies it to `config.json.invalid`. The Grafana token is not in this file.

## Architecture

```
observer-ward/
├── rust-toolchain.toml     # Pins rustc 1.98.0 (also under src-tauri/)
├── src-tauri/              # Rust / Tauri
│   └── src/
│       ├── lib.rs              # Crate root: Tauri builder, run()
│       ├── commands.rs         # IPC commands
│       ├── tray.rs             # Tray icon, popover show/hide
│       ├── terminal.rs         # Open ssh/kubectl in Warp or Terminal
│       ├── config.rs           # Config models and persistence
│       ├── error.rs            # Error-chain formatting
│       ├── metrics.rs          # Per-kind report types, levels, alerts
│       ├── poller.rs           # Poll loop, backoff, tray icon
│       ├── k8s.rs / k8s/       # Per-cluster collector: nodes, requests, pods per namespace
│       ├── ssh.rs / ssh/       # SSH session, TOFU, command parsers
│       └── grafana.rs          # Grafana Alertmanager alerts
└── ui/                     # Vanilla JS / HTML / CSS, native ES modules
    ├── index.html
    ├── app.js              # Navigation, actions, menus, sheets, Tauri events
    ├── styles.css          # Night Glass: 4px grid, 22px text edge, concentric corners
    ├── fonts/              # Geist and Geist Mono (OFL), bundled for the CSP
    └── js/
        ├── format.js       # Number, size, age and name formatting
        ├── model.js        # State, history, restarts, health, attention list
        ├── view.js         # Overview, cluster and namespace pages
        └── forms.js        # Add and settings pages
```

The poll loop collects every server in parallel, then emits Tauri events. The UI folds them into its model and re-renders the current page.

## Development

```sh
cd src-tauri
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo deny check
cd .. && node --test "tests/ui/*.test.mjs"   # UI model and formatting
```

Clippy denies `unwrap`, `panic`, `todo`, and `dbg!` in production code. Logging uses `tracing`.

## Security

See [SECURITY.md](SECURITY.md). Report vulnerabilities via GitHub private advisories.

## License

[MIT](LICENSE)

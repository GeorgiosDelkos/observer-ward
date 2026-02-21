# Observer Ward — Design Document

macOS menu bar server monitoring app. Tron-inspired dark neon UI with live metric bars for Kubernetes clusters and SSH servers.

## Architecture

Three layers:

1. **Tauri v2 backend (Rust)** — Owns all logic: SSH connections via `russh`, Kubernetes API via `kube` crate, config management, metric polling. Emits updates to frontend via Tauri events.

2. **Frontend (HTML/CSS/JS)** — Renders the popover window. Receives metric updates via Tauri event listeners. Sends commands (add/remove server, refresh) via Tauri `invoke`. No framework — vanilla JS.

3. **Config file** (`~/.config/observer-ward/config.json`) — Stores server list, connection details, and settings.

### Data Flow

- A background Tokio task runs a 30-second poll loop
- For each server, it collects metrics via the appropriate backend (SSH or k8s API)
- Parsed metrics are sent to the frontend via `app.emit("metrics-update", payload)`
- Frontend updates bars in-place via DOM manipulation
- Each server is polled independently — one failure does not block others

## UI Design

### Menu Bar

- Monochrome template icon (adapts to light/dark mode)
- Warning dot overlay when any server is unreachable
- Click opens a popover window anchored to the icon

### Popover Window (~320px wide, up to ~500px tall, scrollable)

**Visual style:**

- Background: near-black (#0a0a0f) with dark blue (#0d1117) gradient
- Primary accent: cyan/teal neon (#00fff0)
- Warning accent: hot pink (#ff2d6f) for high usage / errors
- Medium accent: amber (#ffb800) for moderate usage
- Text: light cyan (#b0fbff) for labels, bright white for values
- Borders: 1px dim cyan (#00fff040) with subtle glow
- Font: monospace (JetBrains Mono or system mono)

**Metric bars:**

- Thin horizontal bars with CSS neon glow (`box-shadow`)
- Fill gradient: dark teal to bright cyan with glowing leading edge
- Color thresholds: cyan (0-60%) → amber (60-85%) → pink/red (85%+)
- Subtle animated pulse on the bar edge

**Layout:**

```
┌─────────────────────────────────┐
│  ◈ OBSERVER WARD         [+ ⊕] │
│─────────────────────────────────│
│ ◉ prod-cluster          k8s    │
│   CPU  ▰▰▰▰▰▰▰▰▱▱▱▱▱  62%    │
│   MEM  ▰▰▰▰▰▰▱▱▱▱▱▱▱  45%    │
│   DISK ▰▰▰▰▰▰▰▰▰▰▰▰▱  89%    │
│   NET  ↑ 12 MB/s  ↓ 45 MB/s   │
│─────────────────────────────────│
│ ◉ staging-vm            ssh    │
│   CPU  ▰▰▰▰▱▱▱▱▱▱▱▱▱  28%    │
│   MEM  ▰▰▰▰▰▰▰▰▰▰▱▱▱  78%    │
│   DISK ▰▰▰▰▰▱▱▱▱▱▱▱▱  38%    │
│   NET  ↑ 2 MB/s   ↓ 8 MB/s   │
│─────────────────────────────────│
│ ⚠ dev-box        ssh · offline │
│─────────────────────────────────│
│           [⚙ SETTINGS]         │
└─────────────────────────────────┘
```

**Interactions:**

- [+ ⊕] button opens an add-server form (inline panel)
- Right-click server row → Edit / Remove / Refresh Now
- Server status dots glow green (healthy) or pink (failing)
- Offline servers dim to dark gray with pulsing pink warning icon
- Settings: poll interval, launch at login

### Add Server Form

Dark panel, neon-bordered input fields that glow cyan on focus.

Fields: Name, Type (dropdown: Kubernetes / SSH), Host or Context, Port (SSH only), User (SSH only), Key path (SSH) or Kubeconfig path (k8s).

## Metric Collection

### Kubernetes Backend

- Uses `kube` crate (not kubectl CLI) — reads kubeconfig natively, typed API responses
- CPU/Memory: metrics API (`kubectl top nodes` equivalent)
- Disk: kubelet stats summary API (`/api/v1/nodes/<node>/proxy/stats/summary`)
- Network: same kubelet stats endpoint, `rxBytes`/`txBytes` diffed between polls for rate
- Kubeconfig path from config, defaults to `~/.kube/config`

### SSH Backend

- Uses `russh` crate — pure Rust SSH, no shelling out
- Single compound command per poll:
  ```
  top -bn1 | head -5; free -b; df -B1 /; cat /proc/net/dev
  ```
- CPU: parsed from `top` %Cpu line
- Memory: parsed from `free` output
- Disk: parsed from `df` output
- Network: parsed from `/proc/net/dev`, diffed between polls for rate
- Auth: SSH key files only (path in config)
- Connection pooling: keeps SSH session open between polls, reconnects on failure

### Error Handling

- Each server polled independently
- Failed servers show "offline" state with last-known metrics grayed out
- After 3 consecutive failures, poll interval backs off to 2 minutes
- Connection errors shown as warning icon with tooltip in the UI

## Config Format

```json
{
  "poll_interval_secs": 30,
  "servers": [
    {
      "name": "prod-cluster",
      "type": "k8s",
      "kubeconfig": "~/.kube/config",
      "context": "prod-ctx"
    },
    {
      "name": "staging-vm",
      "type": "ssh",
      "host": "10.0.1.50",
      "port": 22,
      "user": "admin",
      "key_path": "~/.ssh/id_ed25519"
    }
  ]
}
```

Location: `~/.config/observer-ward/config.json`

## Dependencies

| Crate | Purpose |
|-------|---------|
| `tauri` v2 | App framework, system tray, webview popover |
| `tokio` | Async runtime for poll loops and SSH |
| `russh` | Pure Rust SSH client |
| `serde` + `serde_json` | Config serialization |
| `kube` + `k8s-openapi` | Kubernetes API client |
| `tracing` | Structured logging |
| `dirs` | Resolve ~/.config path |

Frontend: vanilla HTML/CSS/JS (no framework).

## Implementation Order

### Step 1: Scaffold Tauri v2 project

Replace the bare Rust project with a Tauri v2 app. Set up system tray icon, popover window, basic HTML shell with Tron dark theme CSS.

### Step 2: Config system

Serde structs for server config. Load/save from `~/.config/observer-ward/config.json`. Tauri commands (`get_config`, `save_config`) exposed to frontend.

### Step 3: Frontend UI

Server list component, metric bar elements with neon glow CSS, add/remove server forms, settings panel. Wire up Tauri event listeners for `metrics-update`.

### Step 4: SSH metric backend

`russh` connection management, command execution, parsing output from `top`/`free`/`df`/`/proc/net/dev` into structured `ServerMetrics`.

### Step 5: Kubernetes metric backend

`kube` crate client setup, fetch node metrics via metrics API, kubelet stats API for disk/network. Parse into same `ServerMetrics` struct.

### Step 6: Poll loop

Tokio background task polling all servers every 30s. Emit `metrics-update` events. Independent error handling per server with exponential backoff.

### Step 7: Polish

Tray icon warning dot for offline servers. Launch-at-login option via `tauri-plugin-autostart`. Bar color transitions. Connection error tooltips.

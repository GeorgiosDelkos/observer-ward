# Observer Ward Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a macOS menu bar server monitoring app with Tron-inspired neon UI that shows live CPU/memory/disk/network metrics from Kubernetes clusters and SSH servers.

**Architecture:** Tauri v2 (Rust backend + vanilla HTML/CSS/JS frontend). Backend polls servers every 30s via `kube` crate (k8s) and `russh` (SSH), emits metric events to the frontend. Config stored as JSON in `~/.config/observer-ward/`.

**Tech Stack:** Tauri 2.10, Tokio, russh, kube, serde, tracing. Frontend: vanilla HTML/CSS/JS with Tron dark neon theme.

---

### Task 1: Restructure into Tauri v2 project

**Files:**
- Delete: `src/main.rs`, `Cargo.toml`
- Create: `src-tauri/Cargo.toml`
- Create: `src-tauri/build.rs`
- Create: `src-tauri/tauri.conf.json`
- Create: `src-tauri/src/main.rs`
- Create: `src-tauri/src/lib.rs`
- Create: `src-tauri/capabilities/default.json`
- Create: `src-tauri/icons/icon.png` (placeholder)
- Create: `ui/index.html` (minimal shell)

**Step 1: Remove old project files**

```bash
rm -f Cargo.toml src/main.rs
rmdir src
```

**Step 2: Create Tauri project structure**

```bash
mkdir -p src-tauri/src src-tauri/icons src-tauri/capabilities ui
```

**Step 3: Create `src-tauri/Cargo.toml`**

```toml
[package]
name = "observer-ward"
version = "0.1.0"
edition = "2021"

[build-dependencies]
tauri-build = { version = "2.0", features = [] }

[dependencies]
tauri = { version = "2.10", features = ["tray-icon"] }
tauri-plugin-positioner = { version = "2.3", features = ["tray-icon"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
tracing-subscriber = "0.3"

[lints.clippy]
pedantic = { level = "warn", priority = -1 }
unwrap_used = "deny"
expect_used = "warn"
panic = "deny"
panic_in_result_fn = "deny"
unimplemented = "deny"
allow_attributes = "deny"
dbg_macro = "deny"
todo = "deny"
print_stdout = "deny"
print_stderr = "deny"
await_holding_lock = "deny"
large_futures = "deny"
exit = "deny"
mem_forget = "deny"
module_name_repetitions = "allow"
similar_names = "allow"
```

**Step 4: Create `src-tauri/build.rs`**

```rust
fn main() {
    tauri_build::build();
}
```

**Step 5: Create `src-tauri/src/main.rs`**

```rust
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    observer_ward_lib::run();
}
```

Note: We use `observer_ward_lib` because Cargo converts the package name `observer-ward` to `observer_ward` for the lib crate, but since we'll set `[lib] name = "observer_ward_lib"` explicitly to avoid ambiguity, use that.

Actually, add to `src-tauri/Cargo.toml`:

```toml
[lib]
name = "observer_ward_lib"
crate-type = ["staticlib", "cdylib", "rlib"]
```

**Step 6: Create `src-tauri/src/lib.rs`**

```rust
use tauri::{
    Manager,
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};
use tauri_plugin_positioner::{Position, WindowExt};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt::init();

    tauri::Builder::default()
        .plugin(tauri_plugin_positioner::init())
        .setup(|app| {
            let _tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().expect("no default icon").clone())
                .menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);

                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            if window.is_visible().unwrap_or(false) {
                                let _ = window.hide();
                            } else {
                                let _ = window.as_ref().window().move_window(Position::TrayCenter);
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                    }
                })
                .build(app)?;

            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

**Step 7: Create `src-tauri/tauri.conf.json`**

```json
{
  "$schema": "https://raw.githubusercontent.com/tauri-apps/tauri/dev/crates/tauri-config-schema/schema.json",
  "productName": "Observer Ward",
  "version": "0.1.0",
  "identifier": "com.observerward.app",
  "build": {
    "frontendDist": "../ui",
    "beforeDevCommand": "",
    "beforeBuildCommand": ""
  },
  "app": {
    "windows": [
      {
        "label": "main",
        "title": "Observer Ward",
        "width": 380,
        "height": 500,
        "resizable": false,
        "decorations": false,
        "visible": false,
        "skipTaskbar": true,
        "transparent": true
      }
    ],
    "trayIcon": {
      "iconPath": "icons/icon.png",
      "iconAsTemplate": true
    },
    "security": {
      "csp": "default-src 'self'; style-src 'self' 'unsafe-inline'; font-src 'self' data:"
    }
  },
  "bundle": {
    "active": true,
    "icon": [
      "icons/icon.png"
    ]
  }
}
```

**Step 8: Create `src-tauri/capabilities/default.json`**

```json
{
  "identifier": "default",
  "description": "Default capabilities for Observer Ward",
  "windows": ["main"],
  "permissions": [
    "core:default",
    "positioner:default"
  ]
}
```

**Step 9: Create placeholder tray icon**

Generate a simple 32x32 PNG icon. Use a solid white circle as a placeholder (we'll replace it later). The simplest approach: use `convert` from ImageMagick or create a tiny PNG programmatically.

```bash
# If ImageMagick is available:
convert -size 22x22 xc:transparent -fill white -draw "circle 11,11 11,2" src-tauri/icons/icon.png
# If not, we'll use a pre-made one or create via Python
```

Alternatively, download the Tauri default icon for now and replace later.

**Step 10: Create `ui/index.html`**

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>Observer Ward</title>
  <style>
    * { margin: 0; padding: 0; box-sizing: border-box; }
    body {
      font-family: "JetBrains Mono", "SF Mono", "Menlo", monospace;
      background: #0a0a0f;
      color: #b0fbff;
      overflow: hidden;
      user-select: none;
      -webkit-user-select: none;
    }
    .container {
      padding: 12px;
      height: 100vh;
      display: flex;
      flex-direction: column;
    }
    .header {
      display: flex;
      justify-content: space-between;
      align-items: center;
      padding-bottom: 10px;
      border-bottom: 1px solid rgba(0, 255, 240, 0.15);
    }
    .title {
      font-size: 13px;
      font-weight: 700;
      letter-spacing: 2px;
      color: #00fff0;
      text-shadow: 0 0 10px rgba(0, 255, 240, 0.5);
    }
    .server-list {
      flex: 1;
      overflow-y: auto;
      padding: 8px 0;
    }
    .empty-state {
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
      height: 200px;
      color: #4a5568;
      font-size: 12px;
    }
  </style>
</head>
<body>
  <div class="container">
    <div class="header">
      <span class="title">◈ OBSERVER WARD</span>
    </div>
    <div class="server-list">
      <div class="empty-state">
        <p>No servers configured</p>
      </div>
    </div>
  </div>
</body>
</html>
```

**Step 11: Update `.gitignore`**

```
/src-tauri/target
```

**Step 12: Build and verify**

```bash
cd src-tauri && cargo build 2>&1
```

Expected: builds successfully, no errors. Warnings from pedantic clippy are fine at this stage.

**Step 13: Run the app**

```bash
cd src-tauri && cargo tauri dev
```

Expected: tray icon appears in macOS menu bar. Click it to show/hide the dark popover window with "OBSERVER WARD" header.

**Step 14: Commit**

```bash
git add -A
git commit -m "feat: scaffold Tauri v2 menu bar app with tray icon and popover"
```

---

### Task 2: Config system

**Files:**
- Create: `src-tauri/src/config.rs`
- Modify: `src-tauri/src/lib.rs`

**Step 1: Create `src-tauri/src/config.rs`**

```rust
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
}

fn default_poll_interval() -> u64 {
    30
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_poll_interval(),
            servers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerConfig {
    K8s {
        name: String,
        kubeconfig: Option<String>,
        context: String,
    },
    Ssh {
        name: String,
        host: String,
        #[serde(default = "default_ssh_port")]
        port: u16,
        user: String,
        key_path: String,
    },
}

fn default_ssh_port() -> u16 {
    22
}

impl ServerConfig {
    pub fn name(&self) -> &str {
        match self {
            ServerConfig::K8s { name, .. }
            | ServerConfig::Ssh { name, .. } => name,
        }
    }

    pub fn server_type(&self) -> &str {
        match self {
            ServerConfig::K8s { .. } => "k8s",
            ServerConfig::Ssh { .. } => "ssh",
        }
    }
}

/// Returns the config file path: ~/.config/observer-ward/config.json
fn config_path() -> Result<PathBuf, String> {
    let config_dir = dirs::config_dir()
        .ok_or_else(|| "could not determine config directory".to_string())?;
    Ok(config_dir.join("observer-ward").join("config.json"))
}

pub fn load_config() -> Result<AppConfig, String> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(AppConfig::default());
    }
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read config: {e}"))?;
    serde_json::from_str(&contents)
        .map_err(|e| format!("failed to parse config: {e}"))
}

pub fn save_config(config: &AppConfig) -> Result<(), String> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create config dir: {e}"))?;
    }
    let json = serde_json::to_string_pretty(config)
        .map_err(|e| format!("failed to serialize config: {e}"))?;
    std::fs::write(&path, json)
        .map_err(|e| format!("failed to write config: {e}"))
}
```

**Step 2: Add `dirs` dependency to `src-tauri/Cargo.toml`**

Add to `[dependencies]`:

```toml
dirs = "6.0"
```

**Step 3: Add Tauri commands and wire up in `src-tauri/src/lib.rs`**

Add `mod config;` at the top, then add these Tauri command functions:

```rust
mod config;

use std::sync::Mutex;
use tauri::State;

struct ConfigState(Mutex<config::AppConfig>);

#[tauri::command]
fn get_config(state: State<'_, ConfigState>) -> Result<config::AppConfig, String> {
    let config = state.0.lock().map_err(|e| format!("lock error: {e}"))?;
    Ok(config.clone())
}

#[tauri::command]
fn save_config_cmd(
    state: State<'_, ConfigState>,
    new_config: config::AppConfig,
) -> Result<(), String> {
    config::save_config(&new_config)?;
    let mut config = state.0.lock().map_err(|e| format!("lock error: {e}"))?;
    *config = new_config;
    Ok(())
}

#[tauri::command]
fn add_server(
    state: State<'_, ConfigState>,
    server: config::ServerConfig,
) -> Result<config::AppConfig, String> {
    let mut config = state.0.lock().map_err(|e| format!("lock error: {e}"))?;
    config.servers.push(server);
    config::save_config(&config)?;
    Ok(config.clone())
}

#[tauri::command]
fn remove_server(
    state: State<'_, ConfigState>,
    name: String,
) -> Result<config::AppConfig, String> {
    let mut config = state.0.lock().map_err(|e| format!("lock error: {e}"))?;
    config.servers.retain(|s| s.name() != name);
    config::save_config(&config)?;
    Ok(config.clone())
}
```

Update the `Builder` chain in `run()` to register state and commands:

```rust
    let initial_config = config::load_config().unwrap_or_default();

    tauri::Builder::default()
        .plugin(tauri_plugin_positioner::init())
        .manage(ConfigState(Mutex::new(initial_config)))
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_config_cmd,
            add_server,
            remove_server,
        ])
        .setup(|app| {
            // ... existing setup code unchanged
        })
```

**Step 4: Build and verify**

```bash
cd src-tauri && cargo build 2>&1
```

Expected: compiles cleanly.

**Step 5: Commit**

```bash
git add -A
git commit -m "feat: add config system with load/save and Tauri commands"
```

---

### Task 3: Frontend — Tron neon theme and server list

**Files:**
- Rewrite: `ui/index.html`
- Create: `ui/styles.css`
- Create: `ui/app.js`

**Step 1: Create `ui/styles.css`**

Full Tron-inspired dark neon theme with metric bar styles, add-server form, scrollable server list, glow effects, color-coded bars, and animations.

Key CSS features:
- `--neon-cyan: #00fff0`, `--neon-pink: #ff2d6f`, `--neon-amber: #ffb800`
- `.metric-bar` with inner `.metric-fill` that has `box-shadow` glow
- `.metric-fill.level-ok` (cyan), `.level-warn` (amber), `.level-crit` (pink)
- `@keyframes pulse` for bar edge animation
- `.server-card` with dim cyan borders and hover glow
- `.add-form` with glowing input focus states
- `.status-dot` that glows green or pink
- Scrollbar styling for dark theme

**Step 2: Create `ui/app.js`**

Frontend logic:
- On load: call `window.__TAURI__.core.invoke("get_config")` to get server list
- Render server cards with metric bar placeholders
- Listen for `metrics-update` events via `window.__TAURI__.event.listen`
- Update bar widths and colors based on metric values
- Add server form: collect fields, call `invoke("add_server", { server })`
- Remove server: call `invoke("remove_server", { name })`
- Helper functions: `renderServerCard(server, metrics)`, `updateMetricBar(el, value)`, `formatBytes(bytes)`

**Step 3: Rewrite `ui/index.html`**

Link to `styles.css` and `app.js`. Structure:

```html
<div class="container">
  <header class="header">
    <span class="title">◈ OBSERVER WARD</span>
    <button class="btn-add" id="btn-add">⊕</button>
  </header>
  <div class="server-list" id="server-list"></div>
  <div class="add-form hidden" id="add-form">
    <!-- form fields -->
  </div>
  <footer class="footer">
    <button class="btn-settings" id="btn-settings">⚙ SETTINGS</button>
  </footer>
</div>
```

**Step 4: Run and verify visually**

```bash
cd src-tauri && cargo tauri dev
```

Expected: click tray icon → dark popover with neon styling. If no servers configured, shows "No servers configured" empty state. Click ⊕ to see the add form.

**Step 5: Commit**

```bash
git add -A
git commit -m "feat: add Tron neon frontend with server list and add form"
```

---

### Task 4: Metrics data model

**Files:**
- Create: `src-tauri/src/metrics.rs`
- Modify: `src-tauri/src/lib.rs`

**Step 1: Create `src-tauri/src/metrics.rs`**

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerMetrics {
    pub server_name: String,
    pub server_type: String,
    pub status: ServerStatus,
    pub cpu_percent: f64,
    pub memory_percent: f64,
    pub disk_percent: f64,
    pub net_rx_bytes_per_sec: u64,
    pub net_tx_bytes_per_sec: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ServerStatus {
    #[default]
    Pending,
    Online,
    Offline,
    Error,
}

/// Event payload sent to the frontend
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsUpdate {
    pub servers: Vec<ServerMetrics>,
}
```

**Step 2: Add `mod metrics;` to `src-tauri/src/lib.rs`**

```rust
mod metrics;
```

**Step 3: Build and verify**

```bash
cd src-tauri && cargo build 2>&1
```

**Step 4: Commit**

```bash
git add -A
git commit -m "feat: add metrics data model"
```

---

### Task 5: SSH metric backend

**Files:**
- Create: `src-tauri/src/ssh_backend.rs`
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/src/lib.rs`

**Step 1: Add `russh` and `russh-keys` to `src-tauri/Cargo.toml`**

```toml
russh = "0.54"
russh-keys = "0.54"
```

**Step 2: Create `src-tauri/src/ssh_backend.rs`**

Implement:

- `SshBackend` struct holding a `russh::client::Handle` and previous network bytes (for rate calculation)
- `connect(host, port, user, key_path)` — loads the private key via `russh_keys`, connects, authenticates
- `collect_metrics(server_name)` — executes the compound command, parses output into `ServerMetrics`
- Parsing functions:
  - `parse_cpu(top_output) -> f64` — extract `%Cpu(s)` line, compute usage
  - `parse_memory(free_output) -> f64` — extract used/total from `free -b`
  - `parse_disk(df_output) -> f64` — extract use% from `df`
  - `parse_network(proc_net_dev, prev_bytes, elapsed_secs) -> (u64, u64)` — compute rx/tx rates

- Reconnection: if command execution fails, drop handle, set status to `Offline`, reconnect on next poll

**Step 3: Build and verify**

```bash
cd src-tauri && cargo build 2>&1
```

**Step 4: Write unit tests for parsers**

Create test module in `ssh_backend.rs` with `#[cfg(test)]` tests for each parser function using sample output strings from `top`, `free`, `df`, and `/proc/net/dev`.

**Step 5: Run tests**

```bash
cd src-tauri && cargo test ssh_backend 2>&1
```

Expected: all parser tests pass.

**Step 6: Commit**

```bash
git add -A
git commit -m "feat: add SSH metric backend with output parsers"
```

---

### Task 6: Kubernetes metric backend

**Files:**
- Create: `src-tauri/src/k8s_backend.rs`
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/src/lib.rs`

**Step 1: Add `kube` and `k8s-openapi` to `src-tauri/Cargo.toml`**

```toml
kube = { version = "0.98", features = ["client", "config", "runtime"] }
k8s-openapi = { version = "0.23", features = ["latest"] }
```

Note: verify exact compatible versions at build time. `kube` 0.98+ pairs with `k8s-openapi` 0.23.

**Step 2: Create `src-tauri/src/k8s_backend.rs`**

Implement:

- `K8sBackend` struct holding a `kube::Client` and previous network bytes
- `connect(kubeconfig_path, context)` — build `kube::Config` from kubeconfig with specific context, create client
- `collect_metrics(server_name)` — aggregates across all nodes in the cluster:
  - CPU/Memory: GET `/apis/metrics.k8s.io/v1beta1/nodes` — sum usage across nodes, compare to allocatable from node status
  - Disk/Network: GET `/api/v1/nodes/{name}/proxy/stats/summary` — parse kubelet stats JSON for `fs` and `network` fields
- Returns a single `ServerMetrics` with cluster-wide averages

**Step 3: Build and verify**

```bash
cd src-tauri && cargo build 2>&1
```

Note: k8s tests require a live cluster. Unit tests can cover JSON parsing of sample kubelet stats responses.

**Step 4: Write unit tests for kubelet stats parsing**

Test with sample JSON payloads for the stats/summary endpoint.

**Step 5: Run tests**

```bash
cd src-tauri && cargo test k8s_backend 2>&1
```

**Step 6: Commit**

```bash
git add -A
git commit -m "feat: add Kubernetes metric backend with kube crate"
```

---

### Task 7: Poll loop and event emission

**Files:**
- Create: `src-tauri/src/poller.rs`
- Modify: `src-tauri/src/lib.rs`

**Step 1: Create `src-tauri/src/poller.rs`**

Implement:

- `Poller` struct holding:
  - `Arc<Mutex<AppConfig>>` (shared with Tauri state)
  - `HashMap<String, SshBackend>` for SSH connections
  - `HashMap<String, K8sBackend>` for k8s connections
  - `HashMap<String, u32>` for consecutive failure counts per server
  - `app_handle: tauri::AppHandle` for emitting events

- `start(app_handle, config_state)` — spawns a `tokio::spawn` loop:
  ```
  loop {
      let config = read current config from mutex
      for each server in config.servers:
          spawn a task to collect metrics (with timeout)
      collect all results
      emit "metrics-update" event to frontend
      sleep(poll_interval_secs)
  }
  ```

- Per-server error handling:
  - On success: reset failure count, set status `Online`
  - On failure: increment failure count, set status `Offline`
  - If failure_count >= 3: use 120s backoff instead of normal interval for that server
  - Always include the server in the emitted update (with `Offline` status if failed)

**Step 2: Wire poller into `lib.rs` setup**

In the `.setup()` closure, after building the tray icon, start the poller:

```rust
let config_state = app.state::<ConfigState>().inner().clone();
let app_handle = app.handle().clone();
tokio::spawn(async move {
    poller::start(app_handle, config_state).await;
});
```

Note: Tauri v2 uses Tokio under the hood, so `tokio::spawn` works in setup.

**Step 3: Build and verify**

```bash
cd src-tauri && cargo build 2>&1
```

**Step 4: Commit**

```bash
git add -A
git commit -m "feat: add poll loop with per-server error handling and event emission"
```

---

### Task 8: Wire frontend to live metrics

**Files:**
- Modify: `ui/app.js`

**Step 1: Add event listener for metrics updates**

In `app.js`, add:

```javascript
const { listen } = window.__TAURI__.event;

listen("metrics-update", (event) => {
    const { servers } = event.payload;
    for (const metrics of servers) {
        updateServerCard(metrics);
    }
});
```

**Step 2: Implement `updateServerCard(metrics)`**

- Find the server card DOM element by `data-server-name`
- Update each metric bar width and CSS class based on value
- Update network text (format bytes/sec)
- Toggle `.offline` class if status is offline
- Update status dot color

**Step 3: Implement bar color logic**

```javascript
function barLevel(percent) {
    if (percent >= 85) return "level-crit";
    if (percent >= 60) return "level-warn";
    return "level-ok";
}
```

**Step 4: Run with a test server and verify**

```bash
cd src-tauri && cargo tauri dev
```

Add an SSH server via the UI form, verify metrics appear and bars animate.

**Step 5: Commit**

```bash
git add -A
git commit -m "feat: wire frontend to live metric events with color-coded bars"
```

---

### Task 9: Settings panel and autostart

**Files:**
- Modify: `ui/app.js`
- Modify: `ui/styles.css`
- Modify: `ui/index.html`
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/capabilities/default.json`

**Step 1: Add `tauri-plugin-autostart` to `src-tauri/Cargo.toml`**

```toml
tauri-plugin-autostart = "2.5"
```

**Step 2: Register autostart plugin in `lib.rs`**

```rust
use tauri_plugin_autostart::MacosLauncher;

// In Builder chain:
.plugin(tauri_plugin_autostart::init(
    MacosLauncher::LaunchAgent,
    None,
))
```

**Step 3: Add autostart permissions to `capabilities/default.json`**

```json
"autostart:allow-enable",
"autostart:allow-disable",
"autostart:allow-is-enabled"
```

**Step 4: Add settings panel to frontend**

Settings panel (hidden by default, shown when ⚙ clicked):
- Poll interval input (number, seconds)
- Launch at login toggle
- Save button

Wire to Tauri commands and autostart JS API.

**Step 5: Build and verify**

```bash
cd src-tauri && cargo tauri dev
```

**Step 6: Commit**

```bash
git add -A
git commit -m "feat: add settings panel with poll interval and launch at login"
```

---

### Task 10: Polish — icon, warnings, and transitions

**Files:**
- Replace: `src-tauri/icons/icon.png` with proper icon
- Modify: `ui/styles.css`
- Modify: `ui/app.js`
- Modify: `src-tauri/src/lib.rs`

**Step 1: Create a proper tray icon**

Design a monochrome 22x22 PNG template icon — a stylized hexagon or eye shape in white on transparent background. Must be single-color for macOS template image compatibility.

**Step 2: Add CSS transitions for bar color changes**

```css
.metric-fill {
    transition: width 0.5s ease, background 0.3s ease, box-shadow 0.3s ease;
}
```

**Step 3: Add offline server styling**

- `.server-card.offline` — dim opacity, pulsing pink warning icon
- `@keyframes pulse-warning` for the offline indicator

**Step 4: Add tray icon warning state**

In the Rust backend, when emitting metrics, check if any server is offline. If so, swap the tray icon to a variant with a small warning dot (pre-generate two icon PNGs: normal and warning).

**Step 5: Close popover when clicking outside**

Add a `blur` event listener on the window to auto-hide:

```javascript
document.addEventListener("blur", () => {
    // Tauri window loses focus = clicked outside
});
```

Or handle via Tauri window events in Rust.

**Step 6: Build, test end-to-end, verify**

```bash
cd src-tauri && cargo tauri dev
```

Verify: add SSH server, see metrics flow, disconnect server, see offline state, reconnect, see recovery.

**Step 7: Commit**

```bash
git add -A
git commit -m "feat: polish UI with transitions, offline states, and tray icon warning"
```

---

## Dependency Summary

### `src-tauri/Cargo.toml` final dependencies

```toml
[dependencies]
tauri = { version = "2.10", features = ["tray-icon"] }
tauri-plugin-positioner = { version = "2.3", features = ["tray-icon"] }
tauri-plugin-autostart = "2.5"
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
tokio = { version = "1", features = ["full"] }
russh = "0.54"
russh-keys = "0.54"
kube = { version = "0.98", features = ["client", "config", "runtime"] }
k8s-openapi = { version = "0.23", features = ["latest"] }
tracing = "0.1"
tracing-subscriber = "0.3"
dirs = "6.0"
```

## Testing Strategy

- **SSH parsers:** Unit tests with sample `top`/`free`/`df`/`/proc/net/dev` output
- **K8s parsers:** Unit tests with sample kubelet stats JSON
- **Config:** Unit tests for load/save/default
- **Integration:** Manual testing with real SSH servers and k8s clusters
- **UI:** Visual verification via `cargo tauri dev`

"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const UNITS = ["B/s", "KB/s", "MB/s", "GB/s"];
const KILO = 1024;

// ── State ─────────────────────────────────────

let servers = [];
let metricsCache = {};
let contextMenuTarget = null;

// ── DOM refs ──────────────────────────────────

const serverListEl = document.getElementById("server-list");
const addFormPanel = document.getElementById("add-form-panel");
const addFormEl = document.getElementById("add-form");
const btnCancel = document.getElementById("btn-cancel");
const typeSelect = document.getElementById("field-type");
const formError = document.getElementById("form-error");
const contextMenu = document.getElementById("context-menu");

const k8sFields = document.querySelectorAll(".field-k8s");
const sshFields = document.querySelectorAll(".field-ssh");

// ── Helpers ───────────────────────────────────

function formatBytes(bytesPerSec) {
  if (bytesPerSec <= 0) {
    return "0 B/s";
  }
  let value = bytesPerSec;
  let unitIndex = 0;
  while (value >= KILO && unitIndex < UNITS.length - 1) {
    value /= KILO;
    unitIndex++;
  }
  if (unitIndex === 0) {
    return `${Math.round(value)} ${UNITS[unitIndex]}`;
  }
  return `${value.toFixed(1)} ${UNITS[unitIndex]}`;
}

function barLevel(percent) {
  if (percent >= 85) {
    return "level-crit";
  }
  if (percent >= 60) {
    return "level-warn";
  }
  return "level-ok";
}

function escapeHtml(str) {
  const el = document.createElement("span");
  el.textContent = str;
  return el.innerHTML;
}

function serverType(server) {
  return server.type;
}

function serverTypeBadge(server) {
  return escapeHtml(serverType(server));
}

// ── Rendering ─────────────────────────────────

function renderMetricBar(label, percent) {
  const level = barLevel(percent);
  const clamped = Math.max(0, Math.min(100, percent));
  return `
    <div class="metric-row">
      <span class="metric-label">${label}</span>
      <div class="metric-bar-track">
        <div class="metric-bar-fill ${level}"
             style="width: ${clamped}%"></div>
      </div>
      <span class="metric-value">${Math.round(clamped)}%</span>
    </div>`;
}

function renderNetRow(txBytes, rxBytes) {
  return `
    <div class="net-row">
      <span class="net-label">NET</span>
      <div class="net-values">
        <span class="net-up">
          <span class="net-arrow">&uarr;</span>
          ${formatBytes(txBytes)}
        </span>
        <span class="net-down">
          <span class="net-arrow">&darr;</span>
          ${formatBytes(rxBytes)}
        </span>
      </div>
    </div>`;
}

function renderServerCard(server) {
  const name = server.name;
  const metrics = metricsCache[name];
  const isOffline = metrics && metrics.error;
  const hasMetrics = metrics && !metrics.error;

  let statusClass = "pending";
  if (hasMetrics) {
    statusClass = "online";
  }
  if (isOffline) {
    statusClass = "offline";
  }

  let cardClass = "server-card";
  if (isOffline) {
    cardClass += " offline";
  }

  let metricsHtml = "";
  if (hasMetrics) {
    metricsHtml = `
      <div class="metric-rows">
        ${renderMetricBar("CPU", metrics.cpu)}
        ${renderMetricBar("MEM", metrics.mem)}
        ${renderMetricBar("DISK", metrics.disk)}
      </div>
      ${renderNetRow(metrics.net_tx, metrics.net_rx)}`;
  } else if (isOffline) {
    metricsHtml = "";
  } else {
    metricsHtml =
      '<div class="metrics-pending">awaiting metrics...</div>';
  }

  let offlineHtml = "";
  if (isOffline) {
    offlineHtml =
      '<span class="offline-label">offline</span>';
  }

  return `
    <div class="${cardClass}"
         data-server-name="${escapeHtml(name)}">
      <div class="server-card-header">
        <span class="status-dot ${statusClass}"></span>
        <span class="server-name">${escapeHtml(name)}</span>
        <span class="server-type-badge">${serverTypeBadge(server)}</span>
        ${offlineHtml}
      </div>
      ${metricsHtml}
    </div>`;
}

function renderServerList(serverArray) {
  if (serverArray.length === 0) {
    serverListEl.innerHTML = `
      <div class="empty-state">
        <div class="empty-state-icon">&#x25C8;</div>
        <div class="empty-state-text">
          No servers configured<br>
          Click + to add one
        </div>
      </div>`;
    return;
  }
  serverListEl.innerHTML = serverArray
    .map(renderServerCard)
    .join("");
}

// ── Form ──────────────────────────────────────

function toggleTypeFields() {
  const selected = typeSelect.value;
  for (const el of k8sFields) {
    el.classList.toggle("hidden", selected !== "k8s");
  }
  for (const el of sshFields) {
    el.classList.toggle("hidden", selected !== "ssh");
  }
}

function openAddForm() {
  addFormEl.reset();
  formError.textContent = "";
  toggleTypeFields();
  addFormPanel.classList.add("open");
}

function closeAddForm() {
  addFormPanel.classList.remove("open");
  formError.textContent = "";
}

function buildServerConfig() {
  const name = document.getElementById("field-name").value.trim();
  const type = typeSelect.value;

  if (!name) {
    return { error: "Name is required" };
  }

  if (type === "k8s") {
    const context = document
      .getElementById("field-context").value.trim();
    const kubeconfig = document
      .getElementById("field-kubeconfig").value.trim() || null;
    if (!context) {
      return { error: "Context is required for Kubernetes" };
    }
    return {
      config: { type: "k8s", name, context, kubeconfig },
    };
  }

  const host = document
    .getElementById("field-host").value.trim();
  const portStr = document
    .getElementById("field-port").value.trim();
  const user = document
    .getElementById("field-user").value.trim();
  const keyPath = document
    .getElementById("field-keypath").value.trim();

  if (!host) {
    return { error: "Host is required for SSH" };
  }
  if (!user) {
    return { error: "User is required for SSH" };
  }
  if (!keyPath) {
    return { error: "Key path is required for SSH" };
  }

  const port = parseInt(portStr, 10);
  if (isNaN(port) || port < 1 || port > 65535) {
    return { error: "Port must be between 1 and 65535" };
  }

  return {
    config: {
      type: "ssh",
      name,
      host,
      port,
      user,
      key_path: keyPath,
    },
  };
}

async function handleAddServer(e) {
  e.preventDefault();
  formError.textContent = "";

  const result = buildServerConfig();
  if (result.error) {
    formError.textContent = result.error;
    return;
  }

  try {
    const config = await invoke("add_server", {
      server: result.config,
    });
    servers = config.servers;
    renderServerList(servers);
    closeAddForm();
  } catch (err) {
    formError.textContent = String(err);
  }
}

// ── Context Menu ──────────────────────────────

function showContextMenu(e, serverName) {
  e.preventDefault();
  contextMenuTarget = serverName;
  contextMenu.style.left = `${e.clientX}px`;
  contextMenu.style.top = `${e.clientY}px`;
  contextMenu.classList.add("visible");
}

function hideContextMenu() {
  contextMenu.classList.remove("visible");
  contextMenuTarget = null;
}

async function handleRemoveServer() {
  if (!contextMenuTarget) {
    return;
  }
  const name = contextMenuTarget;
  hideContextMenu();

  try {
    const config = await invoke("remove_server", { name });
    servers = config.servers;
    delete metricsCache[name];
    renderServerList(servers);
  } catch (err) {
    console.error("Failed to remove server:", err);
  }
}

// ── Metrics Event Listener ────────────────────

function handleMetricsUpdate(event) {
  const data = event.payload;
  if (!data || !data.server_name) {
    return;
  }

  if (data.error) {
    metricsCache[data.server_name] = { error: data.error };
  } else {
    metricsCache[data.server_name] = {
      cpu: data.cpu ?? 0,
      mem: data.mem ?? 0,
      disk: data.disk ?? 0,
      net_tx: data.net_tx ?? 0,
      net_rx: data.net_rx ?? 0,
    };
  }

  renderServerList(servers);
}

// ── Init ──────────────────────────────────────

async function init() {
  try {
    const config = await invoke("get_config");
    servers = config.servers;
    renderServerList(servers);
  } catch (err) {
    console.error("Failed to load config:", err);
    renderServerList([]);
  }

  await listen("metrics-update", handleMetricsUpdate);
}

// ── Event Bindings ────────────────────────────

document.getElementById("btn-open-add")
  .addEventListener("click", openAddForm);

addFormEl.addEventListener("submit", handleAddServer);

btnCancel.addEventListener("click", closeAddForm);

typeSelect.addEventListener("change", toggleTypeFields);

document.getElementById("ctx-remove")
  .addEventListener("click", handleRemoveServer);

serverListEl.addEventListener("contextmenu", (e) => {
  const card = e.target.closest(".server-card");
  if (!card) {
    return;
  }
  const name = card.dataset.serverName;
  if (name) {
    showContextMenu(e, name);
  }
});

document.addEventListener("click", (e) => {
  if (!contextMenu.contains(e.target)) {
    hideContextMenu();
  }
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    hideContextMenu();
    closeAddForm();
  }
});

init();

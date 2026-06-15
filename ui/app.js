"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const WIN_WIDTH = 380;
const WIN_MIN_HEIGHT = 100;
const WIN_PADDING = 20;

const UNITS = ["B/s", "KB/s", "MB/s", "GB/s"];
const KILO = 1024;

// ── State ─────────────────────────────────────

let servers = [];
let pods = [];
let metricsCache = {};
let contextMenuTarget = null;
const collapsedClusters = new Set();
const expandedOnce = new Set();

const metricsHistory = {};
const HISTORY_MAX = 20;

const anomalyState = {};
const ANOMALY_MIN_SAMPLES = 10;
const ANOMALY_SIGMA = 3;

const ALIAS_KEY = "ow-aliases";

function loadAliases() {
  try {
    return JSON.parse(localStorage.getItem(ALIAS_KEY) || "{}");
  } catch {
    return {};
  }
}

function saveAlias(fullName, alias) {
  const aliases = loadAliases();
  if (alias && alias.trim()) {
    aliases[fullName] = alias.trim();
  } else {
    delete aliases[fullName];
  }
  localStorage.setItem(ALIAS_KEY, JSON.stringify(aliases));
}

function getDisplayName(fullName, fallback) {
  return loadAliases()[fullName] || fallback;
}

// ── DOM refs ──────────────────────────────────

const wardEye = document.querySelector(".ward-eye");
const syncLabel = document.getElementById("sync-label");
const serverListEl = document.getElementById("server-list");
const addFormPanel = document.getElementById("add-form-panel");
const addFormEl = document.getElementById("add-form");
const btnCancel = document.getElementById("btn-cancel");
const typeSelect = document.getElementById("field-type");
const formError = document.getElementById("form-error");
const contextMenu = document.getElementById("context-menu");

const k8sFields = document.querySelectorAll(".field-k8s");
const sshFields = document.querySelectorAll(".field-ssh");

// ── Window Sizing ────────────────────────────

function resizeToContent() {
  const header = document.querySelector(".header");
  const footer = document.querySelector(".footer");

  const contentHeight =
    header.offsetHeight +
    addFormPanel.offsetHeight +
    serverListEl.scrollHeight +
    settingsPanel.offsetHeight +
    footer.offsetHeight +
    WIN_PADDING;

  const height = Math.max(WIN_MIN_HEIGHT, contentHeight);
  invoke("resize_window", { width: WIN_WIDTH, height }).catch(() => {});
}

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
  return el.innerHTML.replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}

function formatMillicores(mc) {
  if (mc >= 1000) {
    return `${(mc / 1000).toFixed(1)}c`;
  }
  return `${Math.round(mc)}m`;
}

function formatMemory(bytes) {
  if (bytes <= 0) {
    return "0";
  }
  const gi = bytes / (1024 * 1024 * 1024);
  if (gi >= 1) {
    return `${gi.toFixed(1)}Gi`;
  }
  const mi = bytes / (1024 * 1024);
  return `${Math.round(mi)}Mi`;
}

function formatAge(isoTimestamp) {
  if (!isoTimestamp) {
    return "";
  }
  const start = new Date(isoTimestamp).getTime();
  if (isNaN(start)) {
    return "";
  }
  const diffSec = Math.floor((Date.now() - start) / 1000);
  if (diffSec < 0) {
    return "0s";
  }
  if (diffSec < 60) {
    return `${diffSec}s`;
  }
  if (diffSec < 3600) {
    return `${Math.floor(diffSec / 60)}m`;
  }
  if (diffSec < 86400) {
    return `${Math.floor(diffSec / 3600)}h`;
  }
  return `${Math.floor(diffSec / 86400)}d`;
}

function podStatusClass(podStatus) {
  if (!podStatus) {
    return "pending";
  }
  const s = podStatus.toLowerCase();
  if (s === "running") {
    return "online";
  }
  if (
    s === "crashloopbackoff" ||
    s === "imagepullbackoff" ||
    s === "errimagepull" ||
    s === "oomkilled" ||
    s === "failed"
  ) {
    return "error";
  }
  return "pending";
}

function formatPvcBytes(bytes) {
  if (bytes <= 0) {
    return "0";
  }
  const gi = bytes / (1024 * 1024 * 1024);
  if (gi >= 1) {
    return `${gi.toFixed(1)}Gi`;
  }
  const mi = bytes / (1024 * 1024);
  if (mi >= 1) {
    return `${Math.round(mi)}Mi`;
  }
  const ki = bytes / 1024;
  return `${Math.round(ki)}Ki`;
}

function serverType(server) {
  return server.type;
}

function serverTypeBadge(server) {
  return escapeHtml(serverType(server));
}

// ── Anomaly Detection ─────────────────────────

function computeAnomaly(values) {
  if (!values || values.length < ANOMALY_MIN_SAMPLES) {
    return { isAnomaly: false };
  }
  const n = values.length;
  const sum = values.reduce((a, b) => a + b, 0);
  const mean = sum / n;
  const variance =
    values.reduce((a, v) => a + (v - mean) ** 2, 0) / (n - 1);
  const stddev = Math.sqrt(variance);
  const latest = values[n - 1];
  const deviation = Math.abs(latest - mean);
  return { isAnomaly: deviation > ANOMALY_SIGMA * stddev };
}

// ── Rendering ─────────────────────────────────

function sparklineColor(levelClass) {
  if (levelClass === "level-crit") {
    return "rgba(255,45,111,0.4)";
  }
  if (levelClass === "level-warn") {
    return "rgba(255,184,0,0.4)";
  }
  return "rgba(0,255,240,0.4)";
}

function renderSparkline(values, levelClass) {
  if (!values || values.length < 2) {
    return "";
  }
  const w = 60;
  const h = 12;
  const step = w / (values.length - 1);
  const points = values
    .map((v, i) => {
      const x = (i * step).toFixed(1);
      const clamped = Number.isFinite(v)
        ? Math.min(100, Math.max(0, v))
        : 0;
      const y = (h - (clamped / 100) * h).toFixed(1);
      return `${x},${y}`;
    })
    .join(" ");
  const color = sparklineColor(levelClass);
  return `<svg class="sparkline-svg" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none">
    <polyline points="${points}" fill="none" stroke="${color}" stroke-width="1"/>
  </svg>`;
}

function renderMetricBar(label, percent, history, isAnomaly) {
  const level = barLevel(percent);
  const mClass = `m-${label.toLowerCase()}`;
  const clamped = Math.max(0, Math.min(100, percent));
  const spark = renderSparkline(history, level);
  const anomalyClass = isAnomaly ? " anomaly" : "";
  return `
    <div class="metric-row ${mClass}${anomalyClass}">
      <span class="metric-label">${label}</span>
      <div class="metric-bar-track">
        ${spark}
        <div class="metric-bar-fill ${level}"
             style="width: ${clamped}%"></div>
      </div>
      <span class="metric-value">${Math.round(clamped)}%</span>
    </div>`;
}

function renderPodMetricBar(label, percent, rawValue, history, isAnomaly) {
  const level = barLevel(percent);
  const mClass = `m-${label.toLowerCase()}`;
  const clamped = Math.max(0, Math.min(100, percent));
  const pctText = clamped < 1 && clamped > 0
    ? "<1%"
    : `${Math.round(clamped)}%`;
  const spark = renderSparkline(history, level);
  const anomalyClass = isAnomaly ? " anomaly" : "";
  return `
    <div class="metric-row ${mClass}${anomalyClass}">
      <span class="metric-label">${label}</span>
      <div class="metric-bar-track">
        ${spark}
        <div class="metric-bar-fill ${level}"
             style="width: ${clamped}%"></div>
      </div>
      <span class="metric-value pod-value">${rawValue}</span>
      <span class="metric-pct">${pctText}</span>
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

  let nodeCountBadge = "";
  if (hasMetrics && metrics.node_count > 0 && serverType(server) === "k8s") {
    nodeCountBadge = `<span class="node-count-badge">${metrics.node_count}n</span>`;
  }

  const hist = metricsHistory[name] || { cpu: [], mem: [], disk: [] };
  const anom = anomalyState[name] || {};

  let metricsHtml = "";
  if (hasMetrics) {
    if (serverType(server) === "k8s") {
      const cpuRaw = formatMillicores(metrics.cpu_millicores);
      const memRaw = formatMemory(metrics.memory_bytes);
      let diskRaw = "";
      if (metrics.disk_capacity_bytes > 0) {
        diskRaw = `${formatPvcBytes(metrics.disk_used_bytes)}/${formatPvcBytes(metrics.disk_capacity_bytes)}`;
      }
      metricsHtml = `
        <div class="metric-rows">
          ${renderPodMetricBar("CPU", metrics.cpu, cpuRaw, hist.cpu, anom.cpu)}
          ${renderPodMetricBar("MEM", metrics.mem, memRaw, hist.mem, anom.mem)}
          ${diskRaw ? renderPodMetricBar("DISK", metrics.disk, diskRaw, hist.disk, anom.disk) : renderMetricBar("DISK", metrics.disk, hist.disk, anom.disk)}
        </div>
        ${renderNetRow(metrics.net_tx, metrics.net_rx)}`;
    } else {
      metricsHtml = `
        <div class="metric-rows">
          ${renderMetricBar("CPU", metrics.cpu, hist.cpu, anom.cpu)}
          ${renderMetricBar("MEM", metrics.mem, hist.mem, anom.mem)}
          ${renderMetricBar("DISK", metrics.disk, hist.disk, anom.disk)}
        </div>
        ${renderNetRow(metrics.net_tx, metrics.net_rx)}`;
    }
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
         data-server-name="${escapeHtml(name)}"
         data-server-type="${escapeHtml(serverType(server))}">
      <div class="server-card-header">
        <span class="status-dot ${statusClass}"></span>
        <span class="server-name">${escapeHtml(getDisplayName(name, name))}</span>
        <span class="server-type-badge">${serverTypeBadge(server)}</span>
        ${nodeCountBadge}
        ${offlineHtml}
      </div>
      ${metricsHtml}
    </div>`;
}

function renderPodCard(pod) {
  const name = pod.name;
  const metrics = metricsCache[name];
  const isOffline = metrics && metrics.error;
  const hasMetrics = metrics && !metrics.error;

  let statusClass = "pending";
  if (hasMetrics && metrics.pod_status) {
    statusClass = podStatusClass(metrics.pod_status);
  } else if (hasMetrics) {
    statusClass = "online";
  }
  if (isOffline) {
    statusClass = "offline";
  }

  let cardClass = "server-card";
  if (isOffline) {
    cardClass += " offline";
  }

  let badgesHtml = "";
  if (hasMetrics) {
    const ageBadge = metrics.start_time
      ? `<span class="pod-age">${escapeHtml(formatAge(metrics.start_time))}</span>`
      : "";
    let restartBadge = "";
    if (metrics.restart_count > 0) {
      const restartLevel = metrics.restart_count >= 5
        ? "crit"
        : "warn";
      restartBadge = `<span class="restart-badge ${restartLevel}">${metrics.restart_count}x</span>`;
    }
    badgesHtml = `${ageBadge}${restartBadge}`;
  }

  const hist = metricsHistory[name] || { cpu: [], mem: [], disk: [] };
  const anom = anomalyState[name] || {};

  let metricsHtml = "";
  if (hasMetrics) {
    let pvcBar = "";
    if (metrics.pvc_capacity_bytes > 0) {
      const pvcPct =
        (metrics.pvc_used_bytes / metrics.pvc_capacity_bytes) *
        100;
      const pvcRaw = `${formatPvcBytes(metrics.pvc_used_bytes)}/${formatPvcBytes(metrics.pvc_capacity_bytes)}`;
      pvcBar = renderPodMetricBar("PVC", pvcPct, pvcRaw, hist.disk, anom.disk);
    }
    let podNetHtml = "";
    if (metrics.net_tx > 0 || metrics.net_rx > 0) {
      podNetHtml = renderNetRow(metrics.net_tx, metrics.net_rx);
    }
    metricsHtml = `
      <div class="metric-rows">
        ${renderPodMetricBar("CPU", metrics.cpu, formatMillicores(metrics.cpu_millicores), hist.cpu, anom.cpu)}
        ${renderPodMetricBar("MEM", metrics.mem, formatMemory(metrics.memory_bytes), hist.mem, anom.mem)}
        ${pvcBar}
      </div>
      ${podNetHtml}`;
    if (metrics.last_event) {
      metricsHtml += `<div class="pod-event">${escapeHtml(metrics.last_event)}</div>`;
    }
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
         data-server-name="${escapeHtml(name)}"
         data-card-type="pod">
      <div class="server-card-header">
        <span class="status-dot ${statusClass}"></span>
        <span class="server-name">${escapeHtml(getDisplayName(name, pod.displayName))}</span>
        ${badgesHtml}
        <button class="btn-logs" data-pod-full="${escapeHtml(name)}">logs</button>
        ${offlineHtml}
      </div>
      ${metricsHtml}
    </div>`;
}

function renderClusterSummary(clusterName, clusterPods) {
  let totalCpu = 0;
  let totalMem = 0;

  for (const pod of clusterPods) {
    const m = metricsCache[pod.name];
    if (m && !m.error) {
      totalCpu += m.cpu_millicores || 0;
      totalMem += m.memory_bytes || 0;
    }
  }

  const clusterMetrics = metricsCache[clusterName];
  const nodeCount = clusterMetrics && !clusterMetrics.error
    ? clusterMetrics.node_count || 0
    : 0;
  const nodesStat = nodeCount > 0
    ? `<span class="cluster-stat">${nodeCount}n</span>`
    : "";

  const collapsed = collapsedClusters.has(clusterName);
  const chevronClass = collapsed ? "chevron collapsed" : "chevron";

  return `
    <div class="cluster-summary" data-cluster="${escapeHtml(clusterName)}">
      <span class="${chevronClass}">&#x25B8;</span>
      <span class="cluster-name">${escapeHtml(getDisplayName(clusterName, clusterName))}</span>
      ${nodesStat}
      <span class="cluster-stat">${clusterPods.length} pods</span>
      <span class="cluster-stat">${formatMillicores(totalCpu)} CPU</span>
      <span class="cluster-stat">${formatMemory(totalMem)} MEM</span>
    </div>`;
}

function renderAll() {
  if (servers.length === 0 && pods.length === 0) {
    serverListEl.innerHTML = `
      <div class="empty-state">
        <div class="empty-state-icon">&#x25C8;</div>
        <div class="empty-state-text">
          No servers configured<br>
          Click + to add one
        </div>
      </div>`;
    resizeToContent();
    return;
  }
  const serverHtml = servers
    .map(renderServerCard)
    .join("");

  const clusterGroups = {};
  for (const pod of pods) {
    const cluster = pod.cluster || "default";
    if (!clusterGroups[cluster]) {
      clusterGroups[cluster] = [];
    }
    clusterGroups[cluster].push(pod);
  }

  let podHtml = "";
  for (const [cluster, clusterPods] of Object.entries(clusterGroups)) {
    if (!collapsedClusters.has(cluster) && !expandedOnce.has(cluster)) {
      collapsedClusters.add(cluster);
    }
    clusterPods.sort((a, b) => {
      const ma = metricsCache[a.name];
      const mb = metricsCache[b.name];
      const cpuA = ma && !ma.error ? ma.cpu : 0;
      const cpuB = mb && !mb.error ? mb.cpu : 0;
      return cpuB - cpuA;
    });
    const collapsed = collapsedClusters.has(cluster);
    const displayStyle = collapsed ? "display: none;" : "";
    podHtml += renderClusterSummary(cluster, clusterPods);
    podHtml += `<div class="cluster-pods" data-cluster-pods="${escapeHtml(cluster)}" style="${displayStyle}">`;
    podHtml += clusterPods.map(renderPodCard).join("");
    podHtml += "</div>";
  }

  serverListEl.innerHTML = serverHtml + podHtml;
  resizeToContent();
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
  if (addFormPanel.classList.contains("open")) {
    resizeToContent();
  }
}

function openAddForm() {
  addFormEl.reset();
  formError.textContent = "";
  toggleTypeFields();
  addFormPanel.classList.add("open");
  resizeToContent();
}

function closeAddForm() {
  addFormPanel.classList.remove("open");
  formError.textContent = "";
  resizeToContent();
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
    const namespace = document
      .getElementById("field-namespace").value.trim();
    const kubeconfig = document
      .getElementById("field-kubeconfig").value.trim() || null;
    if (!context) {
      return { error: "Context is required for Kubernetes" };
    }
    if (!namespace) {
      return { error: "Namespace is required for Kubernetes" };
    }
    return {
      config: { type: "k8s", name, context, namespace, kubeconfig },
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
    renderAll();
    closeAddForm();
  } catch (err) {
    formError.textContent = String(err);
  }
}

// ── Context Menu ──────────────────────────────

const ctxOpenTerminal = document.getElementById("ctx-open-terminal");
const ctxCopyKubectl = document.getElementById("ctx-copy-kubectl");
const ctxViewLogs = document.getElementById("ctx-view-logs");
const ctxCopyMetrics = document.getElementById("ctx-copy-metrics");
const ctxRemove = document.getElementById("ctx-remove");
const ctxSeparator = contextMenu.querySelector(".context-menu-separator");

function showContextMenu(e, target) {
  e.preventDefault();
  contextMenuTarget = target;

  // Position then clamp to viewport
  contextMenu.style.left = "0px";
  contextMenu.style.top = "0px";
  contextMenu.classList.add("visible");
  const menuW = contextMenu.offsetWidth;
  const menuH = contextMenu.offsetHeight;
  const vw = document.documentElement.clientWidth;
  const vh = document.documentElement.clientHeight;
  let x = e.clientX;
  let y = e.clientY;
  if (x + menuW > vw) { x = vw - menuW; }
  if (y + menuH > vh) { y = vh - menuH; }
  if (x < 0) { x = 0; }
  if (y < 0) { y = 0; }
  contextMenu.style.left = `${x}px`;
  contextMenu.style.top = `${y}px`;

  const isPod = target.cardType === "pod";
  const isSSH = target.serverType === "ssh";
  const isK8s = target.serverType === "k8s";

  ctxOpenTerminal.style.display = isSSH ? "" : "none";
  ctxCopyKubectl.style.display = isK8s && !isPod ? "" : "none";
  ctxViewLogs.style.display = isPod ? "" : "none";
  ctxCopyMetrics.style.display = "";
  ctxSeparator.style.display = isPod ? "none" : "";
  ctxRemove.style.display = isPod ? "none" : "";
}

function hideContextMenu() {
  contextMenu.classList.remove("visible");
  contextMenuTarget = null;
}

function findServerConfig(name) {
  return servers.find((s) => s.name === name) || null;
}

async function handleRemoveServer() {
  if (!contextMenuTarget) {
    return;
  }
  const name = contextMenuTarget.name;
  hideContextMenu();

  try {
    const config = await invoke("remove_server", { name });
    servers = config.servers;
    delete metricsCache[name];
    delete metricsHistory[name];
    delete anomalyState[name];
    const podPrefix = name + "/";
    for (const key of Object.keys(metricsCache)) {
      if (key.startsWith(podPrefix)) {
        delete metricsCache[key];
        delete metricsHistory[key];
        delete anomalyState[key];
      }
    }
    pods = pods.filter((p) => p.cluster !== name);
    renderAll();
  } catch (err) {
    console.error("Failed to remove server:", err);
  }
}

async function handleOpenTerminal() {
  if (!contextMenuTarget) {
    return;
  }
  const cfg = findServerConfig(contextMenuTarget.name);
  hideContextMenu();
  if (!cfg || cfg.type !== "ssh") {
    return;
  }
  try {
    await invoke("open_ssh_terminal", {
      host: cfg.host,
      port: cfg.port,
      user: cfg.user,
      keyPath: cfg.key_path,
    });
  } catch (err) {
    console.error("Failed to open SSH terminal:", err);
  }
}

async function handleCopyKubectl() {
  if (!contextMenuTarget) {
    return;
  }
  const cfg = findServerConfig(contextMenuTarget.name);
  hideContextMenu();
  if (!cfg || cfg.type !== "k8s") {
    return;
  }
  const q = (s) => `'${s.replace(/'/g, "'\\''")}'`;
  let cmd = `kubectl --context ${q(cfg.context)} -n ${q(cfg.namespace)}`;
  if (cfg.kubeconfig) {
    cmd += ` --kubeconfig ${q(cfg.kubeconfig)}`;
  }
  try {
    await invoke("copy_to_clipboard", { text: cmd });
  } catch (err) {
    console.error("Failed to copy kubectl context:", err);
  }
}

async function handleViewLogs() {
  if (!contextMenuTarget || contextMenuTarget.cardType !== "pod") {
    return;
  }
  const fullName = contextMenuTarget.name;
  hideContextMenu();
  const slashIdx = fullName.indexOf("/");
  if (slashIdx < 0) {
    return;
  }
  const clusterName = fullName.substring(0, slashIdx);
  const podName = fullName.substring(slashIdx + 1);
  const cfg = findServerConfig(clusterName);
  if (!cfg || cfg.type !== "k8s") {
    return;
  }
  try {
    await invoke("open_pod_logs", {
      podName,
      namespace: cfg.namespace,
      context: cfg.context,
      kubeconfig: cfg.kubeconfig || null,
    });
  } catch (err) {
    console.error("Failed to open pod logs:", err);
  }
}

async function handleCopyMetrics() {
  if (!contextMenuTarget) {
    return;
  }
  const name = contextMenuTarget.name;
  hideContextMenu();
  const m = metricsCache[name];
  if (!m || m.error) {
    return;
  }
  const lines = [
    `${name}`,
    `  CPU: ${Math.round(m.cpu)}%`,
    `  MEM: ${Math.round(m.mem)}%`,
    `  DISK: ${Math.round(m.disk)}%`,
  ];
  if (m.net_tx > 0 || m.net_rx > 0) {
    lines.push(`  NET TX: ${formatBytes(m.net_tx)}`);
    lines.push(`  NET RX: ${formatBytes(m.net_rx)}`);
  }
  try {
    await invoke("copy_to_clipboard", { text: lines.join("\n") });
  } catch (err) {
    console.error("Failed to copy metrics:", err);
  }
}

// ── Pod Logs ──────────────────────────────────

async function openPodLogs(fullName) {
  const slashIdx = fullName.indexOf("/");
  if (slashIdx < 0) {
    return;
  }
  const clusterName = fullName.substring(0, slashIdx);
  const podName = fullName.substring(slashIdx + 1);
  const cfg = findServerConfig(clusterName);
  if (!cfg || cfg.type !== "k8s") {
    return;
  }
  try {
    await invoke("open_pod_logs", {
      podName,
      namespace: cfg.namespace,
      context: cfg.context,
      kubeconfig: cfg.kubeconfig || null,
    });
  } catch (err) {
    console.error("Failed to open pod logs:", err);
  }
}

// ── Settings Panel ────────────────────────────

const settingsPanel = document.getElementById("settings-panel");
const fgIntervalInput = document.getElementById("fg-interval");
const bgIntervalInput = document.getElementById("bg-interval");
const autostartToggle = document.getElementById("autostart-toggle");
const notificationsToggle = document.getElementById("notifications-toggle");

const alertsSection = document.getElementById("alerts-section");
const alertsList = document.getElementById("alerts-list");
const alertsStatus = document.getElementById("alerts-status");
const grafanaEnabledToggle = document.getElementById("grafana-enabled");
const grafanaUrlInput = document.getElementById("grafana-url");
const grafanaVerifyTlsToggle = document.getElementById("grafana-verify-tls");
const grafanaTokenInput = document.getElementById("grafana-token");

const GRAFANA_CONN_NAME = "default";

function alertSeverityClass(severity) {
  if (severity === "critical") {
    return "sev-crit";
  }
  if (severity === "warning") {
    return "sev-warn";
  }
  return "sev-info";
}

async function openSettings() {
  fgIntervalInput.classList.remove("input-error");
  bgIntervalInput.classList.remove("input-error");
  try {
    const config = await invoke("get_config");
    fgIntervalInput.value = config.foreground_poll_secs ?? 10;
    bgIntervalInput.value = config.background_poll_secs ?? 300;
    notificationsToggle.checked = config.notifications_enabled ?? false;
    const grafana = config.grafana || null;
    grafanaEnabledToggle.checked = !!(grafana && grafana.enabled);
    grafanaUrlInput.value = grafana ? grafana.url : "";
    grafanaVerifyTlsToggle.checked = grafana ? grafana.verify_tls !== false : true;
    grafanaTokenInput.value = "";
    try {
      const hasToken = await invoke("has_grafana_token", { name: GRAFANA_CONN_NAME });
      grafanaTokenInput.placeholder = hasToken
        ? "(stored; leave blank to keep)"
        : "paste service-account token";
    } catch (err) {
      console.error("Failed to check Grafana token:", err);
    }
  } catch (err) {
    console.error("Failed to load config for settings:", err);
  }

  try {
    const enabled = await invoke("plugin:autostart|is_enabled");
    autostartToggle.checked = enabled;
  } catch (err) {
    console.error("Failed to check autostart status:", err);
    autostartToggle.checked = false;
  }

  settingsPanel.classList.add("open");
  resizeToContent();
}

function closeSettings() {
  settingsPanel.classList.remove("open");
  resizeToContent();
}

async function saveSettings() {
  let hasError = false;

  const fgInterval = parseInt(fgIntervalInput.value, 10);
  if (isNaN(fgInterval) || fgInterval < 5 || fgInterval > 120) {
    fgIntervalInput.classList.add("input-error");
    hasError = true;
  } else {
    fgIntervalInput.classList.remove("input-error");
  }

  const bgInterval = parseInt(bgIntervalInput.value, 10);
  if (isNaN(bgInterval) || bgInterval < 30 || bgInterval > 600) {
    bgIntervalInput.classList.add("input-error");
    hasError = true;
  } else {
    bgIntervalInput.classList.remove("input-error");
  }

  if (hasError) {
    return;
  }

  try {
    const config = await invoke("get_config");
    config.foreground_poll_secs = fgInterval;
    config.background_poll_secs = bgInterval;
    config.notifications_enabled = notificationsToggle.checked;
    const grafanaEnabled = grafanaEnabledToggle.checked;
    const grafanaUrl = grafanaUrlInput.value.trim();
    if (grafanaUrl) {
      config.grafana = {
        name: GRAFANA_CONN_NAME,
        url: grafanaUrl,
        verify_tls: grafanaVerifyTlsToggle.checked,
        enabled: grafanaEnabled,
      };
    } else {
      config.grafana = null;
    }
    grafanaConfigured = grafanaEnabled && !!grafanaUrl;
    await invoke("save_config_cmd", { newConfig: config });
    const tokenValue = grafanaTokenInput.value.trim();
    if (tokenValue) {
      await invoke("set_grafana_token", {
        name: GRAFANA_CONN_NAME,
        token: tokenValue,
      });
    }
    if (!grafanaConfigured) {
      alertsSection.classList.add("hidden");
    }
  } catch (err) {
    console.error("Failed to save settings:", err);
  }

  try {
    if (autostartToggle.checked) {
      await invoke("plugin:autostart|enable");
    } else {
      await invoke("plugin:autostart|disable");
    }
  } catch (err) {
    console.error("Failed to update autostart:", err);
  }

  closeSettings();
}

// ── Metrics Event Listener ────────────────────

function handleMetricsUpdate(event) {
  const payload = event.payload;
  if (!payload || !Array.isArray(payload.servers)) {
    return;
  }

  const currentPods = [];
  const receivedPodNames = new Set();
  const polledClusters = new Set();

  for (const entry of payload.servers) {
    const name = entry.server_name;
    if (!name) {
      continue;
    }

    if (entry.status === "offline" || entry.status === "error") {
      metricsCache[name] = { error: entry.status };
    } else if (entry.status === "pending") {
      // Leave metricsCache[name] untouched so UI shows
      // "awaiting metrics..." until the first real poll
      continue;
    } else {
      metricsCache[name] = {
        cpu: entry.cpu_percent ?? 0,
        mem: entry.memory_percent ?? 0,
        disk: entry.disk_percent ?? 0,
        net_tx: entry.net_tx_bytes_per_sec ?? 0,
        net_rx: entry.net_rx_bytes_per_sec ?? 0,
        cpu_millicores: entry.cpu_millicores ?? 0,
        memory_bytes: entry.memory_bytes ?? 0,
        restart_count: entry.restart_count ?? 0,
        start_time: entry.start_time ?? "",
        pod_status: entry.pod_status ?? "",
        pvc_used_bytes: entry.pvc_used_bytes ?? 0,
        pvc_capacity_bytes: entry.pvc_capacity_bytes ?? 0,
        last_event: entry.last_event ?? "",
        disk_used_bytes: entry.disk_used_bytes ?? 0,
        disk_capacity_bytes: entry.disk_capacity_bytes ?? 0,
        node_count: entry.node_count ?? 0,
      };
    }

    // Track history for sparklines
    if (entry.status === "online") {
      if (!metricsHistory[name]) {
        metricsHistory[name] = { cpu: [], mem: [], disk: [] };
      }
      const h = metricsHistory[name];
      h.cpu.push(entry.cpu_percent ?? 0);
      h.mem.push(entry.memory_percent ?? 0);
      h.disk.push(entry.disk_percent ?? 0);
      if (h.cpu.length > HISTORY_MAX) { h.cpu.shift(); }
      if (h.mem.length > HISTORY_MAX) { h.mem.shift(); }
      if (h.disk.length > HISTORY_MAX) { h.disk.shift(); }

      anomalyState[name] = {
        cpu: computeAnomaly(h.cpu).isAnomaly,
        mem: computeAnomaly(h.mem).isAnomaly,
        disk: computeAnomaly(h.disk).isAnomaly,
      };
    }

    // Track which clusters were included in this poll
    if (entry.server_type !== "pod") {
      polledClusters.add(name);
    }

    if (entry.server_type === "pod") {
      receivedPodNames.add(name);
      const slashIdx = name.indexOf("/");
      const displayName = slashIdx >= 0
        ? name.substring(slashIdx + 1)
        : name;
      const cluster = slashIdx >= 0
        ? name.substring(0, slashIdx)
        : "";
      currentPods.push({
        name,
        displayName,
        cluster,
        type: "pod",
      });
    }
  }

  // Clean up stale pod entries only from clusters that were
  // actually polled — preserve pods from offline/backoff clusters
  for (const key of Object.keys(metricsCache)) {
    if (!key.includes("/")) {
      continue;
    }
    const cluster = key.substring(0, key.indexOf("/"));
    if (polledClusters.has(cluster) && !receivedPodNames.has(key)) {
      delete metricsCache[key];
      delete metricsHistory[key];
      delete anomalyState[key];
    }
  }

  // Prune history/anomaly for servers no longer configured
  const activeServerNames = new Set(servers.map((s) => s.name));
  for (const key of Object.keys(metricsHistory)) {
    if (!key.includes("/") && !activeServerNames.has(key)) {
      delete metricsHistory[key];
      delete anomalyState[key];
    }
  }

  pods = currentPods;
  wardEye.classList.remove("syncing");
  syncLabel.classList.remove("visible");

  let hasAlert = false;
  for (const [key, m] of Object.entries(metricsCache)) {
    if (m.error) {
      continue;
    }
    // Only check servers/pods that are still active
    const isActivePod = key.includes("/") && receivedPodNames.has(key);
    const isActiveServer = !key.includes("/") && activeServerNames.has(key);
    if (!isActivePod && !isActiveServer) {
      continue;
    }
    if (m.cpu >= 85 || m.mem >= 85 || m.disk >= 85) {
      hasAlert = true;
      break;
    }
  }
  wardEye.classList.toggle("alert", hasAlert);

  renderAll();
}

// Grafana Alerts

let grafanaConfigured = false;

function renderAlertRow(alert) {
  const sevClass = alertSeverityClass(alert.severity);
  const suppressed = alert.state === "suppressed" ? " suppressed" : "";
  const age = formatAge(alert.starts_at);
  const ageHtml = age ? `<span class="alert-age">${age}</span>` : "";
  const summary = alert.summary || alert.name;
  const url = alert.generator_url || "";
  return `
    <div class="alert-row ${sevClass}${suppressed}"
         data-alert-url="${escapeHtml(url)}">
      <span class="alert-dot"></span>
      <div class="alert-text">
        <span class="alert-name">${escapeHtml(alert.name)}</span>
        <span class="alert-summary">${escapeHtml(summary)}</span>
      </div>
      ${ageHtml}
    </div>`;
}

function handleAlertsUpdate(event) {
  const payload = event.payload;
  if (!payload) {
    return;
  }

  if (!grafanaConfigured) {
    alertsSection.classList.add("hidden");
    resizeToContent();
    return;
  }
  alertsSection.classList.remove("hidden");

  if (payload.source_error) {
    alertsStatus.textContent = "unreachable";
    alertsStatus.classList.add("error");
    alertsList.innerHTML =
      `<div class="alerts-empty">${escapeHtml(payload.source_error)}</div>`;
    resizeToContent();
    return;
  }

  alertsStatus.classList.remove("error");
  const alerts = Array.isArray(payload.alerts) ? payload.alerts : [];
  const rank = { critical: 0, warning: 1, info: 2, unknown: 3 };
  alerts.sort((a, b) => {
    const sa = a.state === "suppressed" ? 10 : 0;
    const sb = b.state === "suppressed" ? 10 : 0;
    return (sa + (rank[a.severity] ?? 3)) - (sb + (rank[b.severity] ?? 3));
  });

  const active = alerts.filter((a) => a.state !== "suppressed").length;
  alertsStatus.textContent = active > 0 ? `${active} firing` : "all clear";

  if (alerts.length === 0) {
    alertsList.innerHTML = '<div class="alerts-empty">No active alerts</div>';
  } else {
    alertsList.innerHTML = alerts.map(renderAlertRow).join("");
  }
  resizeToContent();
}

// ── Init ──────────────────────────────────────

async function init() {
  try {
    const config = await invoke("get_config");
    servers = config.servers;
    grafanaConfigured = !!(config.grafana && config.grafana.enabled);
    if (!grafanaConfigured) {
      alertsSection.classList.add("hidden");
    }
    renderAll();
  } catch (err) {
    console.error("Failed to load config:", err);
    renderAll();
  }

  await listen("poll-start", () => {
    wardEye.classList.add("syncing");
    syncLabel.classList.add("visible");
  });
  await listen("metrics-update", handleMetricsUpdate);
  await listen("alerts-update", handleAlertsUpdate);
}

// ── Event Bindings ────────────────────────────

document.getElementById("btn-open-add")
  .addEventListener("click", openAddForm);

addFormEl.addEventListener("submit", handleAddServer);

btnCancel.addEventListener("click", closeAddForm);

typeSelect.addEventListener("change", toggleTypeFields);

ctxRemove.addEventListener("click", handleRemoveServer);
ctxOpenTerminal.addEventListener("click", handleOpenTerminal);
ctxCopyKubectl.addEventListener("click", handleCopyKubectl);
ctxViewLogs.addEventListener("click", handleViewLogs);
ctxCopyMetrics.addEventListener("click", handleCopyMetrics);

document.getElementById("btn-settings")
  .addEventListener("click", openSettings);

document.getElementById("btn-close-settings")
  .addEventListener("click", closeSettings);

document.getElementById("btn-save-settings")
  .addEventListener("click", saveSettings);

serverListEl.addEventListener("click", (e) => {
  const logsBtn = e.target.closest(".btn-logs");
  if (logsBtn) {
    e.stopPropagation();
    const fullName = logsBtn.dataset.podFull;
    if (fullName) {
      openPodLogs(fullName);
    }
    return;
  }
  const summary = e.target.closest(".cluster-summary");
  if (!summary) {
    return;
  }
  const cluster = summary.dataset.cluster;
  if (!cluster) {
    return;
  }
  if (collapsedClusters.has(cluster)) {
    collapsedClusters.delete(cluster);
    expandedOnce.add(cluster);
  } else {
    collapsedClusters.add(cluster);
  }
  renderAll();
});

alertsList.addEventListener("click", (e) => {
  const row = e.target.closest(".alert-row");
  if (!row) {
    return;
  }
  const url = row.dataset.alertUrl;
  if (url) {
    invoke("open_url", { url }).catch((err) =>
      console.error("Failed to open alert URL:", err));
  }
});

serverListEl.addEventListener("contextmenu", (e) => {
  const card = e.target.closest(".server-card");
  if (!card) {
    return;
  }
  const name = card.dataset.serverName;
  if (!name) {
    return;
  }
  const isPod = card.dataset.cardType === "pod";
  let serverType = card.dataset.serverType || "";
  if (isPod) {
    const slashIdx = name.indexOf("/");
    const clusterName = slashIdx >= 0 ? name.substring(0, slashIdx) : "";
    const cfg = findServerConfig(clusterName);
    serverType = cfg ? cfg.type : "k8s";
  }
  showContextMenu(e, {
    name,
    cardType: isPod ? "pod" : "server",
    serverType,
  });
});

document.addEventListener("click", (e) => {
  if (!contextMenu.contains(e.target)) {
    hideContextMenu();
  }
});

serverListEl.addEventListener("dblclick", (e) => {
  const nameEl = e.target.closest(".server-name, .cluster-name");
  if (!nameEl || nameEl.querySelector(".alias-input")) {
    return;
  }

  const card = nameEl.closest(".server-card");
  const summary = nameEl.closest(".cluster-summary");
  let fullName;
  if (card) {
    fullName = card.dataset.serverName;
  } else if (summary) {
    fullName = summary.dataset.cluster;
  }
  if (!fullName) {
    return;
  }

  const currentText = nameEl.textContent;
  const input = document.createElement("input");
  input.type = "text";
  input.className = "alias-input";
  input.value = currentText;
  input.maxLength = 30;

  nameEl.textContent = "";
  nameEl.appendChild(input);
  input.focus();
  input.select();

  function commit() {
    const val = input.value.trim();
    const podCard = nameEl.closest("[data-card-type='pod']");
    let fallback = fullName;
    if (podCard) {
      const slashIdx = fullName.indexOf("/");
      fallback = slashIdx >= 0
        ? fullName.substring(slashIdx + 1)
        : fullName;
    }
    if (val && val !== fallback) {
      saveAlias(fullName, val);
    } else {
      saveAlias(fullName, "");
    }
    renderAll();
  }

  input.addEventListener("blur", commit);
  input.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") {
      ev.preventDefault();
      input.blur();
    }
    if (ev.key === "Escape") {
      ev.preventDefault();
      input.removeEventListener("blur", commit);
      renderAll();
    }
  });
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    hideContextMenu();
    closeAddForm();
    closeSettings();
  }
});

init();

"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const WIN_WIDTH = 380;
const WIN_MIN_HEIGHT = 100;
const WIN_PADDING = 20;
const VISIBLE_POD_CARDS = 3;

const UNITS = ["B/s", "KB/s", "MB/s", "GB/s"];

const TRASH_ICON = `<svg class="trash-icon" viewBox="0 0 16 16" aria-hidden="true">
  <path d="M2.5 4h11M6.5 4V2.5h3V4M4 4l.7 9.5h6.6L12 4M6.75 6.5v4.5M9.25 6.5v4.5"/>
</svg>`;
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

let aliasesCache = loadAliases();

function saveAlias(fullName, alias) {
  if (alias && alias.trim()) {
    aliasesCache[fullName] = alias.trim();
  } else {
    delete aliasesCache[fullName];
  }
  localStorage.setItem(ALIAS_KEY, JSON.stringify(aliasesCache));
}

function getDisplayName(fullName, fallback) {
  return aliasesCache[fullName] || fallback;
}

// ── DOM refs ──────────────────────────────────

const wardEye = document.querySelector(".ward-eye");
const syncLabel = document.getElementById("sync-label");
const serverListEl = document.getElementById("server-list");
const addFormPanel = document.getElementById("add-form-panel");
const addFormEl = document.getElementById("add-form");
const btnCancel = document.getElementById("btn-cancel");
const typeSelect = document.getElementById("field-type");
const kubeconfigInput = document.getElementById("field-kubeconfig");
const contextInput = document.getElementById("field-context");
const contextOptions = document.getElementById("context-options");
const btnBrowseKubeconfig = document.getElementById("btn-browse-kubeconfig");
const keyPathInput = document.getElementById("field-keypath");
const btnBrowseKeyPath = document.getElementById("btn-browse-keypath");
const formError = document.getElementById("form-error");
const contextMenu = document.getElementById("context-menu");

const k8sFields = document.querySelectorAll(".field-k8s");
const sshFields = document.querySelectorAll(".field-ssh");

// ── Window Sizing ────────────────────────────

// Padding around an open modal; matches `.modal-overlay` in styles.css.
const MODAL_INSET = 20;

// An open modal can be taller than the list it covers, and the window
// clips it: stretch the container to fit it while it is open.
function fitOpenModal() {
  const container = document.querySelector(".container");
  const modal = document.querySelector(".modal-overlay.open .modal");
  container.style.minHeight = modal
    ? `${modal.offsetHeight + 2 * MODAL_INSET}px`
    : "";
  return container.offsetHeight;
}

function resizeToContent() {
  const header = document.querySelector(".header");
  const footer = document.querySelector(".footer");

  const alerts = document.getElementById("alerts-section");
  const contentHeight =
    header.offsetHeight +
    addFormPanel.offsetHeight +
    (alerts ? alerts.offsetHeight : 0) +
    serverListEl.offsetHeight +
    settingsPanel.offsetHeight +
    footer.offsetHeight +
    WIN_PADDING;

  const height = Math.max(WIN_MIN_HEIGHT, contentHeight, fitOpenModal() + WIN_PADDING);
  invoke("resize_window", { width: WIN_WIDTH, height }).catch(() => {});
}

function snapshotPodScroll() {
  const pos = {};
  for (const list of document.querySelectorAll(".cluster-pods")) {
    const cluster = list.dataset.clusterPods;
    if (cluster) {
      pos[cluster] = list.scrollTop;
    }
  }
  return pos;
}

function restorePodScroll(pos) {
  for (const list of document.querySelectorAll(".cluster-pods")) {
    const cluster = list.dataset.clusterPods;
    if (cluster && pos[cluster]) {
      list.scrollTop = pos[cluster];
    }
  }
}

// Pod cards vary in height (metrics, PVC, events), so the 3-visible
// cap is measured from the first three cards rather than a CSS constant.
function capClusterPodLists() {
  for (const list of document.querySelectorAll(".cluster-pods")) {
    if (list.style.display === "none") {
      list.style.maxHeight = "";
      list.classList.remove("is-scrollable");
      continue;
    }
    const cards = list.querySelectorAll(":scope > .server-card");
    if (cards.length <= VISIBLE_POD_CARDS) {
      list.style.maxHeight = "";
      list.classList.remove("is-scrollable");
      continue;
    }
    let height = 0;
    for (let i = 0; i < VISIBLE_POD_CARDS; i += 1) {
      height += cards[i].offsetHeight;
    }
    list.style.maxHeight = `${height}px`;
    list.classList.add("is-scrollable");
  }
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
    return "rgba(255,69,58,0.30)";
  }
  if (levelClass === "level-warn") {
    return "rgba(255,159,10,0.30)";
  }
  return "rgba(50,215,75,0.30)";
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
  let reasonHtml = "";
  if (isOffline) {
    offlineHtml =
      '<span class="offline-label">offline</span>';
    if (metrics.reason) {
      // Clamped; the full chain is in the tooltip and "Copy Error".
      const reason = escapeHtml(metrics.reason);
      reasonHtml = `<div class="offline-reason" title="${reason}">${reason}</div>`;
    }
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
        ${renderRemoveButton(name)}
      </div>
      ${reasonHtml}
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
         data-cluster="${escapeHtml(pod.cluster)}"
         data-card-type="pod">
      <div class="server-card-header">
        <span class="status-dot ${statusClass}"></span>
        <span class="server-name">${escapeHtml(getDisplayName(name, pod.displayName))}</span>
        ${badgesHtml}
        <button class="btn-logs" data-pod-full="${escapeHtml(name)}">logs</button>
        ${renderHideButton(pod)}
        ${offlineHtml}
      </div>
      ${metricsHtml}
    </div>`;
}

// Pods are hidden by the workload that owns them (see `WorkloadRef` in
// config.rs), since pod names change on every restart and rollout.
function workloadLabel(workload) {
  return `${workload.kind.toLowerCase()} ${workload.name}`;
}

function sameWorkload(a, b) {
  return a.kind === b.kind && a.name === b.name;
}

function hiddenWorkloads(cluster, namespace) {
  const watched = findServerConfig(cluster)?.namespaces ?? [];
  return watched.find((ns) => ns.name === namespace)?.hidden ?? [];
}

// Pods are grouped per watched namespace: "cluster/namespace".
function groupKey(cluster, namespace) {
  return `${cluster}/${namespace}`;
}

function renderHideButton(pod) {
  const label = escapeHtml(`Hide ${workloadLabel(pod.workload)}`);
  return `<button type="button" class="btn-trash btn-hide"
    data-cluster="${escapeHtml(pod.cluster)}"
    data-namespace="${escapeHtml(pod.namespace)}"
    data-kind="${escapeHtml(pod.workload.kind)}"
    data-workload="${escapeHtml(pod.workload.name)}"
    title="${label}" aria-label="${label}">${TRASH_ICON}</button>`;
}

function renderHiddenChip(group) {
  const count = hiddenWorkloads(group.cluster, group.namespace).length;
  if (count === 0) {
    return "";
  }
  return `<button type="button" class="cluster-hidden"
    data-cluster="${escapeHtml(group.cluster)}"
    data-namespace="${escapeHtml(group.namespace)}"
    title="Show hidden workloads">${count} hidden</button>`;
}

// Only a cluster with another namespace can stop watching this one; the
// cluster card removes the last.
function renderUnwatchButton(group) {
  const watched = findServerConfig(group.cluster)?.namespaces ?? [];
  if (watched.length < 2) {
    return "";
  }
  const label = escapeHtml(`Stop watching ${group.namespace}`);
  return `<button type="button" class="btn-trash btn-unwatch"
    data-cluster="${escapeHtml(group.cluster)}"
    data-namespace="${escapeHtml(group.namespace)}"
    title="${label}" aria-label="${label}">${TRASH_ICON}</button>`;
}

function renderClusterSummary(group, clusterPods) {
  let totalCpu = 0;
  let totalMem = 0;

  for (const pod of clusterPods) {
    const m = metricsCache[pod.name];
    if (m && !m.error) {
      totalCpu += m.cpu_millicores || 0;
      totalMem += m.memory_bytes || 0;
    }
  }

  const collapsed = collapsedClusters.has(group.key);
  const chevronClass = collapsed ? "chevron collapsed" : "chevron";

  // The row carries the cluster's alias unless the row has its own.
  const label = `${getDisplayName(group.cluster, group.cluster)} / ${group.namespace}`;

  return `
    <div class="cluster-summary"
         data-cluster="${escapeHtml(group.cluster)}"
         data-group="${escapeHtml(group.key)}"
         data-label="${escapeHtml(label)}">
      <span class="${chevronClass}">&#x25B8;</span>
      <span class="cluster-name">${escapeHtml(getDisplayName(group.key, label))}</span>
      <span class="cluster-stat">${clusterPods.length} pods</span>
      <span class="cluster-stat">${formatMillicores(totalCpu)} CPU</span>
      <span class="cluster-stat">${formatMemory(totalMem)} MEM</span>
      ${renderHiddenChip(group)}
      ${renderUnwatchButton(group)}
    </div>`;
}

function renderAll() {
  if (document.querySelector(".alias-input")) {
    return;
  }
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

  // Every watched namespace gets a summary row, even with no pods: its
  // "hidden" chip and unwatch button live there. Config order first.
  const groups = new Map();
  for (const server of servers) {
    for (const ns of server.type === "k8s" ? server.namespaces : []) {
      const key = groupKey(server.name, ns.name);
      groups.set(key, { key, cluster: server.name, namespace: ns.name, pods: [] });
    }
  }
  for (const pod of pods) {
    const key = groupKey(pod.cluster, pod.namespace);
    if (!groups.has(key)) {
      groups.set(key, { key, cluster: pod.cluster, namespace: pod.namespace, pods: [] });
    }
    groups.get(key).pods.push(pod);
  }

  let podHtml = "";
  for (const group of groups.values()) {
    const cluster = group.key;
    const clusterPods = group.pods;
    if (!collapsedClusters.has(cluster) && !expandedOnce.has(cluster)) {
      collapsedClusters.add(cluster);
    }
    clusterPods.sort((a, b) => {
      const nameA = getDisplayName(a.name, a.displayName);
      const nameB = getDisplayName(b.name, b.displayName);
      return nameA.localeCompare(nameB, undefined, {
        numeric: true,
        sensitivity: "base",
      });
    });
    const collapsed = collapsedClusters.has(cluster);
    const displayStyle = collapsed ? "display: none;" : "";
    podHtml += renderClusterSummary(group, clusterPods);
    podHtml += `<div class="cluster-pods" data-cluster-pods="${escapeHtml(cluster)}" style="${displayStyle}">`;
    podHtml += clusterPods.map(renderPodCard).join("");
    podHtml += "</div>";
  }

  const podScroll = snapshotPodScroll();
  serverListEl.innerHTML = serverHtml + podHtml;
  capClusterPodLists();
  restorePodScroll(podScroll);
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
  inspectSeq += 1;
  addFormEl.reset();
  contextOptions.replaceChildren();
  formError.textContent = "";
  toggleTypeFields();
  addFormPanel.classList.add("open");
  resizeToContent();
  // The kubeconfig field starts empty, i.e. the default kubeconfig; offer
  // its contexts too. Quiet, since having no default kubeconfig is fine.
  loadKubeconfigContexts(null, { quiet: true });
}

function closeAddForm() {
  inspectSeq += 1;
  addFormPanel.classList.remove("open");
  formError.textContent = "";
  resizeToContent();
}

// Bumped per inspect request and when the form opens or closes, so only
// the latest response may touch the form.
let inspectSeq = 0;
// True while the visible form error came from an inspect, so a later
// inspect may clear it without wiping a validation or submit error.
let inspectErrorShown = false;

// Backend errors carry the full cause chain and can wrap over several
// lines, so the popover is resized to keep them visible.
function showFormError(message) {
  formError.textContent = message;
  inspectErrorShown = false;
  resizeToContent();
}

// Open a native file picker via `command` and write the chosen path into
// `input`. Resolves to the path, or null when cancelled or failed.
async function browseForPath(command, input, button) {
  showFormError("");
  // One picker at a time: the backend's blur guard is per dialog, so a
  // second overlapping picker could let the popover hide under it.
  btnBrowseKubeconfig.disabled = true;
  btnBrowseKeyPath.disabled = true;
  try {
    const path = await invoke(command, {
      current: input.value.trim() || null,
    });
    if (path) {
      input.value = path;
    }
    return path;
  } catch (err) {
    showFormError(String(err));
    return null;
  } finally {
    btnBrowseKubeconfig.disabled = false;
    btnBrowseKeyPath.disabled = false;
    button.focus();
  }
}

async function browseKubeconfig() {
  const path = await browseForPath(
    "pick_kubeconfig", kubeconfigInput, btnBrowseKubeconfig,
  );
  if (path) {
    await loadKubeconfigContexts(path);
  }
}

function browseKeyPath() {
  return browseForPath("pick_ssh_key", keyPathInput, btnBrowseKeyPath);
}

// Offer the kubeconfig's contexts as suggestions and prefill its
// current-context, so the context name is picked rather than typed.
// `path` null means the default kubeconfig; `quiet` hides failures.
async function loadKubeconfigContexts(path, { quiet = false } = {}) {
  inspectSeq += 1;
  const seq = inspectSeq;
  contextOptions.replaceChildren();
  let summary;
  try {
    summary = await invoke("inspect_kubeconfig", { path });
  } catch (err) {
    if (seq === inspectSeq && !quiet) {
      showFormError(String(err));
      inspectErrorShown = true;
    }
    return;
  }
  if (seq !== inspectSeq) {
    return;
  }
  if (inspectErrorShown) {
    showFormError("");
  }
  for (const name of summary.contexts) {
    const option = document.createElement("option");
    option.value = name;
    contextOptions.append(option);
  }
  // Replace a context this kubeconfig does not define (empty, or left
  // over from a previously picked file); keep one it does.
  const current = contextInput.value.trim();
  if (summary.current_context && !summary.contexts.includes(current)) {
    contextInput.value = summary.current_context;
  }
}

function buildServerConfig() {
  const name = document.getElementById("field-name").value.trim();
  const type = typeSelect.value;

  if (!name) {
    return { error: "Name is required" };
  }
  // Pod cards are keyed "cluster/namespace/pod"; the backend rejects "/" too.
  if (name.includes("/")) {
    return { error: "Name must not contain '/'" };
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
    // A context that is already configured gains this namespace instead.
    return {
      config: {
        type: "k8s", name, context, kubeconfig, namespaces: [{ name: namespace }],
      },
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
  showFormError("");

  const result = buildServerConfig();
  if (result.error) {
    showFormError(result.error);
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
    showFormError(String(err));
  }
}

// ── Context Menu ──────────────────────────────

const ctxOpenTerminal = document.getElementById("ctx-open-terminal");
const ctxCopyKubectl = document.getElementById("ctx-copy-kubectl");
const ctxViewLogs = document.getElementById("ctx-view-logs");
const ctxCopyMetrics = document.getElementById("ctx-copy-metrics");
const ctxCopyError = document.getElementById("ctx-copy-error");
const ctxRemove = document.getElementById("ctx-remove");
const ctxSeparator = contextMenu.querySelector(".context-menu-separator");

function renderRemoveButton(name) {
  return `<button type="button" class="btn-trash btn-remove"
    data-server-name="${escapeHtml(name)}"
    title="Remove" aria-label="Remove">${TRASH_ICON}</button>`;
}

function showContextMenu(e, target) {
  e.preventDefault();
  contextMenuTarget = target;

  const isPod = target.cardType === "pod";
  const isSSH = target.serverType === "ssh";
  const isK8s = target.serverType === "k8s";
  // Offline cards have no metrics to copy, only the failure reason.
  const reason = metricsCache[target.name]?.reason;

  // Choose items before measuring, so the clamp uses the real size.
  ctxOpenTerminal.style.display = isSSH ? "" : "none";
  ctxCopyKubectl.style.display = isK8s && !isPod ? "" : "none";
  ctxViewLogs.style.display = isPod ? "" : "none";
  ctxCopyMetrics.style.display = reason ? "none" : "";
  ctxCopyError.style.display = reason ? "" : "none";
  ctxSeparator.style.display = isPod ? "none" : "";
  ctxRemove.style.display = isPod ? "none" : "";

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
}

function hideContextMenu() {
  contextMenu.classList.remove("visible");
  contextMenuTarget = null;
}

function findServerConfig(name) {
  return servers.find((s) => s.name === name) || null;
}

const removeModal = document.getElementById("remove-modal");
const removeModalTitle = document.getElementById("remove-modal-title");
const removeModalBody = document.getElementById("remove-modal-body");
const btnRemoveCancel = document.getElementById("btn-remove-cancel");
const btnRemoveConfirm = document.getElementById("btn-remove-confirm");
// { name } removes a server; { cluster, namespace } stops watching one
// namespace of a cluster.
let pendingRemove = null;
let removeInFlight = false;

function serverKindLabel(name) {
  const cfg = findServerConfig(name);
  if (cfg && cfg.type === "k8s") {
    return "cluster";
  }
  if (cfg && cfg.type === "ssh") {
    return "server";
  }
  return "entry";
}

function openRemoveModal(name) {
  const label = getDisplayName(name, name);
  showRemoveModal(
    { name },
    `Remove ${serverKindLabel(name)}?`,
    `Remove "${label}"? This cannot be undone.`,
  );
}

function openUnwatchModal(cluster, namespace) {
  showRemoveModal(
    { cluster, namespace },
    "Stop watching namespace?",
    `Stop watching "${namespace}" in "${getDisplayName(cluster, cluster)}"? `
      + "Its hidden workloads are forgotten too.",
  );
}

function showRemoveModal(target, title, body) {
  hideContextMenu();
  pendingRemove = target;
  removeModalTitle.textContent = title;
  removeModalBody.textContent = body;
  removeModal.classList.add("open");
  resizeToContent();
  btnRemoveCancel.focus();
}

function closeRemoveModal() {
  if (removeInFlight) {
    return;
  }
  pendingRemove = null;
  removeModal.classList.remove("open");
  resizeToContent();
}

async function confirmRemove() {
  if (removeInFlight) {
    return;
  }
  const target = pendingRemove;
  if (!target) {
    closeRemoveModal();
    return;
  }
  removeInFlight = true;
  btnRemoveConfirm.disabled = true;
  btnRemoveCancel.disabled = true;
  try {
    if (target.namespace) {
      await unwatchNamespace(target.cluster, target.namespace);
    } else {
      await removeServerByName(target.name);
    }
    removeInFlight = false;
    closeRemoveModal();
  } catch (err) {
    removeModalBody.textContent = String(err);
  } finally {
    removeInFlight = false;
    btnRemoveConfirm.disabled = false;
    btnRemoveCancel.disabled = false;
  }
}

async function removeServerByName(name) {
  hideContextMenu();
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
}

async function unwatchNamespace(cluster, namespace) {
  const config = await invoke("unwatch_namespace", { server: cluster, namespace });
  servers = config.servers;
  pods = pods.filter((p) => {
    const gone = p.cluster === cluster && p.namespace === namespace;
    if (gone) {
      forgetCard(p.name);
    }
    return !gone;
  });
  renderAll();
}

function forgetCard(key) {
  delete metricsCache[key];
  delete metricsHistory[key];
  delete anomalyState[key];
}

// Hiding is undone from the "hidden" chip, so it needs no confirmation.
// The cards go at once; the backend drops them from every later poll.
async function hideWorkload(cluster, namespace, workload) {
  try {
    const config = await invoke("hide_workload", { server: cluster, namespace, workload });
    servers = config.servers;
  } catch (err) {
    openHiddenModal(cluster, namespace, `Could not hide ${workloadLabel(workload)}: ${err}`);
    return;
  }
  pods = pods.filter((p) => {
    const hidden = p.cluster === cluster
      && p.namespace === namespace
      && sameWorkload(p.workload, workload);
    if (hidden) {
      forgetCard(p.name);
    }
    return !hidden;
  });
  renderAll();
}

const hiddenModal = document.getElementById("hidden-modal");
const hiddenModalTitle = document.getElementById("hidden-modal-title");
const hiddenModalList = document.getElementById("hidden-modal-list");
const hiddenModalError = document.getElementById("hidden-modal-error");
const btnHiddenClose = document.getElementById("btn-hidden-close");
// { cluster, namespace } whose hidden workloads the modal lists.
let hiddenModalTarget = null;

function renderHiddenModalList() {
  const { cluster, namespace } = hiddenModalTarget;
  const workloads = hiddenWorkloads(cluster, namespace);
  hiddenModalList.innerHTML = workloads.length === 0
    ? '<li class="hidden-empty">Nothing hidden</li>'
    : workloads.map((w) => `
      <li class="hidden-row">
        <span class="hidden-kind">${escapeHtml(w.kind.toLowerCase())}</span>
        <span class="hidden-name">${escapeHtml(w.name)}</span>
        <button type="button" class="btn-show"
          data-kind="${escapeHtml(w.kind)}"
          data-workload="${escapeHtml(w.name)}">show</button>
      </li>`).join("");
}

function openHiddenModal(cluster, namespace, error = "") {
  hideContextMenu();
  hiddenModalTarget = { cluster, namespace };
  hiddenModalTitle.textContent =
    `Hidden in ${getDisplayName(cluster, cluster)} / ${namespace}`;
  hiddenModalError.textContent = error;
  hiddenModalError.hidden = !error;
  renderHiddenModalList();
  hiddenModal.classList.add("open");
  resizeToContent();
  btnHiddenClose.focus();
}

function closeHiddenModal() {
  const target = hiddenModalTarget;
  hiddenModalTarget = null;
  hiddenModal.classList.remove("open");
  resizeToContent();
  if (!target) {
    return;
  }
  // Back to the chip that opened it, when it is still there.
  const cluster = CSS.escape(target.cluster);
  const namespace = CSS.escape(target.namespace);
  serverListEl
    .querySelector(`.cluster-hidden[data-cluster="${cluster}"][data-namespace="${namespace}"]`)
    ?.focus();
}

// The pods come back with the poll that `show_workload` wakes.
async function showWorkload(workload) {
  const { cluster, namespace } = hiddenModalTarget;
  try {
    const config = await invoke("show_workload", { server: cluster, namespace, workload });
    servers = config.servers;
  } catch (err) {
    hiddenModalError.textContent = `Could not show ${workloadLabel(workload)}: ${err}`;
    hiddenModalError.hidden = false;
    return;
  }
  hiddenModalError.hidden = true;
  if (hiddenWorkloads(cluster, namespace).length === 0) {
    closeHiddenModal();
  } else {
    renderHiddenModalList();
    btnHiddenClose.focus();
  }
  renderAll();
}

function handleRemoveServer() {
  if (!contextMenuTarget) {
    return;
  }
  openRemoveModal(contextMenuTarget.name);
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
    await invoke("open_ssh_terminal", { name: cfg.name });
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
  // A cluster watches several namespaces, so the command names none.
  let cmd = `kubectl --context ${q(cfg.context)}`;
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
  await openPodLogs(fullName);
}

async function handleCopyError() {
  if (!contextMenuTarget) {
    return;
  }
  const reason = metricsCache[contextMenuTarget.name]?.reason;
  hideContextMenu();
  if (!reason) {
    return;
  }
  try {
    await invoke("copy_to_clipboard", { text: reason });
  } catch (err) {
    console.error("Failed to copy error:", err);
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
  const pod = pods.find((p) => p.name === fullName);
  if (!pod) {
    return;
  }
  try {
    // The backend resolves context and kubeconfig from config.
    await invoke("open_pod_logs", {
      server: pod.cluster,
      namespace: pod.namespace,
      pod: pod.displayName,
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
const settingsError = document.getElementById("settings-error");

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
  settingsError.textContent = "";
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
    const settings = {
      foreground_poll_secs: fgInterval,
      background_poll_secs: bgInterval,
      notifications_enabled: notificationsToggle.checked,
      grafana: null,
    };
    const grafanaEnabled = grafanaEnabledToggle.checked;
    const grafanaUrl = grafanaUrlInput.value.trim();
    if (grafanaUrl) {
      settings.grafana = {
        name: GRAFANA_CONN_NAME,
        url: grafanaUrl,
        verify_tls: grafanaVerifyTlsToggle.checked,
        enabled: grafanaEnabled,
      };
    }
    // Store the token first: if the Keychain refuses it, nothing is saved,
    // instead of a saved URL that then polls without a token.
    const tokenValue = grafanaTokenInput.value.trim();
    if (tokenValue) {
      await invoke("set_grafana_token", {
        name: GRAFANA_CONN_NAME,
        token: tokenValue,
      });
    }
    await invoke("save_settings", { settings });
    grafanaConfigured = grafanaEnabled && !!grafanaUrl;
    if (!grafanaConfigured) {
      alertsSection.classList.add("hidden");
    }
  } catch (err) {
    console.error("Failed to save settings:", err);
    settingsError.textContent = String(err);
    return;
  }

  try {
    if (autostartToggle.checked) {
      await invoke("plugin:autostart|enable");
    } else {
      await invoke("plugin:autostart|disable");
    }
  } catch (err) {
    console.error("Failed to update autostart:", err);
    settingsError.textContent = String(err);
    return;
  }

  closeSettings();
}

// ── Metrics Event Listener ────────────────────

// Flatten one card's report into the shape the renderers read. Missing
// values (no sample yet, no PVC, first poll without a rate) become 0/"".
function cardMetrics(src) {
  const usage = src.usage || {};
  return {
    cpu: usage.cpu_percent ?? 0,
    mem: usage.memory_percent ?? 0,
    disk: usage.disk_percent ?? 0,
    net_tx: src.net?.tx_bytes_per_sec ?? 0,
    net_rx: src.net?.rx_bytes_per_sec ?? 0,
    cpu_millicores: src.cpu_millicores ?? 0,
    memory_bytes: src.memory_bytes ?? 0,
    restart_count: src.restart_count ?? 0,
    start_time: src.start_time ?? "",
    pod_status: src.pod_status ?? "",
    pvc_used_bytes: src.pvc_used_bytes ?? 0,
    pvc_capacity_bytes: src.pvc_capacity_bytes ?? 0,
    last_event: src.last_event ?? "",
    disk_used_bytes: src.disk_used_bytes ?? 0,
    disk_capacity_bytes: src.disk_capacity_bytes ?? 0,
    node_count: src.node_count ?? 0,
  };
}

// Append one sample to a card's sparkline history and refresh its
// anomaly flags.
function recordHistory(name, metrics) {
  if (!metricsHistory[name]) {
    metricsHistory[name] = { cpu: [], mem: [], disk: [] };
  }
  const h = metricsHistory[name];
  h.cpu.push(metrics.cpu);
  h.mem.push(metrics.mem);
  h.disk.push(metrics.disk);
  if (h.cpu.length > HISTORY_MAX) { h.cpu.shift(); }
  if (h.mem.length > HISTORY_MAX) { h.mem.shift(); }
  if (h.disk.length > HISTORY_MAX) { h.disk.shift(); }

  anomalyState[name] = {
    cpu: computeAnomaly(h.cpu).isAnomaly,
    mem: computeAnomaly(h.mem).isAnomaly,
    disk: computeAnomaly(h.disk).isAnomaly,
  };
}


function handleMetricsUpdate(event) {
  const payload = event.payload;
  if (!payload || !Array.isArray(payload.servers)) {
    return;
  }

  const currentPods = [];
  const receivedPodNames = new Set();
  const polledGroups = new Set();
  // Against the config as it is now: a namespace unwatched while this poll
  // ran gets no cards.
  const watchedGroups = new Set(servers.flatMap((s) =>
    s.type === "k8s" ? s.namespaces.map((ns) => groupKey(s.name, ns.name)) : []));

  for (const report of payload.servers) {
    const name = report.name;
    if (!name) {
      continue;
    }

    if (report.status !== "online") {
      metricsCache[name] = { error: "offline", reason: report.error || "" };
      continue;
    }

    const m = report.metrics || {};
    metricsCache[name] = cardMetrics({
      usage: m,
      net: m.net,
      cpu_millicores: m.cpu_millicores,
      memory_bytes: m.memory_bytes,
      disk_used_bytes: m.disk?.used_bytes,
      disk_capacity_bytes: m.disk?.capacity_bytes,
      node_count: m.nodes?.length,
    });
    recordHistory(name, metricsCache[name]);

    if (report.kind !== "k8s") {
      continue;
    }
    for (const ns of m.namespaces ?? []) {
      // `pods` is null when the backend failed to list them: only a
      // successful listing is authoritative enough to prune old cards.
      if (!Array.isArray(ns.pods) || !watchedGroups.has(groupKey(name, ns.name))) {
        continue;
      }
      polledGroups.add(groupKey(name, ns.name));
      // The backend filters hidden pods too; this covers a poll already
      // in flight when a workload was hidden.
      const hidden = hiddenWorkloads(name, ns.name);
      for (const pod of ns.pods) {
        if (hidden.some((w) => sameWorkload(w, pod.workload))) {
          continue;
        }
        const key = `${groupKey(name, ns.name)}/${pod.name}`;
        metricsCache[key] = cardMetrics({
          usage: pod.usage,
          net: pod.net,
          cpu_millicores: pod.usage?.cpu_millicores,
          memory_bytes: pod.usage?.memory_bytes,
          restart_count: pod.restart_count,
          start_time: pod.start_time,
          pod_status: pod.status,
          pvc_used_bytes: pod.pvc?.used_bytes,
          pvc_capacity_bytes: pod.pvc?.capacity_bytes,
          last_event: pod.last_event,
        });
        recordHistory(key, metricsCache[key]);
        receivedPodNames.add(key);
        currentPods.push({
          name: key,
          displayName: pod.name,
          cluster: name,
          namespace: ns.name,
          workload: pod.workload,
          type: "pod",
        });
      }
    }
  }

  // Keys are server names or "cluster/namespace/pod". A legacy server name
  // may contain "/" too, so server keys are recognised by name, not shape.
  const activeServerNames = new Set(servers.map((s) => s.name));
  const isPodKey = (key) => !activeServerNames.has(key) && key.includes("/");
  // Pod and namespace names never contain "/", so the last one splits off
  // the pod, even under a legacy cluster name.
  const groupOf = (key) => key.substring(0, key.lastIndexOf("/"));

  // Clean up stale pod entries only from namespaces that were actually
  // polled (keeping those of offline or backed-off clusters), and every
  // pod of a namespace or cluster no longer watched.
  for (const key of Object.keys(metricsHistory).concat(Object.keys(metricsCache))) {
    if (!isPodKey(key)) {
      if (!activeServerNames.has(key)) {
        delete metricsHistory[key];
        delete anomalyState[key];
      }
      continue;
    }
    const group = groupOf(key);
    const stale = polledGroups.has(group) && !receivedPodNames.has(key);
    if (stale || !watchedGroups.has(group)) {
      delete metricsCache[key];
      delete metricsHistory[key];
      delete anomalyState[key];
    }
  }

  for (const pod of pods) {
    const group = groupKey(pod.cluster, pod.namespace);
    const kept = watchedGroups.has(group) && !polledGroups.has(group);
    if (kept && !receivedPodNames.has(pod.name)) {
      currentPods.push(pod);
    }
  }

  pods = currentPods;
  const activePodNames = new Set(pods.map((p) => p.name));
  wardEye.classList.remove("syncing");
  syncLabel.classList.remove("visible");

  let hasAlert = false;
  for (const [key, m] of Object.entries(metricsCache)) {
    if (m.error) {
      continue;
    }
    const isActivePod = isPodKey(key) && activePodNames.has(key);
    const isActiveServer = activeServerNames.has(key);
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

  try {
    const latest = await invoke("get_latest_metrics");
    if (latest) {
      handleMetricsUpdate({ payload: latest });
    }
  } catch (err) {
    console.error("Failed to load cached metrics:", err);
  }
  try {
    const alerts = await invoke("get_latest_alerts");
    if (alerts) {
      handleAlertsUpdate({ payload: alerts });
    }
  } catch (err) {
    console.error("Failed to load cached alerts:", err);
  }
}

// ── Event Bindings ────────────────────────────

document.getElementById("btn-open-add")
  .addEventListener("click", openAddForm);

addFormEl.addEventListener("submit", handleAddServer);

btnCancel.addEventListener("click", closeAddForm);

typeSelect.addEventListener("change", toggleTypeFields);

btnBrowseKubeconfig.addEventListener("click", browseKubeconfig);
btnBrowseKeyPath.addEventListener("click", browseKeyPath);

// A hand-typed path is checked as soon as the field is left, not only on Add.
kubeconfigInput.addEventListener("change", () => {
  const path = kubeconfigInput.value.trim();
  if (path) {
    loadKubeconfigContexts(path);
  } else {
    loadKubeconfigContexts(null, { quiet: true });
  }
});

ctxRemove.addEventListener("click", handleRemoveServer);
ctxOpenTerminal.addEventListener("click", handleOpenTerminal);
ctxCopyKubectl.addEventListener("click", handleCopyKubectl);
ctxViewLogs.addEventListener("click", handleViewLogs);
ctxCopyMetrics.addEventListener("click", handleCopyMetrics);
ctxCopyError.addEventListener("click", handleCopyError);

document.getElementById("btn-settings")
  .addEventListener("click", openSettings);

document.getElementById("btn-quit")
  .addEventListener("click", () => {
    invoke("quit_app").catch((err) => {
      console.error("Failed to quit:", err);
    });
  });

document.getElementById("btn-close-settings")
  .addEventListener("click", closeSettings);

document.getElementById("btn-save-settings")
  .addEventListener("click", saveSettings);

btnHiddenClose.addEventListener("click", closeHiddenModal);
hiddenModal.addEventListener("click", (e) => {
  if (e.target === hiddenModal) {
    closeHiddenModal();
    return;
  }
  const btn = e.target.closest(".btn-show");
  if (btn) {
    showWorkload({ kind: btn.dataset.kind, name: btn.dataset.workload });
  }
});
hiddenModal.addEventListener("keydown", (e) => {
  if (e.key !== "Tab" || !hiddenModal.classList.contains("open")) {
    return;
  }
  const order = [...hiddenModal.querySelectorAll("button")];
  const idx = order.indexOf(document.activeElement);
  e.preventDefault();
  const step = e.shiftKey ? -1 : 1;
  const next = idx < 0 ? 0 : (idx + step + order.length) % order.length;
  order[next].focus();
});

btnRemoveCancel.addEventListener("click", closeRemoveModal);
btnRemoveConfirm.addEventListener("click", confirmRemove);
removeModal.addEventListener("click", (e) => {
  if (e.target === removeModal) {
    closeRemoveModal();
  }
});
removeModal.addEventListener("keydown", (e) => {
  if (e.key !== "Tab" || !removeModal.classList.contains("open")) {
    return;
  }
  const order = [btnRemoveCancel, btnRemoveConfirm];
  const idx = order.indexOf(document.activeElement);
  e.preventDefault();
  if (e.shiftKey) {
    const prev = idx <= 0 ? order.length - 1 : idx - 1;
    order[prev].focus();
  } else {
    const next = idx < 0 || idx === order.length - 1 ? 0 : idx + 1;
    order[next].focus();
  }
});

serverListEl.addEventListener("click", (e) => {
  const removeBtn = e.target.closest(".btn-remove");
  if (removeBtn) {
    e.stopPropagation();
    const name = removeBtn.dataset.serverName;
    if (name) {
      openRemoveModal(name);
    }
    return;
  }
  const unwatchBtn = e.target.closest(".btn-unwatch");
  if (unwatchBtn) {
    e.stopPropagation();
    const { cluster, namespace } = unwatchBtn.dataset;
    if (cluster && namespace) {
      openUnwatchModal(cluster, namespace);
    }
    return;
  }
  const hideBtn = e.target.closest(".btn-hide");
  if (hideBtn) {
    e.stopPropagation();
    const { cluster, namespace, kind, workload } = hideBtn.dataset;
    if (cluster && namespace && kind && workload) {
      hideWorkload(cluster, namespace, { kind, name: workload });
    }
    return;
  }
  const hiddenChip = e.target.closest(".cluster-hidden");
  if (hiddenChip) {
    e.stopPropagation();
    const { cluster, namespace } = hiddenChip.dataset;
    if (cluster && namespace) {
      openHiddenModal(cluster, namespace);
    }
    return;
  }
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
  const group = summary.dataset.group;
  if (!group) {
    return;
  }
  if (collapsedClusters.has(group)) {
    collapsedClusters.delete(group);
    expandedOnce.add(group);
  } else {
    collapsedClusters.add(group);
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
  const summary = e.target.closest(".cluster-summary");
  if (summary) {
    const name = summary.dataset.cluster;
    if (!name) {
      return;
    }
    const cfg = findServerConfig(name);
    showContextMenu(e, {
      name,
      cardType: "server",
      serverType: cfg ? cfg.type : "k8s",
    });
    return;
  }
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
    serverType = findServerConfig(card.dataset.cluster)?.type ?? "k8s";
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
    fullName = summary.dataset.group;
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
    let fallback = summary ? summary.dataset.label : fullName;
    if (podCard) {
      const slashIdx = fullName.lastIndexOf("/");
      fallback = slashIdx >= 0
        ? fullName.substring(slashIdx + 1)
        : fullName;
    }
    if (val && val !== fallback) {
      saveAlias(fullName, val);
    } else {
      saveAlias(fullName, "");
    }
    finishEdit();
    renderAll();
  }

  function finishEdit() {
    input.removeEventListener("blur", commit);
    input.remove();
  }

  input.addEventListener("blur", commit);
  input.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") {
      ev.preventDefault();
      input.blur();
    }
    if (ev.key === "Escape") {
      ev.preventDefault();
      finishEdit();
      renderAll();
    }
  });
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    if (removeModal.classList.contains("open")) {
      closeRemoveModal();
      return;
    }
    if (hiddenModal.classList.contains("open")) {
      closeHiddenModal();
      return;
    }
    hideContextMenu();
    closeAddForm();
    closeSettings();
  }
});

init();

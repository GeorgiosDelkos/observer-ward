// Renderers for the data pages: overview, cluster, namespace. Each returns
// HTML for the window's three regions from the state and the navigation.
// Interactive elements carry `data-action` (and the ids they act on);
// app.js dispatches on it.

import {
  age,
  bytesShort,
  capacity,
  cores,
  cpuCapacity,
  escapeHtml,
  kindBadge,
  middle,
  level,
  percentText,
  rate,
} from "./format.js";
import {
  attention,
  clusterLevel,
  clusters,
  errorOf,
  hiddenOf,
  hostLevel,
  hosts,
  isAnomaly,
  metricsOf,
  namespaceLevel,
  nodeLevel,
  podKey,
  podLevel,
  recentlyRestarted,
  serverConfig,
  state,
  workloads,
} from "./model.js";

/** Attention rows shown before "Show more". */
const ATTENTION_ROWS = 3;

// ── Icons ───────────────────────────────────────

const svg = (body, extra = "") =>
  `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" ${extra}>${body}</svg>`;

export const ICONS = {
  chev: svg('<path d="M6 3.5 10.5 8 6 12.5" stroke-width="1.6"/>', 'class="chev"'),
  back: svg('<path d="M10 3.5 5.5 8 10 12.5" stroke-width="1.8"/>'),
  plus: svg('<path d="M8 3v10M3 8h10" stroke-width="1.8"/>'),
  more: svg('<circle cx="3.5" cy="8" r="1.1" fill="currentColor" stroke="none"/><circle cx="8" cy="8" r="1.1" fill="currentColor" stroke="none"/><circle cx="12.5" cy="8" r="1.1" fill="currentColor" stroke="none"/>'),
  gear: svg('<circle cx="8" cy="8" r="2.2" stroke-width="1.5"/><path d="M8 1.8v1.6M8 12.6v1.6M1.8 8h1.6M12.6 8h1.6M3.6 3.6l1.1 1.1M11.3 11.3l1.1 1.1M3.6 12.4l1.1-1.1M11.3 4.7l1.1-1.1" stroke-width="1.5"/>'),
  power: svg('<path d="M8 2v5.5" stroke-width="1.6"/><path d="M4.6 4.4a5 5 0 1 0 6.8 0" stroke-width="1.6"/>'),
};

// ── Shared pieces ───────────────────────────────

const ALIAS_KEY = "ow-aliases";

function aliases() {
  try {
    return JSON.parse(localStorage.getItem(ALIAS_KEY) ?? "{}");
  } catch {
    return {};
  }
}

/** A server's display name: its alias when one is set. */
export function displayName(name) {
  return aliases()[name] || name;
}

export function saveAlias(name, alias) {
  const all = aliases();
  if (alias && alias !== name) {
    all[name] = alias;
  } else {
    delete all[name];
  }
  try {
    localStorage.setItem(ALIAS_KEY, JSON.stringify(all));
  } catch {
    // Storage refused: the alias lasts for this session's render only.
  }
}

const dot = (lvl) => `<span class="dot ${lvl}"></span>`;

function micro(percent) {
  const width = Math.max(0, Math.min(100, percent ?? 0));
  return `<span class="micro"><i class="${level(width)}" style="width:${width}%"></i></span>`;
}

function bar(label, percent) {
  return `<div class="bar"><span class="label">${label}</span>${micro(percent)}<span class="num">${percentText(percent)}%</span></div>`;
}

const LEVEL_COLOR = { ok: "var(--ok)", warn: "var(--warn)", crit: "var(--crit)" };

/** A ring gauge: usage arc over a faint requested arc, the value centred. */
function ring(label, percent, requested, caption) {
  const r = 27;
  const c = 2 * Math.PI * r;
  const arc = (pct, stroke, cls = "") =>
    `<circle class="${cls}" cx="32" cy="32" r="${r}" fill="none" stroke="${stroke}" stroke-width="5" stroke-linecap="round" stroke-dasharray="${(c * Math.min(100, pct)) / 100} ${c}"/>`;
  const reqArc = requested == null ? "" : arc(requested, "rgba(255,255,255,0.3)");
  const title = requested == null ? "" : ` title="${Math.round(requested)}% requested"`;
  return `<div class="gauge"${title}>
    <div class="ring"><svg viewBox="0 0 64 64" aria-hidden="true">
      <circle cx="32" cy="32" r="${r}" fill="none" stroke="rgba(255,255,255,0.06)" stroke-width="5"/>
      ${reqArc}${arc(Math.max(percent, 1.5), LEVEL_COLOR[level(percent)], "arc")}
    </svg><span class="pct"><span>${percentText(percent)}<small>%</small></span></span></div>
    <div class="g-cap"><span class="label">${label}</span><span class="v">${caption}</span></div>
  </div>`;
}

/** Area sparkline of a percentage series, ending in a dot. */
function spark(series, percent, id) {
  const values = series?.length > 1 ? series : [percent, percent];
  const w = 132;
  const h = 28;
  const max = Math.max(10, ...values) * 1.15;
  const points = values.map((v, i) => [(i / (values.length - 1)) * w, h - (v / max) * h]);
  const line = points.map(([x, y], i) => `${i ? "L" : "M"}${x.toFixed(1)} ${y.toFixed(1)}`).join(" ");
  const [ex, ey] = points[points.length - 1];
  const color = LEVEL_COLOR[level(percent)];
  const spike = isAnomaly(series) ? '<circle class="spike" r="6" fill="none" stroke-width="1.2"/>' : "";
  return `<svg class="spark" viewBox="0 0 ${w} ${h}" aria-hidden="true">
    <defs><linearGradient id="g-${id}" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="${color}" stop-opacity="0.28"/><stop offset="1" stop-color="${color}" stop-opacity="0"/>
    </linearGradient></defs>
    <path d="M0 ${h} L${line.slice(1)} L${w} ${h} Z" fill="url(#g-${id})"/>
    <path d="${line}" fill="none" stroke="${color}" stroke-width="1.5" filter="url(#glow)"/>
    <g transform="translate(${ex} ${ey})" stroke="${color}">${spike}<circle r="2.5" fill="${color}" stroke="none" filter="url(#glow)"/></g>
  </svg>`;
}

function liveText() {
  if (state.syncing) {
    return "Syncing";
  }
  if (!state.updatedAt) {
    return "Waiting";
  }
  const seconds = Math.max(0, Math.round((Date.now() - state.updatedAt) / 1000));
  return seconds < 2 ? "Live" : `Live · ${seconds}s`;
}

export function livePill() {
  return `<span class="live" id="live"><span class="pulse${state.syncing ? " busy" : ""}"></span><span id="live-text">${liveText()}</span></span>`;
}

export function updateLive() {
  const text = document.getElementById("live-text");
  if (text) {
    text.textContent = liveText();
  }
}

export function subTop({ back, title, sub, action = "", rename = "" }) {
  const renameAttr = rename ? ` data-rename="${escapeHtml(rename)}"` : "";
  return `<button class="round" data-action="back" data-to="${back}" aria-label="Back">${ICONS.back}</button>
    <span class="title grow"><b${renameAttr}>${escapeHtml(title)}</b><span>${escapeHtml(sub)}</span></span>${action}`;
}

function moreButton(action, attrs) {
  return `<button class="round" data-action="${action}" ${attrs} aria-label="More actions">${ICONS.more}</button>`;
}

export function bottom() {
  const sources = state.servers.length;
  const issues = attention().length;
  const issuesText = issues === 0 ? "all clear" : `${issues} ${issues === 1 ? "issue" : "issues"}`;
  return `<span class="foot-text">${sources} ${sources === 1 ? "source" : "sources"} · ${issuesText}</span>
    <span class="foot-actions">
      <button class="round" data-action="settings" aria-label="Settings" title="Settings">${ICONS.gear}</button>
      <button class="round" data-action="quit" aria-label="Quit Observer Ward" title="Quit">${ICONS.power}</button>
    </span>`;
}

// ── Overview ────────────────────────────────────

export function homeTop() {
  return `<span class="mark" aria-hidden="true"></span><span class="brand grow">Observer Ward</span>
    ${livePill()}
    <button class="round" data-action="add" aria-label="Add a cluster or host" title="Add a cluster or host">${ICONS.plus}</button>`;
}

export function homePage(nav) {
  if (state.servers.length === 0) {
    return `<div class="empty">
      <span class="mark big" aria-hidden="true"></span>
      <b>Nothing to watch yet</b>
      <p>Add a Kubernetes cluster or an SSH host to see its health here.</p>
      <button class="pill primary" data-action="add">Add a cluster or host</button>
    </div>`;
  }
  const parts = [attentionCard(nav)];
  const k8s = clusters();
  if (k8s.length > 0) {
    parts.push(sectionLabel("Clusters", k8s.length), ...k8s.map(clusterCard));
  }
  const ssh = hosts();
  if (ssh.length > 0) {
    parts.push(sectionLabel("Hosts", ssh.length), ...ssh.map(hostCard));
  }
  return parts.join("");
}

function sectionLabel(text, count) {
  return `<div class="sec"><span class="label">${text}</span><span class="count">${count}</span></div>`;
}

function attentionCard(nav) {
  const items = attention();
  if (items.length === 0) {
    return "";
  }
  const shown = nav.allIssues ? items : items.slice(0, ATTENTION_ROWS);
  const rows = shown.map((item) => {
    const go = item.go ? ` data-go='${escapeHtml(JSON.stringify(item.go))}'` : "";
    const url = item.url ? ` data-url="${escapeHtml(item.url)}"` : "";
    const clickable = item.go || item.url;
    const tag = clickable ? "button" : "div";
    const action = item.go ? ' data-action="go"' : item.url ? ' data-action="open-url"' : "";
    return `<${tag} class="a-row"${action}${go}${url}>
      <span class="a-ico ${item.level}">${item.icon}</span>
      <span class="a-txt"><b>${escapeHtml(item.title)}</b><span>${escapeHtml(item.detail)}</span></span>
      ${clickable ? ICONS.chev : ""}</${tag}>`;
  });
  const hidden = items.length - shown.length;
  const more = hidden > 0
    ? `<button class="a-more" data-action="all-issues">Show ${hidden} more</button>`
    : "";
  const tone = items[0].level;
  return `<div class="card alert ${tone}">${rows.join("")}${more}</div>`;
}

function clusterCard(server) {
  const name = server.name;
  const lvl = clusterLevel(name);
  // The context only adds information when it differs from the name.
  const context = server.context === name ? "" : server.context;
  const head = `<button class="card-top" data-action="cluster" data-cluster="${escapeHtml(name)}">
    ${dot(lvl)}<b>${escapeHtml(displayName(name))}</b><span class="ctx">${escapeHtml(context)}</span>${ICONS.chev}</button>`;
  const error = errorOf(name);
  if (error) {
    return `<div class="card">${head}<p class="err">${escapeHtml(error)}</p></div>`;
  }
  const metrics = metricsOf(name);
  if (!metrics) {
    return `<div class="card">${head}<p class="wait">Waiting for the first poll…</p></div>`;
  }
  return `<div class="card">${head}
    <div class="gauges">${clusterRings(metrics)}</div>
    ${nodeStrip(name, metrics.nodes)}
    <div class="list">${nsColumns("Pods")}${metrics.namespaces.map((ns) => nsRow(name, ns, false)).join("")}</div>
  </div>`;
}

function clusterRings(metrics) {
  const req = metrics.requested;
  const allocatable = allocatableOf(metrics.nodes);
  const cpuCaption = allocatable ? cpuCapacity(metrics.cpu_millicores, allocatable.cpu) : cores(metrics.cpu_millicores);
  const memCaption = allocatable ? capacity(metrics.memory_bytes, allocatable.mem) : bytesShort(metrics.memory_bytes);
  const diskCaption = metrics.disk.capacity_bytes > 0
    ? capacity(metrics.disk.used_bytes, metrics.disk.capacity_bytes)
    : "no stats";
  return [
    ring("CPU", metrics.cpu_percent, req?.cpu_percent ?? null, cpuCaption),
    ring("MEM", metrics.memory_percent, req?.memory_percent ?? null, memCaption),
    ring("DISK", metrics.disk_percent, null, diskCaption),
  ].join("");
}

/** Allocatable summed over the nodes that report it, as the backend does. */
function allocatableOf(nodes) {
  let cpu = 0;
  let mem = 0;
  for (const node of nodes) {
    if (node.allocatable) {
      cpu += node.allocatable.cpu_millicores;
      mem += node.allocatable.memory_bytes;
    }
  }
  return cpu > 0 ? { cpu, mem } : null;
}

function nodeStrip(cluster, nodes) {
  const ready = nodes.filter((n) => n.ready).length;
  const cells = nodes.map((node) => {
    const u = node.usage;
    const tip = u
      ? `${node.name} · cpu ${Math.round(u.cpu_percent)}% · mem ${Math.round(u.memory_percent)}% · disk ${Math.round(u.disk_percent)}%`
      : `${node.name} · no sample`;
    return `<button class="cell ${nodeLevel(node)}${node.ready ? "" : " down"}" data-action="go"
      data-go='${escapeHtml(JSON.stringify({ page: "cluster", cluster, tab: "nodes", open: node.name }))}'
      title="${escapeHtml(tip)}" aria-label="${escapeHtml(tip)}"></button>`;
  });
  return `<div class="nodes"><span class="label">Nodes</span><span class="cells">${cells.join("")}</span><span class="ready">${ready}/${nodes.length}</span></div>`;
}

function nsColumns(first) {
  return `<div class="cols ns-grid"><span></span><span class="label l">Namespace</span><span class="label">${first}</span><span class="label">CPU</span><span class="label">Mem</span><span></span></div>`;
}

function nsTotals(ns) {
  const pods = ns.pods ?? [];
  let mc = 0;
  let mem = 0;
  for (const pod of pods) {
    mc += pod.usage?.cpu_millicores ?? 0;
    mem += pod.usage?.memory_bytes ?? 0;
  }
  const running = pods.filter((p) => p.status === "Running").length;
  return { count: pods.length, running, mc, mem };
}

function nsRow(cluster, ns, readyColumn) {
  const t = nsTotals(ns);
  const restarting = (ns.pods ?? []).filter((p) => recentlyRestarted(podKey(cluster, ns.name, p.name))).length;
  const tag = restarting > 0 ? `<span class="tag warn" title="${restarting} restarted in the last hour">↻ ${restarting}</span>` : "";
  const first = readyColumn ? `${t.running}/${t.count}` : String(t.count);
  return `<button class="row ns-grid" data-action="namespace" data-cluster="${escapeHtml(cluster)}" data-namespace="${escapeHtml(ns.name)}">
    ${dot(ns.pods ? namespaceLevel(cluster, ns) : "idle")}
    <span class="name">${escapeHtml(ns.name)}${tag}</span>
    <span class="r num">${ns.pods ? first : "–"}</span><span class="r num">${cores(t.mc)}</span><span class="r num">${bytesShort(t.mem)}</span>${ICONS.chev}</button>`;
}

function hostCard(server) {
  const name = server.name;
  const head = `<div class="card-top">${dot(hostLevel(name))}<b data-rename="${escapeHtml(name)}">${escapeHtml(displayName(name))}</b>
    <span class="ctx">${escapeHtml(`${server.user}@${server.host}`)}</span>
    <span class="grow"></span>${moreButton("menu-host", `data-host="${escapeHtml(name)}"`)}</div>`;
  const error = errorOf(name);
  if (error) {
    return `<div class="card">${head}<p class="err">${escapeHtml(error)}</p></div>`;
  }
  const m = metricsOf(name);
  if (!m) {
    return `<div class="card">${head}<p class="wait">Waiting for the first poll…</p></div>`;
  }
  const stat = (label, pct) => `<div class="hs"><div class="hs-top"><span class="label">${label}</span><span class="num">${percentText(pct)}%</span></div>${micro(pct)}</div>`;
  const net = m.net
    ? `<div class="net"><span class="label">Net</span><span class="num">↑ ${rate(m.net.tx_bytes_per_sec)}</span><span class="num">↓ ${rate(m.net.rx_bytes_per_sec)}</span></div>`
    : "";
  return `<div class="card">${head}
    <div class="host-stats">${stat("CPU", m.cpu_percent)}${stat("MEM", m.memory_percent)}${stat("DISK", m.disk_percent)}</div>${net}</div>`;
}

// ── Cluster ─────────────────────────────────────

export function clusterTop(nav) {
  const server = serverConfig(nav.cluster);
  const metrics = metricsOf(nav.cluster);
  const nodes = metrics ? ` · ${metrics.nodes.length} nodes` : "";
  return subTop({
    back: "home",
    title: displayName(nav.cluster),
    sub: `${server?.context ?? ""}${nodes}`,
    rename: nav.cluster,
    action: moreButton("menu-cluster", `data-cluster="${escapeHtml(nav.cluster)}"`),
  });
}

export function clusterPage(nav) {
  const name = nav.cluster;
  const error = errorOf(name);
  if (error) {
    return `<div class="card"><p class="err big">${escapeHtml(error)}</p>
      <div class="actions pad"><button class="pill" data-action="copy" data-text="${escapeHtml(error)}">Copy error</button></div></div>`;
  }
  const metrics = metricsOf(name);
  if (!metrics) {
    return `<div class="card"><p class="wait">Waiting for the first poll…</p></div>`;
  }
  const nodesTab = nav.tab !== "namespaces";
  const seg = `<div class="seg" role="group" aria-label="Cluster view">
    <button data-action="tab" data-tab="nodes" aria-pressed="${nodesTab}">Nodes <span class="n">${metrics.nodes.length}</span></button>
    <button data-action="tab" data-tab="namespaces" aria-pressed="${!nodesTab}">Namespaces <span class="n">${metrics.namespaces.length}</span></button>
  </div>`;
  const body = nodesTab ? nodeList(name, metrics.nodes, nav.open) : namespaceList(name, metrics.namespaces);
  return `<div class="card"><div class="tele">${telemetry(name, metrics)}</div></div>${seg}${body}`;
}

function telemetry(name, metrics) {
  const history = state.history.get(name) ?? { cpu: [], mem: [], disk: [] };
  const allocatable = allocatableOf(metrics.nodes);
  const req = metrics.requested;
  const rows = [
    ["CPU", metrics.cpu_percent, history.cpu, allocatable ? cpuCapacity(metrics.cpu_millicores, allocatable.cpu) : cores(metrics.cpu_millicores), req?.cpu_percent],
    ["MEM", metrics.memory_percent, history.mem, allocatable ? capacity(metrics.memory_bytes, allocatable.mem) : bytesShort(metrics.memory_bytes), req?.memory_percent],
    ["DISK", metrics.disk_percent, history.disk, metrics.disk.capacity_bytes > 0 ? capacity(metrics.disk.used_bytes, metrics.disk.capacity_bytes) : "no stats", null],
  ];
  return rows.map(([label, pct, series, caption, requested]) => {
    const color = level(pct) === "ok" ? "" : ` style="color:${LEVEL_COLOR[level(pct)]}"`;
    const reqLine = requested == null ? "" : `<span>${Math.round(requested)}% requested</span>`;
    return `<div class="t-row"><span class="label">${label}</span>${spark(series, pct, `${label}`)}
      <span class="t-val"><b${color}>${percentText(pct)}%</b><span>${caption}</span>${reqLine}</span></div>`;
  }).join("");
}

function nodeList(cluster, nodes, open) {
  const rows = nodes.map((node) => {
    const expanded = open === node.name;
    const u = node.usage;
    const pods = podsOnNode(cluster, node.name).length;
    return `<div class="item"><button class="row node-grid" data-action="toggle" data-key="${escapeHtml(node.name)}" aria-expanded="${expanded}">
      ${dot(nodeLevel(node))}<span class="name m" title="${escapeHtml(node.name)}">${escapeHtml(middle(node.name, 19))}</span>
      ${micro(u?.cpu_percent)}${micro(u?.memory_percent)}${micro(node.disk ? diskPercent(node.disk) : 0)}
      <span class="r num">${pods}</span></button>${expanded ? nodeDetail(cluster, node) : ""}</div>`;
  });
  return `<div class="card"><div class="list lead">
    <div class="cols node-grid"><span></span><span class="label l">Node</span><span class="label">CPU</span><span class="label">Mem</span><span class="label">Disk</span><span class="label">Pods</span></div>
    ${rows.join("")}</div></div>`;
}

function diskPercent(disk) {
  return disk.capacity_bytes > 0 ? (disk.used_bytes / disk.capacity_bytes) * 100 : 0;
}

/** Watched pods scheduled on `node`, as "namespace/pod". */
function podsOnNode(cluster, node) {
  const metrics = metricsOf(cluster);
  const pods = [];
  for (const ns of metrics?.namespaces ?? []) {
    for (const pod of ns.pods ?? []) {
      if (pod.node === node) {
        pods.push(`${ns.name}/${pod.name}`);
      }
    }
  }
  return pods;
}

function nodeDetail(cluster, node) {
  const chips = [
    `<span class="chip ${node.ready ? "ok" : "bad"}">${node.ready ? "Ready" : "Not ready"}</span>`,
    ...node.problems.map((p) => `<span class="chip bad">${escapeHtml(p)}</span>`),
    node.unschedulable ? '<span class="chip">Cordoned</span>' : "",
    ...node.roles.map((r) => `<span class="chip">${escapeHtml(r)}</span>`),
    node.kubelet_version ? `<span class="chip">${escapeHtml(node.kubelet_version)}</span>` : "",
  ].join("");
  const u = node.usage;
  const bars = u
    ? `<div class="bars">${bar("CPU", u.cpu_percent)}${bar("MEM", u.memory_percent)}${bar("DISK", node.disk ? diskPercent(node.disk) : 0)}</div>`
    : '<p class="wait inline">No metrics sample for this node yet.</p>';
  const alloc = node.allocatable
    ? `${cores(node.allocatable.cpu_millicores)} · ${bytesShort(node.allocatable.memory_bytes)}`
    : "not reported";
  const usedLine = u ? `<dt>In use</dt><dd>${cores(u.cpu_millicores)} · ${bytesShort(u.memory_bytes)}</dd>` : "";
  const diskLine = node.disk ? `<dt>Disk</dt><dd>${capacity(node.disk.used_bytes, node.disk.capacity_bytes)}</dd>` : "";
  const pods = podsOnNode(cluster, node.name);
  const podList = pods.length > 0 ? pods.map(escapeHtml).join("<br>") : "none in watched namespaces";
  return `<div class="detail"><div class="chips">${chips}</div>${bars}
    <dl class="kv"><dt>Allocatable</dt><dd>${alloc}</dd>${usedLine}${diskLine}<dt>Your pods</dt><dd>${podList}</dd></dl></div>`;
}

function namespaceList(cluster, namespaces) {
  return `<div class="card"><div class="list lead">${nsColumns("Ready")}${namespaces.map((ns) => nsRow(cluster, ns, true)).join("")}</div></div>
    <button class="ghost" data-action="watch" data-cluster="${escapeHtml(cluster)}">+ Watch another namespace</button>`;
}

// ── Namespace ───────────────────────────────────

export function namespaceTop(nav) {
  return subTop({
    back: "cluster",
    title: nav.namespace,
    sub: `${displayName(nav.cluster)} · namespace`,
    action: moreButton("menu-namespace", `data-cluster="${escapeHtml(nav.cluster)}" data-namespace="${escapeHtml(nav.namespace)}"`),
  });
}

export function namespacePage(nav) {
  const metrics = metricsOf(nav.cluster);
  const ns = metrics?.namespaces.find((n) => n.name === nav.namespace);
  const error = errorOf(nav.cluster);
  if (error) {
    return `<div class="card"><p class="err big">${escapeHtml(error)}</p></div>`;
  }
  if (!ns) {
    return `<div class="card"><p class="wait">Waiting for the first poll…</p></div>`;
  }
  const t = nsTotals(ns);
  const restarting = (ns.pods ?? []).filter((p) => recentlyRestarted(podKey(nav.cluster, ns.name, p.name))).length;
  const summary = `<div class="card summary">
    <div><span class="label">Running</span><b>${t.running}</b></div>
    <div><span class="label">Restarts</span><b class="${restarting ? "warn" : ""}">${restarting}</b></div>
    <div><span class="label">CPU</span><b>${cores(t.mc)}</b></div>
    <div><span class="label">Mem</span><b>${bytesShort(t.mem)}</b></div>
  </div>`;
  const stale = ns.stale ? '<p class="note">Pods could not be listed this poll; showing the last list.</p>' : "";
  const groups = workloads(ns.pods);
  const body = groups.length > 0
    ? groups.map((group) => workloadCard(nav, group)).join("")
    : '<div class="card"><p class="wait">No pods to show in this namespace.</p></div>';
  const hidden = hiddenOf(nav.cluster, nav.namespace).length;
  const hiddenButton = hidden > 0
    ? `<button class="ghost" data-action="hidden" data-cluster="${escapeHtml(nav.cluster)}" data-namespace="${escapeHtml(nav.namespace)}">${hidden} hidden ${hidden === 1 ? "workload" : "workloads"}</button>`
    : "";
  return `${summary}${stale}${body}${hiddenButton}`;
}

function workloadCard(nav, group) {
  const running = group.pods.filter((p) => p.status === "Running").length;
  const short = group.kind === "CronJob" || group.kind === "Job";
  const worst = group.pods.reduce(
    (w, p) => (podLevel(nav.cluster, nav.namespace, p) === "ok" ? w : "warn"),
    "ok",
  );
  const ready = short ? `${running} running` : `${running}/${group.pods.length}`;
  const rows = group.pods.map((pod) => podRow(nav, group, pod)).join("");
  return `<div class="card"><div class="wl-top"><span class="kind" title="${escapeHtml(group.kind)}">${escapeHtml(kindBadge(group.kind))}</span>
    <b>${escapeHtml(group.name)}</b><span class="ready ${worst === "ok" ? "" : "warn"}">${ready}</span></div>
    <div class="list">${rows}</div></div>`;
}

/** A cron job's pods are named <cron job>-<run>-<suffix>; the run reads better. */
function podLabel(group, pod) {
  if (group.kind === "CronJob") {
    const parts = pod.name.split("-");
    if (parts.length >= 2) {
      return `run ${parts[parts.length - 2]}`;
    }
  }
  return pod.name;
}

function podRow(nav, group, pod) {
  const expanded = nav.open === pod.name;
  const lvl = pod.status === "Succeeded" ? "idle" : podLevel(nav.cluster, nav.namespace, pod);
  const restarts = pod.restart_count > 0 ? `<span class="tag ${recentlyRestarted(podKey(nav.cluster, nav.namespace, pod.name)) ? "warn" : ""}">${pod.restart_count}×</span>` : "";
  return `<div class="item"><button class="row pod-grid" data-action="toggle" data-key="${escapeHtml(pod.name)}" aria-expanded="${expanded}">
    ${dot(lvl)}<span class="name m" title="${escapeHtml(pod.name)}">${escapeHtml(middle(podLabel(group, pod), restarts ? 18 : 22))}${restarts}</span>
    <span class="age">${age(pod.start_time)}</span>
    <span class="r num">${pod.usage ? cores(pod.usage.cpu_millicores) : "–"}</span>
    <span class="r num">${pod.usage ? bytesShort(pod.usage.memory_bytes) : "–"}</span></button>
    ${expanded ? podDetail(nav, pod) : ""}</div>`;
}

function podDetail(nav, pod) {
  const u = pod.usage;
  const bars = u
    ? `<div class="bars">${bar("CPU", u.cpu_percent)}${bar("MEM", u.memory_percent)}${pod.pvc ? bar("PVC", diskPercent(pod.pvc)) : ""}</div>`
    : '<p class="wait inline">No metrics sample for this pod yet.</p>';
  const rows = [
    u ? `<dt>Usage</dt><dd>${cores(u.cpu_millicores)} · ${bytesShort(u.memory_bytes)}, of limits</dd>` : "",
    `<dt>Status</dt><dd>${escapeHtml(pod.status ?? "unknown")}${pod.restart_count ? ` · ${pod.restart_count} restarts` : ""}</dd>`,
    pod.node ? `<dt>Node</dt><dd>${escapeHtml(pod.node)}</dd>` : "<dt>Node</dt><dd>not scheduled</dd>",
    pod.pvc ? `<dt>Volume</dt><dd>${capacity(pod.pvc.used_bytes, pod.pvc.capacity_bytes)}</dd>` : "",
    pod.net ? `<dt>Network</dt><dd>↑ ${rate(pod.net.tx_bytes_per_sec)} · ↓ ${rate(pod.net.rx_bytes_per_sec)}</dd>` : "",
    `<dt>Pod</dt><dd>${escapeHtml(pod.name)}</dd>`,
  ].join("");
  const event = pod.last_event ? `<div class="event">● ${escapeHtml(pod.last_event)}</div>` : "";
  const ids = `data-cluster="${escapeHtml(nav.cluster)}" data-namespace="${escapeHtml(nav.namespace)}"`;
  return `<div class="detail">${bars}<dl class="kv">${rows}</dl>${event}
    <div class="actions">
      <button class="pill primary" data-action="logs" ${ids} data-pod="${escapeHtml(pod.name)}">Logs</button>
      <button class="pill" data-action="hide" ${ids} data-kind="${escapeHtml(pod.workload.kind)}" data-workload="${escapeHtml(pod.workload.name)}">Hide ${escapeHtml(pod.workload.kind.toLowerCase())}</button>
    </div></div>`;
}

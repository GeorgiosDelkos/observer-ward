// App state and everything derived from it: per-card history, restart
// tracking, health levels, and the "needs attention" list. No DOM here.

import { level, worse } from "./format.js";

const HISTORY_MAX = 20;
const ANOMALY_MIN_SAMPLES = 10;
const ANOMALY_SIGMA = 3;

/** A restart this recent puts the pod on the attention list. */
const RECENT_RESTART_MS = 60 * 60 * 1000;

/** Pod statuses that mean the pod is not doing its job. */
const FAILED_STATUSES = new Set([
  "CrashLoopBackOff",
  "ImagePullBackOff",
  "ErrImagePull",
  "OOMKilled",
  "Failed",
  "Unknown",
]);

export const state = {
  /** `servers` from the config, in config order. */
  servers: [],
  grafanaEnabled: false,
  /** Server name -> latest report, with namespaces filtered to the config. */
  reports: new Map(),
  /** Latest `alerts-update` payload, or null before the first one. */
  alerts: null,
  /** Card key -> { cpu, mem, disk } arrays of recent percentages. */
  history: new Map(),
  /** Pod key -> { count, raisedAt } where raisedAt is when it last rose. */
  restarts: new Map(),
  updatedAt: 0,
  syncing: false,
};

export function setConfig(config) {
  state.servers = config.servers ?? [];
  state.grafanaEnabled = Boolean(config.grafana?.enabled && config.grafana?.url);
  // Drop reports of servers no longer configured, and namespaces no
  // longer watched, right away rather than at the next poll.
  const reports = new Map();
  for (const [name, report] of state.reports) {
    const server = serverConfig(name);
    if (server) {
      reports.set(name, filtered(report, server));
    }
  }
  state.reports = reports;
}

export function serverConfig(name) {
  return state.servers.find((s) => s.name === name) ?? null;
}

export function clusters() {
  return state.servers.filter((s) => s.type === "k8s");
}

export function hosts() {
  return state.servers.filter((s) => s.type === "ssh");
}

/** Online metrics of server `name`, or null while offline or not yet polled. */
export function metricsOf(name) {
  const report = state.reports.get(name);
  return report?.status === "online" ? report.metrics : null;
}

export function errorOf(name) {
  const report = state.reports.get(name);
  return report?.status === "offline" ? report.error ?? "offline" : null;
}

export function hiddenOf(cluster, namespace) {
  const server = serverConfig(cluster);
  return server?.namespaces?.find((ns) => ns.name === namespace)?.hidden ?? [];
}

export function podKey(cluster, namespace, pod) {
  return `${cluster}/${namespace}/${pod}`;
}

// ── Ingest ──────────────────────────────────────

/** Take a `metrics-update` payload. */
export function ingestMetrics(update) {
  const previous = state.reports;
  const reports = new Map();
  const seen = new Set();
  const now = Date.now();

  for (const report of update.servers ?? []) {
    const server = serverConfig(report.name);
    if (!server) {
      continue;
    }
    const next = filtered(keepListedPods(report, previous.get(report.name)), server);
    reports.set(report.name, next);
    if (next.status !== "online") {
      continue;
    }
    seen.add(report.name);
    record(report.name, next.metrics);
    for (const ns of next.metrics.namespaces ?? []) {
      for (const pod of ns.pods ?? []) {
        const key = podKey(report.name, ns.name, pod.name);
        seen.add(key);
        record(key, pod.usage);
        trackRestarts(key, pod.restart_count, now);
      }
    }
  }

  state.reports = reports;
  // An offline server keeps its history for when it returns; anything else
  // not reported this poll is gone.
  for (const key of state.history.keys()) {
    if (!seen.has(key) && reports.get(ownerOf(key))?.status !== "offline") {
      state.history.delete(key);
    }
  }
  for (const key of state.restarts.keys()) {
    if (!seen.has(key)) {
      state.restarts.delete(key);
    }
  }
  state.updatedAt = now;
  state.syncing = false;
}

export function ingestAlerts(update) {
  state.alerts = update;
}

/** A namespace whose pods could not be listed keeps its last pods. */
function keepListedPods(report, before) {
  if (report.kind !== "k8s" || report.status !== "online" || before?.status !== "online") {
    return report;
  }
  const namespaces = report.metrics.namespaces.map((ns) => {
    if (Array.isArray(ns.pods)) {
      return ns;
    }
    const last = before.metrics.namespaces.find((b) => b.name === ns.name);
    return { ...ns, pods: last?.pods ?? null, stale: true };
  });
  return { ...report, metrics: { ...report.metrics, namespaces } };
}

/** Keep only watched namespaces and pods of workloads not hidden: the
 * backend filters too, this covers a poll already running during an edit. */
function filtered(report, server) {
  if (report.status !== "online" || server.type !== "k8s") {
    return report;
  }
  const namespaces = [];
  for (const watched of server.namespaces) {
    const ns = report.metrics.namespaces?.find((n) => n.name === watched.name);
    if (!ns) {
      continue;
    }
    const hidden = watched.hidden ?? [];
    const pods = ns.pods?.filter(
      (p) => !hidden.some((w) => w.kind === p.workload.kind && w.name === p.workload.name),
    );
    namespaces.push({ ...ns, pods: pods ?? null });
  }
  return { ...report, metrics: { ...report.metrics, namespaces } };
}

function record(key, usage) {
  if (!usage) {
    return;
  }
  const h = state.history.get(key) ?? { cpu: [], mem: [], disk: [] };
  push(h.cpu, usage.cpu_percent);
  push(h.mem, usage.memory_percent);
  push(h.disk, usage.disk_percent);
  state.history.set(key, h);
}

function push(series, value) {
  series.push(Number.isFinite(value) ? value : 0);
  if (series.length > HISTORY_MAX) {
    series.shift();
  }
}

function trackRestarts(key, count, now) {
  const before = state.restarts.get(key);
  const raisedAt = before && count > before.count ? now : before?.raisedAt ?? 0;
  state.restarts.set(key, { count, raisedAt });
}

/** The server a history key belongs to. Server names may hold "/" only in
 * legacy files, so a configured name is matched first. */
function ownerOf(key) {
  if (serverConfig(key)) {
    return key;
  }
  const parts = key.split("/");
  return parts.slice(0, -2).join("/");
}

/** Whether the newest sample is more than 3σ from the window's mean. */
export function isAnomaly(series) {
  if (!series || series.length < ANOMALY_MIN_SAMPLES) {
    return false;
  }
  const mean = series.reduce((a, b) => a + b, 0) / series.length;
  const variance = series.reduce((a, v) => a + (v - mean) ** 2, 0) / (series.length - 1);
  const latest = series[series.length - 1];
  return Math.abs(latest - mean) > ANOMALY_SIGMA * Math.sqrt(variance);
}

// ── Health ──────────────────────────────────────

function usageLevel(usage) {
  if (!usage) {
    return "ok";
  }
  return level(Math.max(usage.cpu_percent, usage.memory_percent, usage.disk_percent));
}

export function nodeLevel(node) {
  if (!node.ready || node.problems.length > 0) {
    return "crit";
  }
  return usageLevel(node.usage);
}

export function recentlyRestarted(key) {
  const entry = state.restarts.get(key);
  return Boolean(entry?.raisedAt && Date.now() - entry.raisedAt < RECENT_RESTART_MS);
}

export function podLevel(cluster, namespace, pod) {
  if (FAILED_STATUSES.has(pod.status)) {
    return "crit";
  }
  let result = level(Math.max(pod.usage?.cpu_percent ?? 0, pod.usage?.memory_percent ?? 0));
  if (pod.status === "Pending" || recentlyRestarted(podKey(cluster, namespace, pod.name))) {
    result = worse(result, "warn");
  }
  return result;
}

export function namespaceLevel(cluster, ns) {
  return (ns.pods ?? []).reduce(
    (worst, pod) => worse(worst, podLevel(cluster, ns.name, pod)),
    "ok",
  );
}

/** A cluster's health: offline, its totals, nodes, and watched pods. */
export function clusterLevel(name) {
  if (errorOf(name)) {
    return "crit";
  }
  const metrics = metricsOf(name);
  if (!metrics) {
    return "idle";
  }
  // Cluster totals are flattened into the metrics object, like a host's.
  let result = usageLevel(metrics);
  for (const node of metrics.nodes) {
    result = worse(result, nodeLevel(node));
  }
  for (const ns of metrics.namespaces) {
    result = worse(result, namespaceLevel(name, ns));
  }
  return result;
}

export function hostLevel(name) {
  if (errorOf(name)) {
    return "crit";
  }
  const metrics = metricsOf(name);
  return metrics ? usageLevel(metrics) : "idle";
}

/** Pods grouped by owning workload, workloads A-Z, pods A-Z. */
export function workloads(pods) {
  const groups = new Map();
  for (const pod of pods ?? []) {
    const key = `${pod.workload.kind}/${pod.workload.name}`;
    if (!groups.has(key)) {
      groups.set(key, { kind: pod.workload.kind, name: pod.workload.name, pods: [] });
    }
    groups.get(key).pods.push(pod);
  }
  const byName = (a, b) => a.name.localeCompare(b.name, undefined, { numeric: true });
  const list = [...groups.values()].sort(byName);
  for (const group of list) {
    group.pods.sort(byName);
  }
  return list;
}

// ── Attention ───────────────────────────────────

/**
 * Everything that needs a look, worst first: offline sources, unhealthy
 * nodes, failing or restarting pods, anything at a critical threshold,
 * and firing Grafana alerts. Each item says where to go to see it.
 */
export function attention() {
  const items = [];
  for (const server of state.servers) {
    const error = errorOf(server.name);
    if (error) {
      items.push({
        level: "crit",
        icon: "!",
        title: `${server.name} is offline`,
        detail: error,
        go: server.type === "k8s" ? { page: "cluster", cluster: server.name } : null,
      });
      continue;
    }
    if (server.type === "k8s") {
      items.push(...clusterAttention(server.name));
    } else {
      items.push(...hostAttention(server.name));
    }
  }
  items.push(...alertAttention());
  const rank = { crit: 0, warn: 1 };
  return items.sort((a, b) => rank[a.level] - rank[b.level]);
}

function clusterAttention(cluster) {
  const metrics = metricsOf(cluster);
  if (!metrics) {
    return [];
  }
  const items = [];
  for (const node of metrics.nodes) {
    const go = { page: "cluster", cluster, tab: "nodes", open: node.name };
    if (!node.ready) {
      items.push({ level: "crit", icon: "!", title: `${node.name} is not ready`, detail: cluster, go });
    } else if (node.problems.length > 0) {
      const problems = node.problems.join(", ");
      items.push({ level: "crit", icon: "!", title: `${node.name}: ${problems}`, detail: cluster, go });
    } else if (usageLevel(node.usage) === "crit") {
      items.push({ level: "crit", icon: "▲", title: `${node.name} is near capacity`, detail: `${cluster} · ${hottest(node.usage)}`, go });
    }
  }
  for (const ns of metrics.namespaces) {
    for (const pod of ns.pods ?? []) {
      const item = podAttention(cluster, ns.name, pod);
      if (item) {
        items.push(item);
      }
    }
  }
  return items;
}

function podAttention(cluster, namespace, pod) {
  const go = { page: "namespace", cluster, namespace, open: pod.name };
  const where = `${cluster} / ${namespace}`;
  if (FAILED_STATUSES.has(pod.status)) {
    return { level: "crit", icon: "!", title: `${pod.name}: ${pod.status}`, detail: where, go };
  }
  if (recentlyRestarted(podKey(cluster, namespace, pod.name))) {
    const times = pod.restart_count === 1 ? "once" : `${pod.restart_count} times`;
    const event = pod.last_event ? ` · ${pod.last_event}` : "";
    return { level: "warn", icon: "↻", title: `${pod.name} restarted ${times}`, detail: `${where}${event}`, go };
  }
  if (level(Math.max(pod.usage?.cpu_percent ?? 0, pod.usage?.memory_percent ?? 0)) === "crit") {
    return { level: "crit", icon: "▲", title: `${pod.name} is near its limits`, detail: `${where} · ${hottest(pod.usage)}`, go };
  }
  return null;
}

function hostAttention(name) {
  const metrics = metricsOf(name);
  if (!metrics || usageLevel(metrics) !== "crit") {
    return [];
  }
  return [{ level: "crit", icon: "▲", title: `${name} is near capacity`, detail: hottest(metrics), go: null }];
}

function alertAttention() {
  if (!state.grafanaEnabled || !state.alerts) {
    return [];
  }
  if (state.alerts.source_error) {
    return [{ level: "warn", icon: "◌", title: "Grafana is unreachable", detail: state.alerts.source_error, go: null }];
  }
  return (state.alerts.alerts ?? [])
    .filter((a) => a.state === "active" && (a.severity === "critical" || a.severity === "warning"))
    .map((a) => ({
      level: a.severity === "critical" ? "crit" : "warn",
      icon: "◆",
      title: a.name,
      detail: a.summary || "Grafana alert",
      url: a.generator_url ?? null,
      go: null,
    }));
}

function hottest(usage) {
  const metrics = [
    ["CPU", usage.cpu_percent],
    ["memory", usage.memory_percent],
    ["disk", usage.disk_percent ?? 0],
  ];
  const [name, value] = metrics.reduce((a, b) => (b[1] > a[1] ? b : a));
  return `${name} ${Math.round(value)}%`;
}

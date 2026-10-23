import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";

import {
  attention,
  clusterLevel,
  ingestMetrics,
  metricsOf,
  recentlyRestarted,
  setConfig,
  state,
  workloads,
} from "../../ui/js/model.js";

function pod(name, extra = {}) {
  return {
    name,
    workload: { kind: "Deployment", name: name.split("-")[0] },
    status: "Running",
    restart_count: 0,
    start_time: null,
    last_event: null,
    node: "n1",
    usage: { cpu_percent: 10, memory_percent: 10, disk_percent: 0, cpu_millicores: 100, memory_bytes: 1 },
    net: null,
    pvc: null,
    ...extra,
  };
}

function node(name, extra = {}) {
  return {
    name,
    roles: [],
    ready: true,
    unschedulable: false,
    problems: [],
    kubelet_version: null,
    allocatable: { cpu_millicores: 4000, memory_bytes: 8 },
    usage: { cpu_percent: 10, memory_percent: 10, disk_percent: 10, cpu_millicores: 400, memory_bytes: 1 },
    disk: null,
    ...extra,
  };
}

function cluster({ totals = 10, nodes = [node("n1")], namespaces }) {
  return {
    kind: "k8s",
    name: "prod",
    status: "online",
    metrics: {
      cpu_percent: totals,
      memory_percent: totals,
      disk_percent: totals,
      cpu_millicores: 0,
      memory_bytes: 0,
      net: null,
      disk: { used_bytes: 0, capacity_bytes: 0 },
      requested: null,
      nodes,
      namespaces,
    },
  };
}

const host = { kind: "ssh", name: "bastion", status: "online", metrics: { cpu_percent: 5, memory_percent: 5, disk_percent: 5, net: null } };

beforeEach(() => {
  state.reports = new Map();
  state.history.clear();
  state.restarts.clear();
  setConfig({
    servers: [
      {
        type: "k8s",
        name: "prod",
        context: "prod",
        kubeconfig: null,
        namespaces: [{ name: "default", hidden: [{ kind: "Deployment", name: "secret" }] }, { name: "s3" }],
      },
      { type: "ssh", name: "bastion", host: "h", port: 22, user: "u", key_path: "/k" },
    ],
    grafana: null,
  });
});

test("an SSH host survives a second update", () => {
  ingestMetrics({ servers: [host] });
  ingestMetrics({ servers: [host] });

  assert.equal(metricsOf("bastion").cpu_percent, 5);
});

test("a namespace that failed to list keeps its last pods, marked stale", () => {
  const listed = [{ name: "default", pods: [pod("api-1")] }, { name: "s3", pods: [pod("minio-0")] }];
  ingestMetrics({ servers: [cluster({ namespaces: listed })] });

  const failed = [{ name: "default", pods: null }, { name: "s3", pods: [] }];
  ingestMetrics({ servers: [cluster({ namespaces: failed })] });

  const [ns, s3] = metricsOf("prod").namespaces;
  assert.equal(ns.stale, true);
  assert.deepEqual(ns.pods.map((p) => p.name), ["api-1"]);
  assert.deepEqual(s3.pods, [], "a listed empty namespace is authoritative");
});

test("unwatched namespaces and hidden workloads are dropped", () => {
  const namespaces = [
    { name: "default", pods: [pod("api-1"), pod("secret-1")] },
    { name: "kube-system", pods: [pod("dns-1")] },
  ];
  ingestMetrics({ servers: [cluster({ namespaces })] });

  const shown = metricsOf("prod").namespaces;
  assert.deepEqual(shown.map((n) => n.name), ["default"]);
  assert.deepEqual(shown[0].pods.map((p) => p.name), ["api-1"]);
});

test("only a rising restart count flags a pod", () => {
  const at = (count) => [{ name: "s3", pods: [pod("gw-1", { restart_count: count })] }];
  ingestMetrics({ servers: [cluster({ namespaces: at(5) })] });
  assert.equal(recentlyRestarted("prod/s3/gw-1"), false, "lifetime restarts are history");

  ingestMetrics({ servers: [cluster({ namespaces: at(6) })] });
  assert.equal(recentlyRestarted("prod/s3/gw-1"), true);

  const titles = attention().map((i) => i.title);
  assert.ok(titles.includes("gw-1 restarted 6 times"), titles.join(" | "));
});

test("cluster health reads the flattened totals, nodes and pods", () => {
  ingestMetrics({ servers: [cluster({ totals: 90, namespaces: [] })] });
  assert.equal(clusterLevel("prod"), "crit", "totals at 90%");

  ingestMetrics({ servers: [cluster({ nodes: [node("n1", { problems: ["DiskPressure"] })], namespaces: [] })] });
  assert.equal(clusterLevel("prod"), "crit", "a pressured node");

  ingestMetrics({ servers: [cluster({ namespaces: [{ name: "s3", pods: [pod("gw-1", { status: "Pending" })] }] })] });
  assert.equal(clusterLevel("prod"), "warn", "a pending pod");
});

test("attention lists critical items before warnings, each with a destination", () => {
  const namespaces = [{ name: "s3", pods: [pod("gw-1", { status: "Pending" }), pod("db-0", { status: "CrashLoopBackOff" })] }];
  const nodes = [node("n1", { ready: false })];
  ingestMetrics({ servers: [cluster({ nodes, namespaces }), { kind: "ssh", name: "bastion", status: "offline", error: "refused" }] });

  const items = attention();
  assert.deepEqual(items.map((i) => i.level), ["crit", "crit", "crit"]);
  assert.deepEqual(items.map((i) => i.title), ["n1 is not ready", "db-0: CrashLoopBackOff", "bastion is offline"]);
  assert.deepEqual(items[0].go, { page: "cluster", cluster: "prod", tab: "nodes", open: "n1" });
  assert.deepEqual(items[1].go, { page: "namespace", cluster: "prod", namespace: "s3", open: "db-0" });
});

test("pods group by workload, both sorted naturally", () => {
  const groups = workloads([pod("web-10"), pod("api-2"), pod("web-9"), pod("api-10")]);

  assert.deepEqual(groups.map((g) => g.name), ["api", "web"]);
  assert.deepEqual(groups[1].pods.map((p) => p.name), ["web-9", "web-10"]);
});

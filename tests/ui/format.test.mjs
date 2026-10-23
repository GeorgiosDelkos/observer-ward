import assert from "node:assert/strict";
import { test } from "node:test";

import {
  bytesShort,
  capacity,
  cores,
  kindBadge,
  level,
  middle,
  ofCeiling,
  percentText,
  worse,
} from "../../ui/js/format.js";

const GiB = 1024 ** 3;
const TiB = 1024 ** 4;

test("levels follow the backend thresholds", () => {
  assert.equal(level(59.9), "ok");
  assert.equal(level(60), "warn");
  assert.equal(level(84.9), "warn");
  assert.equal(level(85), "crit");
  assert.equal(worse("ok", "warn"), "warn");
  assert.equal(worse("crit", "warn"), "crit");
});

test("capacity shares a unit when it can, and names both when not", () => {
  assert.equal(capacity(4.5 * TiB, 7.2 * TiB), "4.5 / 7.2 TiB");
  assert.equal(capacity(651 * GiB, 1.8 * TiB), "651 GiB / 1.8 TiB");
  assert.equal(capacity(61 * GiB, 200 * GiB), "61.0 / 200 GiB");
});

test("middle keeps the distinguishing end of a long name", () => {
  assert.equal(middle("k8s-v3-node1", 19), "k8s-v3-node1");
  const cut = middle("hippius-k8s-v3-master-1", 19);
  assert.equal(cut.length, 19);
  assert.ok(cut.endsWith("-master-1"), cut);
  assert.ok(cut.includes("…"));
});

test("small values read as such", () => {
  assert.equal(percentText(0.4), "<1");
  assert.equal(percentText(0), "0");
  assert.equal(percentText(140), "100");
  assert.equal(cores(250), "250m");
  assert.equal(cores(1400), "1.4c");
});

test("workload kinds get short badges", () => {
  assert.equal(kindBadge("StatefulSet"), "STS");
  assert.equal(kindBadge("Rollout"), "RO");
  assert.equal(kindBadge("ScaledJob"), "SCA");
});

test("a pod's usage reads against its ceiling and where it comes from", () => {
  assert.equal(ofCeiling(1600, { amount: 16000, source: "node" }, cores), "1.6c of 16.0c · node");
  assert.equal(ofCeiling(2 * GiB, { amount: 8 * GiB, source: "limit" }, bytesShort), "2.0G of 8.0G · limit");
});

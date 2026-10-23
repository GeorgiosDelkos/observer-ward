// Formatting helpers shared by every view. Pure functions of their input.

const KIB = 1024;
const GIB = KIB ** 3;
const TIB = KIB ** 4;

/** Thresholds shared with the backend (`classify_level`): green < 60, amber < 85. */
export function level(percent) {
  if (percent >= 85) {
    return "crit";
  }
  if (percent >= 60) {
    return "warn";
  }
  return "ok";
}

const LEVEL_RANK = { ok: 0, warn: 1, crit: 2 };

/** The more severe of two levels. */
export function worse(a, b) {
  return LEVEL_RANK[a] >= LEVEL_RANK[b] ? a : b;
}

export function escapeHtml(value) {
  return String(value)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

/** Whole percent, with "<1" for a small non-zero share. */
export function percentText(percent) {
  const clamped = Math.max(0, Math.min(100, percent || 0));
  if (clamped > 0 && clamped < 1) {
    return "<1";
  }
  return String(Math.round(clamped));
}

/** "250m" below one core, "1.4c" from one core. */
export function cores(millicores) {
  if (millicores >= 1000) {
    return `${(millicores / 1000).toFixed(1)}c`;
  }
  return `${Math.round(millicores)}m`;
}

/** Bytes as the largest unit that keeps a short number: "512M", "1.4G", "4.5T". */
export function bytesShort(bytes) {
  if (!bytes || bytes <= 0) {
    return "0";
  }
  if (bytes >= TIB) {
    return `${(bytes / TIB).toFixed(1)}T`;
  }
  if (bytes >= GIB) {
    return `${(bytes / GIB).toFixed(1)}G`;
  }
  return `${Math.round(bytes / KIB ** 2)}M`;
}

/** "4.5 / 7.2 TiB" when both sides share a unit, else "651 GiB / 1.8 TiB". */
export function capacity(used, total) {
  const [u, uUnit] = sized(used);
  const [t, tUnit] = sized(total);
  return uUnit === tUnit ? `${u} / ${t} ${tUnit}` : `${u} ${uUnit} / ${t} ${tUnit}`;
}

/** A byte count in TiB from 1 TiB, else GiB; whole above 100. */
function sized(bytes) {
  const [value, unit] = bytes >= TIB ? [bytes / TIB, "TiB"] : [bytes / GIB, "GiB"];
  return [value >= 100 ? Math.round(value).toString() : value.toFixed(1), unit];
}

export function cpuCapacity(usedMillicores, totalMillicores) {
  const used = usedMillicores / 1000;
  const total = Math.round(totalMillicores / 1000);
  return `${used.toFixed(1)} / ${total} cores`;
}

const RATE_UNITS = ["B/s", "KB/s", "MB/s", "GB/s"];

export function rate(bytesPerSec) {
  let value = bytesPerSec || 0;
  let unit = 0;
  while (value >= KIB && unit < RATE_UNITS.length - 1) {
    value /= KIB;
    unit += 1;
  }
  const digits = unit === 0 ? 0 : 1;
  return `${value.toFixed(digits)} ${RATE_UNITS[unit]}`;
}

/** Compact age of an RFC 3339 timestamp: "42s", "6m", "4h", "21d". */
export function age(timestamp) {
  const start = Date.parse(timestamp ?? "");
  if (Number.isNaN(start)) {
    return "";
  }
  const seconds = Math.max(0, Math.floor((Date.now() - start) / 1000));
  if (seconds < 60) {
    return `${seconds}s`;
  }
  if (seconds < 3600) {
    return `${Math.floor(seconds / 60)}m`;
  }
  if (seconds < 86400) {
    return `${Math.floor(seconds / 3600)}h`;
  }
  return `${Math.floor(seconds / 86400)}d`;
}

const KIND_BADGES = new Map([
  ["Deployment", "DEP"],
  ["StatefulSet", "STS"],
  ["DaemonSet", "DS"],
  ["CronJob", "CJ"],
  ["Job", "JOB"],
  ["ReplicaSet", "RS"],
  ["Rollout", "RO"],
  ["Pod", "POD"],
]);

/** Short badge for a workload kind; custom resources get their first letters. */
export function kindBadge(kind) {
  return KIND_BADGES.get(kind) ?? kind.slice(0, 3).toUpperCase();
}

/**
 * `text` cut to `max` characters in the middle. Kubernetes names share
 * prefixes and differ at the end (`…-master-1`, `…-7d9f6-lk4tq`), so the
 * end is what must stay visible.
 */
export function middle(text, max) {
  if (text.length <= max) {
    return text;
  }
  const tail = Math.ceil((max - 1) / 2);
  return `${text.slice(0, max - 1 - tail)}…${text.slice(-tail)}`;
}

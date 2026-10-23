// Entry point: navigation, the click/keyboard dispatch, menus and sheets,
// window sizing, and the Tauri events that feed the model.

import { escapeHtml, percentText } from "./js/format.js";
import { mountAdd, mountSettings } from "./js/forms.js";
import {
  attention,
  errorOf,
  hiddenOf,
  ingestAlerts,
  ingestMetrics,
  metricsOf,
  serverConfig,
  setConfig,
  state,
} from "./js/model.js";
import {
  bottom,
  clusterPage,
  clusterTop,
  displayName,
  homePage,
  homeTop,
  namespacePage,
  namespaceTop,
  saveAlias,
  subTop,
  updateLive,
} from "./js/view.js";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const WIN_WIDTH = 400;
const MIN_HEIGHT = 160;
const MAX_HEIGHT = 720;
/** Room around a sheet taller than the page it covers. */
const SHEET_MARGIN = 24;

const els = {
  win: document.getElementById("win"),
  top: document.getElementById("top"),
  page: document.getElementById("page"),
  content: document.getElementById("content"),
  bottom: document.getElementById("bottom"),
  menu: document.getElementById("menu"),
  overlay: document.getElementById("overlay"),
  sheet: document.getElementById("sheet"),
};

/**
 * Where the popover is. `open` is the expanded node or pod row; `back` is
 * the page the add and settings pages return to.
 */
const nav = {
  page: "home",
  cluster: null,
  namespace: null,
  tab: "nodes",
  open: null,
  allIssues: false,
  back: null,
};

const DEPTH = { home: 0, cluster: 1, settings: 1, add: 2, namespace: 2 };
const FORM_PAGES = new Set(["add", "settings"]);

// ── Rendering ───────────────────────────────────

/**
 * Re-render the data page and the chrome. Forms keep their DOM, and so
 * does a page with a rename in progress: the rename re-renders when it
 * ends. Focus and scroll survive a refresh; navigation focuses the new
 * page's first control.
 */
function render({ direction = null } = {}) {
  validateNav();
  const issues = attention();
  const focus = direction ? null : focusKey(document.activeElement);
  const editing = Boolean(els.win.querySelector("input.rename"));
  if (!FORM_PAGES.has(nav.page) && !editing) {
    const scroll = direction ? 0 : els.page.scrollTop;
    els.top.innerHTML = topHtml();
    els.content.innerHTML = pageHtml(issues);
    els.page.scrollTop = scroll;
  }
  els.bottom.innerHTML = bottom(issues);
  els.win.classList.toggle("troubled", issues.some((i) => i.level === "crit"));
  if (direction) {
    animate(direction);
    if (!FORM_PAGES.has(nav.page)) {
      els.top.querySelector("button")?.focus({ preventScroll: true });
    }
  } else {
    restoreFocus(focus);
  }
  resize();
  // A row opened by navigation (an attention item, a node cell) may sit
  // below the fold; bring it into view once the window has its height.
  if (direction && nav.open) {
    const item = els.content.querySelector(`[data-key="${CSS.escape(nav.open)}"]`)?.closest(".item");
    requestAnimationFrame(() => scrollPageTo(item));
  }
}

/** Attributes that, with `data-action`, identify a control across renders. */
const FOCUS_IDS = ["key", "cluster", "namespace", "tab", "host"];

/** A selector for the focused control, or null when focus is elsewhere. */
function focusKey(el) {
  const control = el?.closest?.("[data-action]");
  if (!control || !els.win.contains(control)) {
    return null;
  }
  const ids = FOCUS_IDS.filter((id) => control.dataset[id] !== undefined)
    .map((id) => `[data-${id}="${CSS.escape(control.dataset[id])}"]`)
    .join("");
  return `[data-action="${CSS.escape(control.dataset.action)}"]${ids}`;
}

function restoreFocus(selector) {
  if (selector && !els.win.contains(document.activeElement)) {
    els.win.querySelector(selector)?.focus({ preventScroll: true });
  }
}

/** Scroll only the page (never the window shell) so `el` sits near its top. */
function scrollPageTo(el) {
  if (!el) {
    return;
  }
  const page = els.page.getBoundingClientRect();
  const target = el.getBoundingClientRect();
  els.page.scrollTop += target.top - page.top - 8;
}

function topHtml() {
  switch (nav.page) {
    case "cluster":
      return clusterTop(nav);
    case "namespace":
      return namespaceTop(nav);
    default:
      return homeTop();
  }
}

function pageHtml(issues) {
  switch (nav.page) {
    case "cluster":
      return clusterPage(nav);
    case "namespace":
      return namespacePage(nav);
    default:
      return homePage(nav, issues);
  }
}

/** A page whose cluster or namespace went away falls back a level. */
function validateNav() {
  if (nav.cluster && !serverConfig(nav.cluster) && nav.page !== "settings") {
    Object.assign(nav, { page: "home", cluster: null, namespace: null, open: null });
  }
  if (nav.page === "namespace") {
    const watched = serverConfig(nav.cluster)?.namespaces?.some((ns) => ns.name === nav.namespace);
    if (!watched) {
      Object.assign(nav, { page: "cluster", namespace: null, open: null });
    }
  }
}

function animate(direction) {
  els.page.classList.remove("enter", "back");
  // Force a reflow so the same animation can run again.
  void els.page.offsetWidth;
  els.page.classList.add(direction);
}

/** Fit the window to its content, between MIN_HEIGHT and MAX_HEIGHT. */
function resize() {
  const chrome = els.top.offsetHeight + els.bottom.offsetHeight;
  const pagePad = parseFloat(getComputedStyle(els.page).paddingBottom) || 0;
  let height = chrome + els.content.offsetHeight + pagePad;
  if (!els.overlay.hidden) {
    // scrollHeight: the sheet's own height is capped by the window it
    // is trying to grow.
    height = Math.max(height, els.sheet.scrollHeight + 2 * SHEET_MARGIN);
  }
  const fitted = Math.round(Math.min(MAX_HEIGHT, Math.max(MIN_HEIGHT, height)));
  invoke("resize_window", { width: WIN_WIDTH, height: fitted }).catch((err) => {
    console.warn("resize_window failed:", err);
  });
}

// ── Navigation ──────────────────────────────────

function go(next) {
  const from = nav.page;
  Object.assign(nav, { open: null, allIssues: false }, next);
  closeMenu();
  const direction = DEPTH[nav.page] >= DEPTH[from] ? "enter" : "back";
  if (nav.page === "add") {
    els.top.innerHTML = subTop({
      title: nav.cluster ? "Watch a namespace" : "Add a source",
      sub: nav.cluster ? displayName(nav.cluster) : "Kubernetes cluster or SSH host",
    });
    mountAdd(els.content, {
      cluster: nav.cluster,
      onAdded: (config) => {
        setConfig(config);
        goBack();
      },
      onResize: resize,
    });
  } else if (nav.page === "settings") {
    els.top.innerHTML = subTop({ title: "Settings", sub: "Observer Ward" });
    mountSettings(els.content, {
      onSaved: (config) => {
        setConfig(config);
        goBack();
      },
      onResize: resize,
    });
  }
  render({ direction });
}

function goBack() {
  switch (nav.page) {
    case "namespace":
      go({ page: "cluster", namespace: null });
      break;
    case "add":
    case "settings":
      go({ page: nav.back ?? "home" });
      break;
    default:
      go({ page: "home", cluster: null });
  }
}

// ── Actions ─────────────────────────────────────

const actions = {
  back: () => goBack(),
  add: () => go({ page: "add", cluster: null, back: nav.page === "home" ? "home" : nav.page }),
  watch: (el) => go({ page: "add", cluster: el.dataset.cluster, back: "cluster" }),
  settings: () => go({ page: "settings", back: FORM_PAGES.has(nav.page) ? "home" : nav.page }),
  quit: () => invoke("quit_app").catch((err) => console.error("quit failed:", err)),
  cluster: (el) => go({ page: "cluster", cluster: el.dataset.cluster, tab: "nodes" }),
  namespace: (el) => go({ page: "namespace", cluster: el.dataset.cluster, namespace: el.dataset.namespace }),
  go: (el) => go(JSON.parse(el.dataset.go)),
  tab: (el) => {
    Object.assign(nav, { tab: el.dataset.tab, open: null });
    render();
  },
  toggle: (el) => {
    nav.open = nav.open === el.dataset.key ? null : el.dataset.key;
    render();
  },
  "all-issues": () => {
    nav.allIssues = true;
    render();
  },
  "open-url": (el) => invoke("open_url", { url: el.dataset.url }).catch(report),
  copy: (el) => copy(el.dataset.text),
  logs: (el) => invoke("open_pod_logs", {
    server: el.dataset.cluster,
    namespace: el.dataset.namespace,
    pod: el.dataset.pod,
  }).catch(report),
  hide: (el) => hideWorkload(el.dataset.cluster, el.dataset.namespace, {
    kind: el.dataset.kind,
    name: el.dataset.workload,
  }),
  hidden: (el) => openHiddenSheet(el.dataset.cluster, el.dataset.namespace),
  "menu-cluster": (el) => openMenu(el, clusterMenu(el.dataset.cluster)),
  "menu-namespace": (el) => openMenu(el, namespaceMenu(el.dataset.cluster, el.dataset.namespace)),
  "menu-host": (el) => openMenu(el, hostMenu(el.dataset.host)),
};

els.win.addEventListener("click", (e) => {
  const item = e.target.closest("[data-menu-item]");
  if (item) {
    runMenuItem(Number(item.dataset.menuItem));
    return;
  }
  if (!els.menu.hidden && !e.target.closest("#menu")) {
    closeMenu();
  }
  const el = e.target.closest("[data-action]");
  const action = el && actions[el.dataset.action];
  if (action && !el.closest("#overlay")) {
    e.preventDefault();
    action(el);
  }
});

function report(err) {
  console.error(err);
  flash(String(err));
}

/** A short-lived message at the bottom of the window. */
function flash(message) {
  const toast = document.createElement("div");
  toast.className = "toast";
  toast.setAttribute("role", "status");
  toast.textContent = message;
  els.win.append(toast);
  setTimeout(() => toast.remove(), 3200);
}

async function copy(text) {
  try {
    await invoke("copy_to_clipboard", { text });
    flash("Copied");
  } catch (err) {
    report(err);
  }
}

const shellQuote = (s) => `'${s.replaceAll("'", "'\\''")}'`;

function kubectlFor(cluster, namespace = null) {
  const server = serverConfig(cluster);
  let cmd = `kubectl --context ${shellQuote(server.context)}`;
  if (namespace) {
    cmd += ` -n ${shellQuote(namespace)}`;
  }
  if (server.kubeconfig) {
    cmd += ` --kubeconfig ${shellQuote(server.kubeconfig)}`;
  }
  return cmd;
}

function metricsSummary(name) {
  const m = metricsOf(name);
  if (!m) {
    return name;
  }
  return [
    name,
    `  CPU: ${percentText(m.cpu_percent)}%`,
    `  MEM: ${percentText(m.memory_percent)}%`,
    `  DISK: ${percentText(m.disk_percent)}%`,
  ].join("\n");
}

async function hideWorkload(cluster, namespace, workload) {
  try {
    const config = await invoke("hide_workload", { server: cluster, namespace, workload });
    setConfig(config);
    nav.open = null;
    render();
    flash(`Hid ${workload.kind.toLowerCase()} ${workload.name}`);
  } catch (err) {
    report(err);
  }
}

// ── Menus ───────────────────────────────────────

let menuItems = [];
/** The control that opened the menu, to take focus back when it closes. */
let menuReturn = null;

function clusterMenu(cluster) {
  const error = errorOf(cluster);
  return [
    { label: "Copy kubectl context", run: () => copy(kubectlFor(cluster)) },
    { label: "Copy metrics", run: () => copy(metricsSummary(cluster)), hidden: Boolean(error) },
    { label: "Copy error", run: () => copy(error), hidden: !error },
    { label: "Watch another namespace", run: () => go({ page: "add", cluster, back: "cluster" }) },
    { label: "Remove cluster…", danger: true, run: () => confirmRemove(cluster, "cluster") },
  ];
}

function namespaceMenu(cluster, namespace) {
  const last = (serverConfig(cluster)?.namespaces?.length ?? 0) < 2;
  return [
    { label: "Copy kubectl command", run: () => copy(kubectlFor(cluster, namespace)) },
    {
      label: "Stop watching…",
      danger: true,
      hidden: last,
      run: () => confirmUnwatch(cluster, namespace),
    },
  ];
}

function hostMenu(host) {
  const error = errorOf(host);
  return [
    { label: "Open terminal", run: () => invoke("open_ssh_terminal", { name: host }).catch(report) },
    { label: "Copy metrics", run: () => copy(metricsSummary(host)), hidden: Boolean(error) },
    { label: "Copy error", run: () => copy(error), hidden: !error },
    { label: "Remove host…", danger: true, run: () => confirmRemove(host, "host") },
  ];
}

function openMenu(anchor, items) {
  menuItems = items.filter((item) => !item.hidden);
  els.menu.innerHTML = menuItems
    .map((item, i) => `<button class="menu-item${item.danger ? " danger" : ""}" data-menu-item="${i}" role="menuitem">${escapeHtml(item.label)}</button>`)
    .join("");
  els.menu.hidden = false;
  const win = els.win.getBoundingClientRect();
  const at = anchor.getBoundingClientRect();
  els.menu.style.top = `${at.bottom - win.top + 6}px`;
  els.menu.style.right = `${win.right - at.right}px`;
  menuReturn = focusKey(anchor);
  els.menu.querySelector("button")?.focus();
}

function closeMenu() {
  if (els.menu.hidden) {
    return;
  }
  const hadFocus = els.menu.contains(document.activeElement);
  els.menu.hidden = true;
  menuItems = [];
  if (hadFocus && menuReturn) {
    els.win.querySelector(menuReturn)?.focus({ preventScroll: true });
  }
}

function runMenuItem(index) {
  const item = menuItems[index];
  closeMenu();
  item?.run();
}

// ── Sheets ──────────────────────────────────────

/** The control focused when a sheet opened, to take focus back after. */
let sheetReturn = null;

/** Show a sheet over the page. Reopening an open sheet replaces it. */
function openSheet(html) {
  if (els.overlay.hidden) {
    sheetReturn = focusKey(document.activeElement);
  }
  closeMenu();
  els.sheet.innerHTML = html;
  els.overlay.hidden = false;
  resize();
  els.sheet.querySelector("[data-autofocus]")?.focus();
}

/** Close the sheet and refresh the page under it, which it may have changed. */
function closeSheet() {
  els.overlay.hidden = true;
  els.sheet.innerHTML = "";
  sheetConfirm = null;
  render();
  if (sheetReturn) {
    els.win.querySelector(sheetReturn)?.focus({ preventScroll: true });
  }
}

let sheetConfirm = null;

function confirmSheet({ title, body, confirm, run }) {
  sheetConfirm = run;
  openSheet(`<h2 id="sheet-title">${escapeHtml(title)}</h2><p>${escapeHtml(body)}</p>
    <p class="form-error" id="sheet-error" hidden></p>
    <div class="sheet-actions">
      <button class="pill" data-sheet="cancel" data-autofocus>Cancel</button>
      <button class="pill danger" data-sheet="confirm">${escapeHtml(confirm)}</button>
    </div>`);
}

function confirmRemove(name, kind) {
  confirmSheet({
    title: `Remove ${displayName(name)}?`,
    body: kind === "cluster"
      ? "Stops watching this cluster and every namespace in it. Nothing changes on the cluster."
      : "Stops watching this host. Nothing changes on the host.",
    confirm: "Remove",
    run: async () => {
      const config = await invoke("remove_server", { name });
      setConfig(config);
      go({ page: "home", cluster: null, namespace: null });
    },
  });
}

function confirmUnwatch(cluster, namespace) {
  confirmSheet({
    title: `Stop watching ${namespace}?`,
    body: `Its pods leave ${displayName(cluster)}, and its hidden workloads are forgotten. Nothing changes on the cluster.`,
    confirm: "Stop watching",
    run: async () => {
      const config = await invoke("unwatch_namespace", { server: cluster, namespace });
      setConfig(config);
      go({ page: "cluster", namespace: null });
    },
  });
}

function openHiddenSheet(cluster, namespace) {
  sheetConfirm = null;
  const list = hiddenOf(cluster, namespace);
  const rows = list.length === 0
    ? '<li class="hint">Nothing hidden.</li>'
    : list.map((w) => `<li><span class="kind-name"><span class="label">${escapeHtml(w.kind)}</span>${escapeHtml(w.name)}</span>
        <button class="pill" data-sheet="show" data-kind="${escapeHtml(w.kind)}" data-workload="${escapeHtml(w.name)}">Show</button></li>`).join("");
  openSheet(`<h2 id="sheet-title">Hidden in ${escapeHtml(namespace)}</h2>
    <p>Hidden workloads are left out of the list, the tray and notifications. Grafana alerts about them still fire.</p>
    <ul class="hidden-list">${rows}</ul>
    <p class="form-error" id="sheet-error" hidden></p>
    <div class="sheet-actions"><button class="pill" data-sheet="cancel" data-autofocus>Done</button></div>`);
  els.sheet.dataset.cluster = cluster;
  els.sheet.dataset.namespace = namespace;
}

els.overlay.addEventListener("click", async (e) => {
  if (e.target === els.overlay) {
    closeSheet();
    return;
  }
  const button = e.target.closest("[data-sheet]");
  if (!button) {
    return;
  }
  const kind = button.dataset.sheet;
  if (kind === "cancel") {
    closeSheet();
    return;
  }
  button.disabled = true;
  try {
    if (kind === "confirm" && sheetConfirm) {
      await sheetConfirm();
      closeSheet();
    } else if (kind === "show") {
      const { cluster, namespace } = els.sheet.dataset;
      const workload = { kind: button.dataset.kind, name: button.dataset.workload };
      const config = await invoke("show_workload", { server: cluster, namespace, workload });
      setConfig(config);
      openHiddenSheet(cluster, namespace);
    }
  } catch (err) {
    // The sheet may have been closed while the request ran.
    const error = els.sheet.querySelector("#sheet-error");
    if (!error) {
      report(err);
      return;
    }
    error.textContent = String(err);
    error.hidden = false;
    button.disabled = false;
    resize();
  }
});

// ── Rename ──────────────────────────────────────

els.win.addEventListener("dblclick", (e) => {
  const label = e.target.closest("[data-rename]");
  if (!label || label.querySelector("input")) {
    return;
  }
  const name = label.dataset.rename;
  const input = document.createElement("input");
  input.className = "rename";
  input.value = displayName(name);
  input.maxLength = 32;
  input.setAttribute("aria-label", `Display name for ${name}`);
  label.replaceChildren(input);
  input.focus();
  input.select();
  let done = false;
  const finish = (save) => {
    if (done) {
      return;
    }
    done = true;
    if (save) {
      saveAlias(name, input.value.trim());
    }
    // Gone first: render() leaves the page alone while a rename is open.
    input.remove();
    render();
  };
  input.addEventListener("blur", () => finish(true));
  input.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") {
      ev.preventDefault();
      finish(true);
    } else if (ev.key === "Escape") {
      ev.preventDefault();
      ev.stopPropagation();
      finish(false);
    }
  });
});

// ── Keyboard ────────────────────────────────────

document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape") {
    return;
  }
  if (!els.menu.hidden) {
    closeMenu();
  } else if (!els.overlay.hidden) {
    closeSheet();
  } else if (nav.page !== "home") {
    goBack();
  }
});

// Keep focus inside an open sheet.
els.overlay.addEventListener("keydown", (e) => {
  if (e.key !== "Tab") {
    return;
  }
  const buttons = [...els.sheet.querySelectorAll("button:not([disabled])")];
  if (buttons.length === 0) {
    return;
  }
  const index = buttons.indexOf(document.activeElement);
  const step = e.shiftKey ? -1 : 1;
  e.preventDefault();
  buttons[(index + step + buttons.length) % buttons.length].focus();
});

// ── Init ────────────────────────────────────────

async function init() {
  try {
    setConfig(await invoke("get_config"));
  } catch (err) {
    console.error("Failed to load config:", err);
  }
  render();

  await listen("poll-start", () => {
    state.syncing = true;
    document.querySelector(".pulse")?.classList.add("busy");
    updateLive();
  });
  await listen("metrics-update", (event) => {
    ingestMetrics(event.payload ?? {});
    render();
  });
  await listen("alerts-update", (event) => {
    ingestAlerts(event.payload ?? null);
    render();
  });

  try {
    const latest = await invoke("get_latest_metrics");
    if (latest) {
      ingestMetrics(latest);
    }
    const alerts = await invoke("get_latest_alerts");
    if (alerts) {
      ingestAlerts(alerts);
    }
  } catch (err) {
    console.error("Failed to load cached results:", err);
  }
  render();
  setInterval(updateLive, 1000);
}

init();

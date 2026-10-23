// The add and settings pages. Unlike the data pages they own their DOM
// while open: a metrics update must not re-render a half-filled form.

import { escapeHtml } from "./format.js";
import { clusters } from "./model.js";

const { invoke } = window.__TAURI__.core;

const GRAFANA_CONN_NAME = "default";

// ── Add ─────────────────────────────────────────

/**
 * Render the add page into `root`. `cluster`, when given, is an existing
 * cluster to watch another namespace of: its connection is fixed and only
 * namespaces are picked. `onAdded(config)` runs after a successful add,
 * `onResize()` whenever the form's height may have changed.
 */
export function mountAdd(root, { cluster = null, onAdded, onResize }) {
  const fixed = cluster ? clusters().find((c) => c.name === cluster) : null;
  root.innerHTML = addHtml(fixed);
  const $ = (id) => root.querySelector(`#${id}`);

  const form = {
    kind: "k8s",
    available: [],
    selected: new Set(),
    seq: 0,
  };

  const kubeconfig = $("f-kubeconfig");
  const context = $("f-context");
  const filter = $("f-ns-filter");
  const error = $("f-error");

  function showError(message) {
    error.textContent = message;
    error.hidden = !message;
    onResize();
  }

  function existing() {
    const path = kubeconfig.value.trim() || null;
    const ctx = context.value.trim();
    return clusters().find((c) => c.context === ctx && (c.kubeconfig ?? null) === path) ?? null;
  }

  function renderChips() {
    const known = existing();
    const watched = new Set(known?.namespaces.map((ns) => ns.name) ?? []);
    const term = filter.value.trim().toLowerCase();
    const names = new Set([...form.available, ...form.selected]);
    const chips = [...names]
      .filter((name) => !term || name.toLowerCase().includes(term))
      .sort()
      .map((name) => {
        const on = watched.has(name) || form.selected.has(name);
        const locked = watched.has(name);
        return `<button type="button" class="ns-chip" data-ns="${escapeHtml(name)}" aria-pressed="${on}"${locked ? " disabled" : ""}
          title="${locked ? "Already watched" : on ? "Selected" : "Select"}">${escapeHtml(name)}</button>`;
      });
    $("f-ns-chips").innerHTML = chips.length > 0
      ? chips.join("")
      : `<span class="hint">${form.available.length > 0 ? "No match. Press Enter to add it." : "Type a namespace and press Enter."}</span>`;
    const hint = $("f-target");
    if (hint && !fixed) {
      hint.textContent = known ? `Adds to ${known.name}` : "";
      $("f-name-row").hidden = Boolean(known);
    }
    $("f-ns-count").textContent = form.selected.size > 0 ? `${form.selected.size} selected` : "";
    onResize();
  }

  async function loadNamespaces() {
    form.seq += 1;
    const seq = form.seq;
    const ctx = context.value.trim();
    form.available = [];
    if (!ctx) {
      renderChips();
      return;
    }
    $("f-ns-state").textContent = "Loading namespaces…";
    try {
      const names = await invoke("list_namespaces", {
        kubeconfig: kubeconfig.value.trim() || null,
        context: ctx,
      });
      if (seq !== form.seq) {
        return;
      }
      form.available = names;
      $("f-ns-state").textContent = "";
    } catch (err) {
      if (seq !== form.seq) {
        return;
      }
      $("f-ns-state").textContent = "Could not list namespaces; type them instead.";
      console.warn("list_namespaces failed:", err);
    }
    renderChips();
  }

  async function loadContexts(path, { quiet }) {
    form.seq += 1;
    const seq = form.seq;
    let summary;
    try {
      summary = await invoke("inspect_kubeconfig", { path });
    } catch (err) {
      if (seq === form.seq && !quiet) {
        showError(String(err));
      }
      return;
    }
    if (seq !== form.seq) {
      return;
    }
    showError("");
    $("f-contexts").innerHTML = summary.contexts
      .map((name) => `<option value="${escapeHtml(name)}"></option>`)
      .join("");
    // Replace a context this kubeconfig does not define; keep one it does.
    if (summary.current_context && !summary.contexts.includes(context.value.trim())) {
      context.value = summary.current_context;
    }
    await loadNamespaces();
  }

  async function browse(command, input) {
    showError("");
    const buttons = root.querySelectorAll("[data-browse]");
    // One picker at a time: the backend's blur guard is per dialog.
    for (const b of buttons) {
      b.disabled = true;
    }
    try {
      const path = await invoke(command, { current: input.value.trim() || null });
      if (path) {
        input.value = path;
      }
      return path;
    } catch (err) {
      showError(String(err));
      return null;
    } finally {
      for (const b of buttons) {
        b.disabled = false;
      }
    }
  }

  function setKind(kind) {
    form.kind = kind;
    for (const b of root.querySelectorAll("[data-kind]")) {
      b.setAttribute("aria-pressed", String(b.dataset.kind === kind));
    }
    $("f-k8s").hidden = kind !== "k8s";
    $("f-ssh").hidden = kind !== "ssh";
    showError("");
  }

  function addTyped() {
    const name = filter.value.trim();
    if (!name) {
      return;
    }
    if (name.includes("/")) {
      showError("A namespace name cannot contain '/'.");
      return;
    }
    form.selected.add(name);
    filter.value = "";
    renderChips();
  }

  root.addEventListener("click", async (e) => {
    const kindButton = e.target.closest("[data-kind]");
    if (kindButton) {
      setKind(kindButton.dataset.kind);
      return;
    }
    const chip = e.target.closest(".ns-chip");
    if (chip && !chip.disabled) {
      const name = chip.dataset.ns;
      if (form.selected.has(name)) {
        form.selected.delete(name);
      } else {
        form.selected.add(name);
      }
      renderChips();
      return;
    }
    const browseButton = e.target.closest("[data-browse]");
    if (browseButton?.dataset.browse === "kubeconfig") {
      const path = await browse("pick_kubeconfig", kubeconfig);
      if (path) {
        await loadContexts(path, { quiet: false });
      }
    } else if (browseButton?.dataset.browse === "key") {
      await browse("pick_ssh_key", $("f-key"));
    }
  });

  kubeconfig?.addEventListener("change", () => {
    const path = kubeconfig.value.trim() || null;
    loadContexts(path, { quiet: !path });
  });
  context?.addEventListener("change", loadNamespaces);
  filter.addEventListener("input", renderChips);
  filter.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      addTyped();
    }
  });

  root.querySelector("form").addEventListener("submit", async (e) => {
    e.preventDefault();
    if (filter.value.trim()) {
      addTyped();
    }
    const built = form.kind === "k8s" ? buildCluster() : buildHost();
    if (built.error) {
      showError(built.error);
      return;
    }
    const submit = $("f-submit");
    submit.disabled = true;
    try {
      const config = await invoke("add_server", { server: built.server });
      onAdded(config);
    } catch (err) {
      showError(String(err));
    } finally {
      submit.disabled = false;
    }
  });

  function buildCluster() {
    const ctx = context.value.trim();
    const known = existing();
    const name = known ? known.name : $("f-name").value.trim();
    if (!name) {
      return { error: "Give the cluster a name." };
    }
    if (name.includes("/")) {
      return { error: "A name cannot contain '/'." };
    }
    if (!ctx) {
      return { error: "Pick a context." };
    }
    if (form.selected.size === 0) {
      return { error: "Pick at least one namespace to watch." };
    }
    return {
      server: {
        type: "k8s",
        name,
        kubeconfig: kubeconfig.value.trim() || null,
        context: ctx,
        namespaces: [...form.selected].map((ns) => ({ name: ns })),
      },
    };
  }

  function buildHost() {
    const value = (id) => $(id).value.trim();
    const port = Number.parseInt(value("f-port"), 10);
    const missing = [["f-host-name", "a name"], ["f-host", "a host"], ["f-user", "a user"], ["f-key", "a key path"]]
      .find(([id]) => !value(id));
    if (missing) {
      return { error: `Fill in ${missing[1]}.` };
    }
    if (value("f-host-name").includes("/")) {
      return { error: "A name cannot contain '/'." };
    }
    if (!Number.isInteger(port) || port < 1 || port > 65535) {
      return { error: "The port must be between 1 and 65535." };
    }
    return {
      server: {
        type: "ssh",
        name: value("f-host-name"),
        host: value("f-host"),
        port,
        user: value("f-user"),
        key_path: value("f-key"),
      },
    };
  }

  if (fixed) {
    kubeconfig.value = fixed.kubeconfig ?? "";
    context.value = fixed.context;
    loadNamespaces();
  } else {
    // The default kubeconfig's contexts, quietly: having none is fine.
    loadContexts(null, { quiet: true });
  }
  setKind("k8s");
  renderChips();
  (fixed ? filter : $("f-name")).focus();
}

function addHtml(fixed) {
  const kindSwitch = fixed
    ? ""
    : `<div class="seg" role="group" aria-label="Source type">
        <button type="button" data-kind="k8s" aria-pressed="true">Kubernetes</button>
        <button type="button" data-kind="ssh" aria-pressed="false">SSH host</button>
      </div>`;
  const connection = fixed
    ? `<input type="hidden" id="f-kubeconfig"><input type="hidden" id="f-context"><datalist id="f-contexts"></datalist>`
    : `<div class="field" id="f-name-row"><label for="f-name">Name</label><input id="f-name" type="text" placeholder="prod" autocomplete="off" spellcheck="false"></div>
       <div class="field"><label for="f-kubeconfig">Kubeconfig</label>
         <div class="with-button"><input id="f-kubeconfig" type="text" placeholder="~/.kube/config" spellcheck="false">
         <button type="button" class="pill" data-browse="kubeconfig">Browse</button></div></div>
       <div class="field"><label for="f-context">Context</label>
         <input id="f-context" type="text" list="f-contexts" placeholder="prod-ctx" spellcheck="false"><datalist id="f-contexts"></datalist></div>
       <p class="hint target" id="f-target"></p>`;
  return `<form class="form" novalidate autocomplete="off">
    ${kindSwitch}
    <div id="f-k8s" class="card fields">
      ${connection}
      <div class="field"><label for="f-ns-filter">Namespaces <span class="hint" id="f-ns-count"></span></label>
        <input id="f-ns-filter" type="text" placeholder="Filter, or type one and press Enter" spellcheck="false">
        <div class="ns-chips" id="f-ns-chips"></div>
        <span class="hint" id="f-ns-state"></span></div>
    </div>
    <div id="f-ssh" class="card fields" hidden>
      <div class="field"><label for="f-host-name">Name</label><input id="f-host-name" type="text" placeholder="bastion" spellcheck="false"></div>
      <div class="field split">
        <div><label for="f-host">Host</label><input id="f-host" type="text" placeholder="10.0.1.50" spellcheck="false"></div>
        <div class="port"><label for="f-port">Port</label><input id="f-port" type="number" value="22" min="1" max="65535"></div>
      </div>
      <div class="field"><label for="f-user">User</label><input id="f-user" type="text" placeholder="deploy" spellcheck="false"></div>
      <div class="field"><label for="f-key">Private key</label>
        <div class="with-button"><input id="f-key" type="text" placeholder="~/.ssh/id_ed25519" spellcheck="false">
        <button type="button" class="pill" data-browse="key">Browse</button></div></div>
    </div>
    <p class="form-error" id="f-error" role="alert" hidden></p>
    <button type="submit" class="pill primary wide" id="f-submit">${fixed ? "Watch namespaces" : "Add"}</button>
  </form>`;
}

// ── Settings ────────────────────────────────────

/** Render the settings page into `root`; `onSaved()` runs after a save. */
export async function mountSettings(root, { onSaved, onResize }) {
  root.innerHTML = settingsHtml();
  const $ = (id) => root.querySelector(`#${id}`);
  const error = $("s-error");

  try {
    const config = await invoke("get_config");
    $("s-fg").value = config.foreground_poll_secs ?? 10;
    $("s-bg").value = config.background_poll_secs ?? 300;
    $("s-notify").checked = config.notifications_enabled ?? false;
    const grafana = config.grafana ?? null;
    $("s-grafana").checked = Boolean(grafana?.enabled);
    $("s-url").value = grafana?.url ?? "";
    $("s-tls").checked = grafana ? grafana.verify_tls !== false : true;
    const hasToken = await invoke("has_grafana_token", { name: GRAFANA_CONN_NAME });
    $("s-token").placeholder = hasToken ? "Stored · leave blank to keep" : "Service account token";
  } catch (err) {
    error.textContent = `Could not load settings: ${err}`;
    error.hidden = false;
  }
  try {
    $("s-login").checked = await invoke("plugin:autostart|is_enabled");
  } catch (err) {
    console.warn("autostart status unavailable:", err);
  }
  onResize();

  root.querySelector("form").addEventListener("submit", async (e) => {
    e.preventDefault();
    error.hidden = true;
    const fg = Number.parseInt($("s-fg").value, 10);
    const bg = Number.parseInt($("s-bg").value, 10);
    $("s-fg").classList.toggle("invalid", !(fg >= 5 && fg <= 120));
    $("s-bg").classList.toggle("invalid", !(bg >= 30 && bg <= 600));
    if (!(fg >= 5 && fg <= 120) || !(bg >= 30 && bg <= 600)) {
      error.textContent = "Open refreshes every 5–120 s; hidden every 30–600 s.";
      error.hidden = false;
      onResize();
      return;
    }
    const url = $("s-url").value.trim();
    const settings = {
      foreground_poll_secs: fg,
      background_poll_secs: bg,
      notifications_enabled: $("s-notify").checked,
      grafana: url
        ? { name: GRAFANA_CONN_NAME, url, verify_tls: $("s-tls").checked, enabled: $("s-grafana").checked }
        : null,
    };
    try {
      // The token first: if the Keychain refuses it, nothing is saved,
      // rather than a URL that then polls without a token.
      const token = $("s-token").value.trim();
      if (token) {
        await invoke("set_grafana_token", { name: GRAFANA_CONN_NAME, token });
      }
      const config = await invoke("save_settings", { settings });
      await invoke($("s-login").checked ? "plugin:autostart|enable" : "plugin:autostart|disable");
      onSaved(config);
    } catch (err) {
      error.textContent = String(err);
      error.hidden = false;
      onResize();
    }
  });
}

function settingsHtml() {
  const toggle = (id) =>
    `<label class="toggle"><input type="checkbox" id="${id}"><span></span></label>`;
  return `<form class="form" novalidate>
    <div class="sec"><span class="label">Refresh</span></div>
    <div class="card rows">
      <label class="srow" for="s-fg"><span>While open</span><span class="unit"><input id="s-fg" type="number" min="5" max="120"> s</span></label>
      <label class="srow" for="s-bg"><span>While hidden</span><span class="unit"><input id="s-bg" type="number" min="30" max="600"> s</span></label>
    </div>
    <div class="sec"><span class="label">General</span></div>
    <div class="card rows">
      <div class="srow"><label for="s-login">Open at login</label>${toggle("s-login")}</div>
      <div class="srow"><label for="s-notify">Notify at thresholds</label>${toggle("s-notify")}</div>
    </div>
    <div class="sec"><span class="label">Grafana alerts</span></div>
    <div class="card rows">
      <div class="srow"><label for="s-grafana">Show firing alerts</label>${toggle("s-grafana")}</div>
      <div class="field"><label for="s-url">URL</label><input id="s-url" type="text" placeholder="https://grafana.internal" spellcheck="false"></div>
      <div class="field"><label for="s-token">API token</label><input id="s-token" type="password" autocomplete="off"></div>
      <div class="srow"><label for="s-tls">Verify TLS certificate</label>${toggle("s-tls")}</div>
    </div>
    <p class="form-error" id="s-error" role="alert" hidden></p>
    <button type="submit" class="pill primary wide">Save</button>
  </form>`;
}

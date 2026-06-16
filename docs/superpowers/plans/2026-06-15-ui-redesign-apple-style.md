# Apple-style UI Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Re-skin the Observer Ward tray UI from the "Tron neon" theme to a clean, professional Apple-style **Refined Dark** look (dark-only, Apple Blue accent), with no loss of functionality.

**Architecture:** Pure presentation change. The new look is driven by a full rewrite of `ui/styles.css` around a token set; `app.js` keeps generating the same CSS class names (we re-skin those exact classes), with one small palette change (`sparklineColor`). `index.html` changes only the `.ward-eye` inner markup. No Rust, no events, no commands, no features change.

**Tech Stack:** Vanilla CSS / HTML / JS (no build step, no JS test harness). Verification is manual via `cargo tauri dev` plus `node --check` / CSS brace-balance checks.

---

## File Structure

- `ui/index.html` — MODIFY (tiny): replace the 3-element `.ward-eye` internals (`.ward-core` + two `.ward-ring`) with a single `.ward-dot`. Keep the `.ward-eye` element and its `syncing`/`alert` state classes (set by `app.js`).
- `ui/app.js` — MODIFY (tiny): repoint `sparklineColor()` to a flat, subtle palette. No other JS change.
- `ui/styles.css` — REWRITE (the bulk): replace the entire file with the new themed stylesheet. Re-skins every existing class name; no renames.

The dynamic class names that `app.js` emits MUST stay styled (this is the contract). The full list lives in the spec under "Principle: re-skin existing classes" and is the checklist for Task 4.

---

## Task 1: Simplify the ward-eye markup

**Files:**
- Modify: `ui/index.html` (the `.ward-eye` block inside `.header`)

- [ ] **Step 1: Replace the ward-eye markup**

In `ui/index.html`, find this block:

```html
        <div class="ward-eye">
          <div class="ward-core"></div>
          <div class="ward-ring ward-ring-outer"></div>
          <div class="ward-ring ward-ring-inner"></div>
        </div>
```

Replace it with:

```html
        <div class="ward-eye">
          <span class="ward-dot"></span>
        </div>
```

- [ ] **Step 2: Verify the element hooks are intact**

Run: `grep -n 'ward-eye\|ward-dot\|ward-core\|ward-ring' ui/index.html ui/app.js`
Expected: `index.html` shows only `ward-eye` and `ward-dot` (no `ward-core`/`ward-ring`); `app.js` still references `.ward-eye` (the `wardEye` query selector and its `classList` `syncing`/`alert` toggles are unchanged). Confirm no `app.js` line selects `.ward-core`/`.ward-ring`/`.ward-dot` (it does not — it only toggles classes on `.ward-eye`).

- [ ] **Step 3: Commit**

```bash
git add ui/index.html
git commit -m "refactor(ui): reduce ward-eye to a single status dot"
```

---

## Task 2: Flatten the sparkline palette

**Files:**
- Modify: `ui/app.js` (`sparklineColor` function, currently ~lines 226-234)

- [ ] **Step 1: Replace `sparklineColor`**

In `ui/app.js`, find:

```js
function sparklineColor(levelClass) {
  if (levelClass === "level-crit") {
    return "rgba(255,45,111,0.4)";
  }
  if (levelClass === "level-warn") {
    return "rgba(255,184,0,0.4)";
  }
  return "rgba(0,255,240,0.4)";
}
```

Replace it with (flat, low-opacity strokes matching the new semantic palette — no neon):

```js
function sparklineColor(levelClass) {
  if (levelClass === "level-crit") {
    return "rgba(255,69,58,0.30)";
  }
  if (levelClass === "level-warn") {
    return "rgba(255,159,10,0.30)";
  }
  return "rgba(50,215,75,0.30)";
}
```

- [ ] **Step 2: Verify syntax**

Run: `node --check ui/app.js`
Expected: exit 0, no output.

- [ ] **Step 3: Commit**

```bash
git add ui/app.js
git commit -m "style(ui): flatten sparkline stroke palette"
```

---

## Task 3: Rewrite the stylesheet to Refined Dark

**Files:**
- Modify: `ui/styles.css` (replace the ENTIRE file)

- [ ] **Step 1: Replace the whole file**

Replace the entire contents of `ui/styles.css` with exactly this:

```css
/* Observer Ward -- Refined Dark (Apple-style) theme */

:root {
  /* Surfaces / lines */
  --bg: #1c1c1e;
  --bg-hover: rgba(255, 255, 255, 0.04);
  --bg-active: rgba(255, 255, 255, 0.07);
  --sep: rgba(255, 255, 255, 0.08);

  /* Text */
  --text: #ffffff;
  --text-2: rgba(255, 255, 255, 0.55);
  --text-3: rgba(255, 255, 255, 0.38);

  /* Accent (interactive chrome only) */
  --accent: #0a84ff;
  --accent-bg: rgba(10, 132, 255, 0.16);

  /* Semantic (health / status) */
  --green: #32d74b;
  --amber: #ff9f0a;
  --red: #ff453a;
  --track: rgba(255, 255, 255, 0.12);

  /* Shape */
  --r-window: 14px;
  --r-control: 8px;
  --r-pill: 6px;
  --r-bar: 3px;

  /* Type */
  --font: -apple-system, BlinkMacSystemFont, "SF Pro Text",
    "Helvetica Neue", system-ui, sans-serif;

  --pad-x: 14px;
}

/* ── Reset ─────────────────────────────────── */
*,
*::before,
*::after {
  margin: 0;
  padding: 0;
  box-sizing: border-box;
}

/* ── Base ──────────────────────────────────── */
html,
body {
  height: auto;
  overflow: hidden;
  background: transparent;
}

body {
  font-family: var(--font);
  font-size: 13px;
  line-height: 1.4;
  color: var(--text);
  user-select: none;
  -webkit-user-select: none;
  -webkit-font-smoothing: antialiased;
}

/* ── Scrollbar ─────────────────────────────── */
::-webkit-scrollbar {
  width: 6px;
}
::-webkit-scrollbar-track {
  background: transparent;
}
::-webkit-scrollbar-thumb {
  background: var(--track);
  border-radius: 3px;
}

/* ── Layout ────────────────────────────────── */
.container {
  display: flex;
  flex-direction: column;
  background: var(--bg);
  border: 1px solid var(--sep);
  border-radius: var(--r-window);
  overflow: hidden;
}

/* ── Header ────────────────────────────────── */
.header {
  display: flex;
  align-items: center;
  gap: 9px;
  padding: 13px var(--pad-x) 12px;
  border-bottom: 1px solid var(--sep);
  flex-shrink: 0;
}
.header-side {
  display: flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
}
.header-center {
  flex: 1;
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 1px;
}

/* Ward dot (header live indicator) */
.ward-eye {
  width: 16px;
  height: 16px;
  display: flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
}
.ward-dot {
  width: 9px;
  height: 9px;
  border-radius: 50%;
  background: var(--green);
  animation: breathe-ok 2.6s ease-in-out infinite;
}
.ward-eye.syncing .ward-dot {
  background: var(--accent);
  animation: breathe-sync 1.6s ease-in-out infinite;
}
.ward-eye.alert .ward-dot {
  background: var(--red);
  animation: breathe-alert 2s ease-in-out infinite;
}

.title {
  font-size: 14px;
  font-weight: 600;
  letter-spacing: 0.2px;
  color: var(--text);
}
.sync-label {
  font-size: 10px;
  font-weight: 500;
  letter-spacing: 0.2px;
  color: var(--text-3);
  opacity: 0;
  transition: opacity 0.2s ease;
}
.sync-label.visible {
  opacity: 1;
}

.btn-icon {
  background: none;
  border: none;
  color: var(--text-2);
  font-size: 15px;
  cursor: pointer;
  width: 28px;
  height: 28px;
  display: flex;
  align-items: center;
  justify-content: center;
  border-radius: var(--r-control);
  transition: background 0.18s ease, color 0.18s ease;
}
.btn-icon:hover {
  background: var(--bg-hover);
  color: var(--text);
}
/* The add (+) button reads as the primary action */
#btn-open-add {
  color: var(--accent);
}
#btn-open-add:hover {
  background: var(--accent-bg);
  color: var(--accent);
}

/* ── Server List ───────────────────────────── */
.server-list {
  flex: none;
  overflow: visible;
}

/* ── Empty State ───────────────────────────── */
.empty-state {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  padding: 28px 0;
  gap: 8px;
}
.empty-state-icon {
  font-size: 22px;
  color: var(--text-3);
}
.empty-state-text {
  color: var(--text-3);
  font-size: 12px;
  text-align: center;
  line-height: 1.6;
}

/* ── Server Card ───────────────────────────── */
.server-card {
  padding: 11px var(--pad-x);
  border-bottom: 1px solid var(--sep);
  transition: background 0.18s ease;
}
.server-card:last-child {
  border-bottom: none;
}
.server-card:hover {
  background: var(--bg-hover);
}
.server-card.offline {
  opacity: 0.5;
}
.server-card-header {
  display: flex;
  align-items: center;
  gap: 8px;
}

.status-dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  flex-shrink: 0;
}
.status-dot.online {
  background: var(--green);
}
.status-dot.offline,
.status-dot.error {
  background: var(--red);
}
.status-dot.pending {
  background: var(--text-3);
}

.server-name {
  font-size: 13px;
  font-weight: 500;
  color: var(--text);
  flex: 1;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  cursor: default;
}
.server-name:hover,
.cluster-name:hover {
  color: var(--text);
}

.alias-input {
  background: rgba(255, 255, 255, 0.06);
  border: 1px solid var(--accent);
  border-radius: var(--r-control);
  color: var(--text);
  font-family: var(--font);
  font-size: inherit;
  font-weight: inherit;
  padding: 1px 6px;
  width: 100%;
  outline: none;
}

.server-type-badge {
  font-size: 10px;
  font-weight: 500;
  letter-spacing: 0.3px;
  text-transform: uppercase;
  color: var(--text-2);
  background: var(--bg-active);
  border-radius: var(--r-pill);
  padding: 1px 7px;
  flex-shrink: 0;
}
.offline-label {
  font-size: 11px;
  font-weight: 500;
  color: var(--red);
  flex-shrink: 0;
}

/* ── Metric Rows ───────────────────────────── */
.metric-rows {
  display: flex;
  flex-direction: column;
  gap: 7px;
  margin-top: 9px;
  padding-left: 16px;
}
.metric-row {
  display: flex;
  align-items: center;
  gap: 8px;
}
.metric-label {
  width: 34px;
  font-size: 11px;
  font-weight: 400;
  color: var(--text-2);
  flex-shrink: 0;
}

.metric-bar-track {
  flex: 1;
  height: 6px;
  background: var(--track);
  border-radius: var(--r-bar);
  overflow: hidden;
  position: relative;
}
.sparkline-svg {
  position: absolute;
  top: 0;
  left: 0;
  width: 100%;
  height: 100%;
  pointer-events: none;
}
.metric-bar-fill {
  height: 100%;
  border-radius: var(--r-bar);
  width: 0%;
  transition: width 0.4s ease, background 0.3s ease;
  position: relative;
}
.metric-bar-fill.level-ok {
  background: var(--green);
}
.metric-bar-fill.level-warn {
  background: var(--amber);
}
.metric-bar-fill.level-crit {
  background: var(--red);
}

.metric-value {
  width: 38px;
  font-size: 11px;
  font-weight: 500;
  color: rgba(255, 255, 255, 0.85);
  text-align: right;
  flex-shrink: 0;
  font-variant-numeric: tabular-nums;
}

/* ── Anomaly Indicator ─────────────────────── */
.metric-row.anomaly .metric-bar-track {
  box-shadow: inset 0 0 0 1px var(--amber);
}

/* ── Network Row ───────────────────────────── */
.net-row {
  display: flex;
  align-items: center;
  gap: 14px;
  padding-left: 16px;
  margin-top: 8px;
}
.net-label {
  font-size: 11px;
  font-weight: 400;
  color: var(--text-3);
  flex-shrink: 0;
}
.net-values {
  display: flex;
  gap: 14px;
  font-size: 11px;
  color: var(--text-2);
}
.net-up,
.net-down {
  color: var(--text-2);
}
.net-up .net-arrow,
.net-down .net-arrow {
  color: var(--text-3);
  font-size: 10px;
}

/* ── Pending metrics ───────────────────────── */
.metrics-pending {
  padding-left: 16px;
  margin-top: 8px;
  font-size: 12px;
  color: var(--text-3);
}

/* ── Add Form ──────────────────────────────── */
.add-form-panel {
  display: none;
  flex-shrink: 0;
}
.add-form-panel.open {
  display: block;
}
.add-form-inner {
  padding: 14px var(--pad-x);
  border-bottom: 1px solid var(--sep);
}
.form-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--text);
  margin-bottom: 12px;
}
.form-group {
  margin-bottom: 10px;
}
.form-group label {
  display: block;
  font-size: 11px;
  font-weight: 500;
  color: var(--text-2);
  margin-bottom: 4px;
}
.form-group input,
.form-group select {
  width: 100%;
  padding: 6px 8px;
  font-family: var(--font);
  font-size: 13px;
  color: var(--text);
  background: rgba(255, 255, 255, 0.06);
  border: 1px solid var(--sep);
  border-radius: var(--r-control);
  outline: none;
  transition: border-color 0.18s ease, box-shadow 0.18s ease;
}
.form-group input:focus,
.form-group select:focus {
  border-color: var(--accent);
  box-shadow: 0 0 0 3px var(--accent-bg);
}
.form-group input::placeholder {
  color: var(--text-3);
}
.form-group select option {
  background: var(--bg);
  color: var(--text);
}
.form-group.hidden {
  display: none;
}
.form-error {
  color: var(--red);
  font-size: 12px;
  margin-bottom: 8px;
  min-height: 14px;
}
.form-actions {
  display: flex;
  gap: 8px;
  margin-top: 12px;
}

.btn {
  flex: 1;
  padding: 7px 14px;
  font-family: var(--font);
  font-size: 12px;
  font-weight: 600;
  border-radius: var(--r-control);
  cursor: pointer;
  transition: background 0.18s ease, opacity 0.18s ease;
  border: 1px solid transparent;
}
.btn-primary {
  background: var(--accent);
  border-color: var(--accent);
  color: #fff;
}
.btn-primary:hover {
  opacity: 0.9;
}
.btn-cancel {
  background: transparent;
  border-color: var(--sep);
  color: var(--text-2);
}
.btn-cancel:hover {
  background: var(--bg-hover);
  color: var(--text);
}

/* ── Settings Panel ────────────────────────── */
.settings-panel {
  display: none;
  flex-shrink: 0;
}
.settings-panel.open {
  display: block;
}
.settings-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 13px var(--pad-x) 10px;
  border-bottom: 1px solid var(--sep);
}
.settings-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--text);
}
.settings-body {
  padding: 12px var(--pad-x);
}
.settings-row {
  display: flex;
  justify-content: space-between;
  align-items: center;
  margin-bottom: 12px;
}
.settings-row label {
  font-size: 12px;
  font-weight: 400;
  color: var(--text);
}
.settings-row input[type="number"],
.settings-row input[type="text"],
.settings-row input[type="password"] {
  width: 64px;
  padding: 5px 8px;
  font-family: var(--font);
  font-size: 12px;
  color: var(--text);
  background: rgba(255, 255, 255, 0.06);
  border: 1px solid var(--sep);
  border-radius: var(--r-control);
  outline: none;
  text-align: right;
  transition: border-color 0.18s ease, box-shadow 0.18s ease;
}
.settings-row input[type="text"],
.settings-row input[type="password"] {
  width: 150px;
  text-align: left;
}
.settings-row input:focus {
  border-color: var(--accent);
  box-shadow: 0 0 0 3px var(--accent-bg);
}
.settings-row input.input-error {
  border-color: var(--red);
  box-shadow: 0 0 0 3px rgba(255, 69, 58, 0.18);
}
.settings-divider {
  height: 1px;
  background: var(--sep);
  margin: 14px 0;
}
.settings-actions {
  display: flex;
  margin-top: 4px;
}
.settings-actions .btn {
  flex: none;
  padding: 7px 22px;
}

/* ── Toggle Switch ────────────────────────── */
.toggle-switch {
  position: relative;
  display: inline-block;
  width: 38px;
  height: 22px;
  flex-shrink: 0;
}
.toggle-switch input {
  opacity: 0;
  width: 0;
  height: 0;
}
.toggle-slider {
  position: absolute;
  cursor: pointer;
  inset: 0;
  background: var(--track);
  border-radius: 11px;
  transition: background 0.18s ease;
}
.toggle-slider::before {
  content: "";
  position: absolute;
  height: 18px;
  width: 18px;
  left: 2px;
  bottom: 2px;
  background: #fff;
  border-radius: 50%;
  transition: transform 0.18s ease;
}
.toggle-switch input:checked + .toggle-slider {
  background: var(--accent);
}
.toggle-switch input:checked + .toggle-slider::before {
  transform: translateX(16px);
}

/* ── Footer ────────────────────────────────── */
.footer {
  padding: 10px var(--pad-x) 12px;
  border-top: 1px solid var(--sep);
  display: flex;
  justify-content: center;
  flex-shrink: 0;
}
.btn-settings {
  background: none;
  border: none;
  color: var(--text-2);
  font-family: var(--font);
  font-size: 12px;
  font-weight: 400;
  padding: 4px 10px;
  border-radius: var(--r-control);
  cursor: pointer;
  transition: background 0.18s ease, color 0.18s ease;
}
.btn-settings:hover {
  color: var(--text);
  background: var(--bg-hover);
}

/* ── Context Menu ──────────────────────────── */
.context-menu {
  position: fixed;
  z-index: 1000;
  background: #2c2c2e;
  border: 1px solid var(--sep);
  border-radius: var(--r-control);
  box-shadow: 0 8px 28px rgba(0, 0, 0, 0.5);
  padding: 4px;
  min-width: 150px;
  display: none;
}
.context-menu.visible {
  display: block;
}
.context-menu-item {
  padding: 6px 10px;
  font-family: var(--font);
  font-size: 12px;
  color: var(--text);
  cursor: pointer;
  border-radius: 6px;
  transition: background 0.12s ease;
}
.context-menu-item:hover {
  background: var(--accent);
  color: #fff;
}
.context-menu-separator {
  height: 1px;
  background: var(--sep);
  margin: 4px 6px;
}
.context-menu-item.danger {
  color: var(--red);
}
.context-menu-item.danger:hover {
  background: var(--red);
  color: #fff;
}

/* ── Pod Status & Badges ──────────────────── */
.pod-status-badge {
  font-size: 10px;
  font-weight: 500;
  border-radius: var(--r-pill);
  padding: 1px 6px;
  flex-shrink: 0;
}
.pod-status-badge.online {
  color: var(--green);
  background: rgba(50, 215, 75, 0.14);
}
.pod-status-badge.error {
  color: var(--red);
  background: rgba(255, 69, 58, 0.14);
}
.pod-status-badge.pending {
  color: var(--amber);
  background: rgba(255, 159, 10, 0.14);
}
.pod-age,
.node-count-badge {
  font-size: 11px;
  color: var(--text-3);
  flex-shrink: 0;
}
.restart-badge {
  font-size: 10px;
  font-weight: 500;
  border-radius: var(--r-pill);
  padding: 1px 7px;
  flex-shrink: 0;
}
.restart-badge.warn {
  color: var(--amber);
  background: rgba(255, 159, 10, 0.16);
}
.restart-badge.crit {
  color: var(--red);
  background: rgba(255, 69, 58, 0.16);
}

/* ── Cluster Summary ──────────────────────── */
.cluster-summary {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 9px var(--pad-x);
  border-bottom: 1px solid var(--sep);
  background: var(--bg-hover);
  cursor: pointer;
  transition: background 0.18s ease;
}
.cluster-summary:hover {
  background: var(--bg-active);
}
.cluster-name {
  font-size: 12px;
  font-weight: 600;
  color: var(--text);
  flex-shrink: 0;
  cursor: default;
}
.cluster-stat {
  font-size: 11px;
  color: var(--text-3);
  flex-shrink: 0;
}
.chevron {
  font-size: 10px;
  color: var(--text-3);
  flex-shrink: 0;
  transition: transform 0.18s ease;
  transform: rotate(90deg);
}
.chevron.collapsed {
  transform: rotate(0deg);
}

/* Indent pod cards under their cluster */
.cluster-pods .server-card {
  padding-left: 26px;
}

/* ── Pod Logs Button ──────────────────────── */
.btn-logs {
  background: none;
  border: 1px solid var(--sep);
  color: var(--text-2);
  font-family: var(--font);
  font-size: 10px;
  font-weight: 500;
  padding: 2px 8px;
  border-radius: var(--r-pill);
  cursor: pointer;
  transition: background 0.18s ease, color 0.18s ease,
    border-color 0.18s ease;
  flex-shrink: 0;
}
.btn-logs:hover {
  color: var(--accent);
  border-color: var(--accent);
  background: var(--accent-bg);
}

/* ── Pod Event ────────────────────────────── */
.pod-event {
  padding-left: 16px;
  margin-top: 6px;
  font-size: 11px;
  color: var(--text-3);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* ── Pod Metric Values ────────────────────── */
.metric-value.pod-value {
  width: 44px;
  font-size: 11px;
  color: rgba(255, 255, 255, 0.85);
}
.metric-pct {
  width: 30px;
  font-size: 11px;
  font-weight: 500;
  color: var(--text-3);
  text-align: right;
  flex-shrink: 0;
  font-variant-numeric: tabular-nums;
}

/* ── Grafana Alerts ───────────────────────── */
.alerts-section {
  flex-shrink: 0;
}
.alerts-section.hidden {
  display: none;
}
.alerts-header {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 12px var(--pad-x) 6px;
}
.alerts-title {
  font-size: 10.5px;
  font-weight: 600;
  letter-spacing: 0.6px;
  text-transform: uppercase;
  color: var(--text-3);
}
.alerts-status {
  font-size: 11px;
  color: var(--text-3);
  margin-left: auto;
}
.alerts-status.error {
  color: var(--red);
}
.alerts-empty {
  font-size: 12px;
  color: var(--text-3);
  padding: 4px var(--pad-x) 10px;
}
.alert-row {
  display: flex;
  align-items: stretch;
  gap: 10px;
  padding: 10px var(--pad-x);
  border-bottom: 1px solid var(--sep);
  cursor: pointer;
  transition: background 0.18s ease;
}
.alert-row:hover {
  background: var(--bg-hover);
}
.alert-row.suppressed {
  opacity: 0.5;
}
/* severity bar on the left, full row height */
.alert-dot {
  width: 3px;
  height: auto;
  align-self: stretch;
  border-radius: 2px;
  flex-shrink: 0;
}
.alert-row.sev-crit .alert-dot {
  background: var(--red);
}
.alert-row.sev-warn .alert-dot {
  background: var(--amber);
}
.alert-row.sev-info .alert-dot {
  background: var(--accent);
}
.alert-text {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-width: 0;
}
.alert-name {
  font-size: 12.5px;
  font-weight: 500;
  color: var(--text);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.alert-summary {
  font-size: 11px;
  color: var(--text-2);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.alert-age {
  font-size: 11px;
  color: var(--text-3);
  flex-shrink: 0;
}

/* ── Animations ────────────────────────────── */
@keyframes breathe-ok {
  0%, 100% { box-shadow: 0 0 0 0 rgba(50, 215, 75, 0.45); }
  50% { box-shadow: 0 0 0 5px rgba(50, 215, 75, 0); }
}
@keyframes breathe-sync {
  0%, 100% { box-shadow: 0 0 0 0 rgba(10, 132, 255, 0.5); }
  50% { box-shadow: 0 0 0 5px rgba(10, 132, 255, 0); }
}
@keyframes breathe-alert {
  0%, 100% { box-shadow: 0 0 0 0 rgba(255, 69, 58, 0.5); }
  50% { box-shadow: 0 0 0 6px rgba(255, 69, 58, 0); }
}

@media (prefers-reduced-motion: reduce) {
  .ward-dot,
  .ward-eye.syncing .ward-dot,
  .ward-eye.alert .ward-dot {
    animation: none;
  }
  * {
    transition: none !important;
  }
}
```

- [ ] **Step 2: Verify CSS brace balance**

Run:
```bash
node -e "const s=require('fs').readFileSync('ui/styles.css','utf8');const o=(s.match(/{/g)||[]).length,c=(s.match(/}/g)||[]).length;console.log('open',o,'close',c,o===c?'BALANCED':'MISMATCH');"
```
Expected: `BALANCED`.

- [ ] **Step 3: Visual smoke test**

Run `cargo tauri dev` from the repo root. With at least one SSH server and one K8s cluster configured (or the empty state), confirm: dark `#1c1c1e` window, system font (not monospace), header with a green breathing dot + "Observer Ward" title + blue `+` + gray gear, server cards with green/amber/red bars (no neon glow), hairline separators between rows. No element appears as raw unstyled text.

- [ ] **Step 4: Commit**

```bash
git add ui/styles.css
git commit -m "style(ui): rewrite stylesheet to Refined Dark Apple theme"
```

---

## Task 4: Full-state QA pass and re-skin verification

**Files:**
- Modify (only if fixes needed): `ui/styles.css`

This task finds and fixes any class the rewrite missed or any state that looks wrong. No new feature work.

- [ ] **Step 1: Cross-check every dynamic class is styled**

Run this to list class names `app.js` applies, then confirm each has a rule in `ui/styles.css`:
```bash
grep -oE 'class="[^"]*"' ui/app.js | tr ' ' '\n' | grep -oE '[a-z][a-z0-9-]+' | sort -u
```
For each class in the spec's "re-skin existing classes" list, confirm a matching selector exists:
```bash
for c in container ward-eye title sync-label btn-icon server-list empty-state server-card status-dot server-name alias-input server-type-badge offline-label metric-rows metric-row metric-label metric-bar-track sparkline-svg metric-bar-fill metric-value metric-pct net-row net-label net-values net-up net-down net-arrow metrics-pending add-form-panel add-form-inner form-title form-group form-error form-actions btn btn-primary btn-cancel settings-panel settings-header settings-title settings-body settings-row settings-divider settings-actions toggle-switch toggle-slider footer btn-settings context-menu context-menu-item context-menu-separator cluster-summary cluster-name cluster-stat chevron btn-logs pod-event pod-age restart-badge pod-status-badge node-count-badge alerts-section alerts-header alerts-title alerts-status alerts-empty alert-row alert-dot alert-text alert-name alert-summary alert-age; do grep -q "\.$c" ui/styles.css || echo "MISSING: .$c"; done
```
Expected: no `MISSING:` lines. If any appear, add a styled rule for it (match the theme), then re-run.

- [ ] **Step 2: Manual walkthrough of every state**

Run `cargo tauri dev` and verify each, fixing any off-theme styling in `ui/styles.css` as you go:
- Empty state (no servers): icon + text are muted, centered.
- SSH server online: green dot, name, SSH badge, CPU/Mem/Disk bars colored by level, net row subtle.
- Server offline/error: red dot, card dimmed, "offline" label red.
- Server pending: gray dot, "awaiting metrics..." muted.
- K8s cluster: header row with chevron; click to collapse/expand (chevron rotates); pods indented with inset hairlines.
- Pod with restarts: amber/red `2 restarts` pill; pod event line muted; logs button (hover → blue).
- Anomaly: a metric bar track shows a thin amber inset ring (no pulsing).
- Grafana alerts: uppercase gray section header + count; crit row red left bar, warn amber, info blue; suppressed dimmed; `alerts-status.error` red; empty/unreachable text muted.
- Add form: open via `+`; switch type k8s↔ssh (fields show/hide); focus shows blue ring; trigger a validation error (red text); primary button solid blue, cancel subtle.
- Settings: open via footer; number inputs + the redesigned toggle (off gray / on blue, knob slides); Grafana URL/token text+password inputs; divider hairline; SAVE button.
- Context menu: right-click a server (Open Terminal / Copy kubectl / Copy Metrics / Remove-danger-red) and a pod (View Logs); hover highlights blue, danger highlights red.
- Alias edit: double-click a server/cluster name → inline input with blue border.
- Header states: idle (green breathing dot); during a poll the `.syncing` class (blue dot, faster) and `sync-label` "syncing" caption appear; if a metric is critical the `.alert` class (red dot) shows.

- [ ] **Step 3: Reduced-motion check**

Temporarily enable macOS "Reduce Motion" (System Settings → Accessibility → Display) OR confirm by code review that the `@media (prefers-reduced-motion: reduce)` block disables `.ward-dot` animation and transitions. Confirm the dot stops breathing.

- [ ] **Step 4: Commit any fixes**

```bash
git add ui/styles.css
git commit -m "style(ui): QA fixes for Refined Dark theme"
```
(If no fixes were needed, skip the commit and note "QA clean — no fixes required".)

---

## Self-Review

**Spec coverage:**
- Refined Dark, dark-only, Apple Blue accent, semantic health colors → Task 3 tokens + `.metric-bar-fill.level-*` (green/amber/red) + accent confined to `#btn-open-add`, focus rings, toggle, `btn-primary`, `btn-logs` hover, context-menu hover, `sev-info`.
- Ward-eye → single breathing dot → Task 1 (markup) + Task 3 (`.ward-dot` + `breathe-*` keyframes + `.syncing`/`.alert` states).
- System font, 13px base, tabular numerals → Task 3 `--font`, body, `.metric-value`/`.metric-pct` `tabular-nums`.
- Restrained motion, neon removed, `prefers-reduced-motion` → Task 3 (only `breathe-*` keyframes remain; reduced-motion block).
- Sparkline flattened, anomaly = inset ring → Task 2 (`sparklineColor`) + Task 3 (`.metric-row.anomaly` inset ring).
- macOS list grouping (hairlines, cluster header, uppercase section header) → Task 3 server-card/cluster-summary/alerts-header.
- Re-skin every existing class (no renames) → Task 3 covers the full list; Task 4 Step 1 verifies none missing.
- Per-component specs (header, cards, metrics, net, cluster/pods, alerts, add form, settings, toggle, context menu, scrollbar, empty, alias) → all present in Task 3.

**Placeholder scan:** No TBD/TODO; Task 3 contains the complete file; every command has expected output. Task 4 Step 4 explicitly handles the "no fixes" case.

**Type/contract consistency:** All selectors in Task 3 match class names `app.js`/`index.html` emit (verified against the read source). `sparklineColor` return values (Task 2) are valid CSS colors consumed by the inline SVG stroke. The `.ward-eye` element + `syncing`/`alert` classes (untouched in `app.js`) are styled in Task 3. `#btn-open-add` id matches `index.html`. The alerts `.alert-dot` is repurposed as the left severity bar (the JS renders `<span class="alert-dot">`); confirmed consistent with the existing `app.js` `renderAlertRow`.

# UI Redesign — "Refined Dark" Apple-style — Design Spec

**Date:** 2026-06-15
**Status:** Approved for planning
**Component:** Observer Ward frontend (`ui/`)

## Goal

Re-skin the tray UI from its current "Tron neon" theme to a clean, slick,
professional **Apple-style dark** look (think macOS Control Center / Notification
Center), while keeping a small amount of the brand's futurism. No functionality
changes — every existing feature and screen is preserved; this is a visual
restyle plus light layout/spacing polish.

Direction chosen during brainstorming (with rendered mockups):
- **Refined Dark**, dark-only (no light mode this pass).
- **Apple Blue** accent (`#0A84FF`) used in interactive chrome only.
- **Semantic traffic-light** colors for metric health (green/amber/red).
- The orbiting "ward-eye" reduces to a single subtly **breathing live dot**.

## Scope

In scope:
- Full rewrite of `ui/styles.css` to the new theme.
- Small `ui/index.html` change: simplify the `.ward-eye` markup (remove the two
  orbiting `.ward-ring` elements and `.ward-core`; replace with one dot element)
  while keeping the `.ward-eye` container and its `syncing`/`alert` state classes
  that `app.js` toggles.
- Minimal `ui/app.js` change: update the sparkline stroke palette
  (`sparklineColor`) to flat, subtle colors. No logic/feature changes.

Out of scope (YAGNI):
- Light mode / system-appearance switching.
- Any change to features, data shown, Rust backend, events, or commands.
- Renaming the dynamic CSS classes that `app.js` generates (we re-skin the
  existing class names so JS stays untouched apart from the sparkline palette).
- The menu-bar tray icon PNGs (separate assets, unchanged).

## Principle: re-skin existing classes

`app.js` generates fixed class names for dynamic content. The redesign **restyles
these exact classes** rather than renaming them, so the JS↔CSS contract holds.
Classes that must remain styled (non-exhaustive, verified against current
`app.js`/`index.html`):
`container, header, ward-eye (+ .syncing/.alert), title, sync-label, btn-icon,
server-list, empty-state(+icon/text), server-card(+ .offline), server-card-header,
status-dot(+ .online/.offline/.pending/.error), server-name, alias-input,
server-type-badge, offline-label, node-count-badge, metric-rows, metric-row(+
.m-cpu/.m-mem/.m-disk/.m-pvc/.anomaly), metric-label, metric-bar-track,
sparkline-svg, metric-bar-fill(+ .level-ok/.level-warn/.level-crit), metric-value(+
.pod-value), metric-pct, net-row/net-label/net-values/net-up/net-down/net-arrow,
metrics-pending, add-form-panel(+ .open), add-form-inner, form-title, form-group(+
.hidden), form-error, form-actions, btn(+ .btn-primary/.btn-cancel), settings-panel(+
.open), settings-header, settings-title, settings-body, settings-row, settings-divider,
settings-actions, toggle-switch/toggle-slider, footer, btn-settings, context-menu(+
.visible), context-menu-item(+ .danger), context-menu-separator, cluster-summary,
cluster-name, cluster-stat, chevron(+ .collapsed), btn-logs, pod-event, pod-age,
restart-badge(+ .warn/.crit), pod-status-badge(+ .online/.error/.pending),
alerts-section(+ .hidden), alerts-header, alerts-title, alerts-status(+ .error),
alerts-empty, alert-row(+ .suppressed/.sev-crit/.sev-warn/.sev-info), alert-dot,
alert-text, alert-name, alert-summary, alert-age`.

The implementation plan must include a checklist verifying each of these renders
correctly in the new theme before sign-off.

## Design tokens (CSS custom properties on `:root`)

```
Surfaces / lines
  --bg:        #1c1c1e   /* window content background */
  --bg-hover:  rgba(255,255,255,0.04)
  --bg-active: rgba(255,255,255,0.07)
  --sep:       rgba(255,255,255,0.08)   /* hairline separators */

Text
  --text:   #ffffff
  --text-2: rgba(255,255,255,0.55)      /* secondary / labels */
  --text-3: rgba(255,255,255,0.38)      /* tertiary / captions */

Accent (interactive chrome only)
  --accent:    #0a84ff
  --accent-bg: rgba(10,132,255,0.16)

Semantic (health / status — NOT the accent)
  --green: #32d74b   /* healthy / online */
  --amber: #ff9f0a   /* warning */
  --red:   #ff453a   /* critical / offline / error */
  --track: rgba(255,255,255,0.12)   /* metric bar track */

Shape
  --r-window: 14px;  --r-control: 8px;  --r-pill: 6px;  --r-bar: 3px

Type
  --font: -apple-system, BlinkMacSystemFont, "SF Pro Text", "Helvetica Neue",
          system-ui, sans-serif;   /* JetBrains Mono is dropped */
  base body font-size: 13px; line-height 1.4
  Metric values use font-variant-numeric: tabular-nums.
```

Type scale: title 14/600; server & pod names 13/500; section/list secondary
11/400; uppercase section headers & labels 10.5/600 letter-spacing 0.6px in
`--text-3`; metric values 11 tabular.

## Motion

Restrained. Remove: the orbiting `ward-orbit` ring animations, the pulsing
`bar-pulse` metric-edge, `pulse-pink`, neon `text-shadow`/`box-shadow` glows,
`anomaly-pulse` glow, `ward-core-*` scale pulses. Keep:
- A single header **live dot**: a 2.6s ease-in-out "breathe" (an expanding,
  fading 0→5px ring via `box-shadow`), green normally.
- `.ward-eye.syncing` → dot/ring uses accent blue and a slightly faster breathe.
- `.ward-eye.alert` → dot red, breathe in red.
- Standard transitions 0.18s ease on hover/focus/toggle.
- Wrap all non-essential animation in `@media (prefers-reduced-motion: reduce)`
  to disable the breathe and reduce transitions.

## Component specs

**Window / container.** `--bg`, `--r-window`, 1px `--sep` border, 0 internal
neon. Remove the deep gradient; flat `--bg`. Padding 0 (rows manage their own
padding); list rows span full width with inset hairlines.

**Header.** `live dot` (9px, breathing) + title (14/600, sentence case
"Observer Ward", not uppercase/letter-spaced) + spacer + `+` add button (blue
glyph on `--accent-bg`, `--r-control`) + gear (`--text-2`). Bottom hairline.
`sync-label` becomes a quiet `--text-3` caption (no neon).

**Server card.** Full-width row, 11px/14px padding, hairline between cards.
Header line: `status-dot` (green online / red offline-pulsing-removed→solid /
gray pending) + name (13/500, ellipsis) + type badge (gray pill `--bg-active`/
`--text-2`) + optional node-count/offline label. Hover `--bg-hover`. Offline
card 0.5 opacity.

**Metric rows.** Label (32px, `--text-2`, sentence case "CPU"/"Mem"/"Disk") +
track (6px, `--track`, `--r-bar`) with fill colored by level: ok→`--green`,
warn→`--amber`, crit→`--red` (flat, no gradient, no glow, no animated edge) +
value (right, tabular, `--text` 0.85). Drop the per-metric cyan/purple/green
label tinting — labels are uniformly `--text-2` for calm.

**Sparkline + anomaly (retained, subtle).** `sparkline-svg` stays behind the
fill; `app.js` `sparklineColor` updated to a flat low-opacity stroke matching the
level (e.g. `rgba(255,255,255,0.18)` or a 25%-opacity level tint). Anomaly:
replace the glow with a thin 1px `--amber` ring on `.metric-row.anomaly
.metric-bar-track` (no pulsing).

**Net row.** Subtle `--text-2`, up/down with small arrows, values `--text` 0.8,
no amber/cyan tint.

**Cluster summary + pods.** Cluster header row: chevron (`--text-3`, rotates),
name (12/600), stats (`--text-3`), faint `--bg-hover` background, hover
`--bg-active`. Pods render as indented cards (left padding ~26px) with inset
hairlines. Restart badge: amber pill on `rgba(255,159,10,0.16)`; crit variant
red. Age/node-count: `--text-3` captions.

**Grafana alerts.** Uppercase-gray section header ("Grafana Alerts" + "· N
firing" count in `--text-3`). Alert row: a 3px full-height severity bar
(crit→red, warn→amber, info→accent), name (12.5/500), summary (`--text-2`), age
(`--text-3`, top-right). Suppressed → 0.5 opacity. `alerts-status.error` →
`--red`. `alerts-empty` → `--text-3`. Keep the existing `sev-crit/sev-warn/
sev-info` and `suppressed` hooks.

**Add form & settings.** Inputs: `--bg` darker fill (`rgba(255,255,255,0.06)`),
1px `--sep` border, `--r-control`, focus → `--accent` border + `--accent-bg`
ring (no neon). Labels sentence-case `--text-2` (drop uppercase letter-spacing).
Buttons: primary = white text on `--accent` solid, hover slightly darker; cancel
= text button `--text-2`. `settings-divider` → `--sep`. Section/title text drops
neon `text-shadow`.

**Toggle switch.** Off: track `--track`, knob `--text-2`. On: track `--accent`,
knob white (Apple switch), smooth 0.18s, no glow.

**Context menu.** `--bg` panel, 1px `--sep`, `--r-control`, soft shadow
`0 8px 28px rgba(0,0,0,0.5)` (no cyan glow). Item hover `--bg-hover`; `.danger`
text `--red`, hover `rgba(255,69,58,0.12)`.

**Scrollbar / empty state / alias input.** Scrollbar thumb `--track`. Empty
state icon/text `--text-3`. `alias-input` inline edit: `--bg` fill, `--accent`
border, `--r-control`, no glow.

## Testing

No JS test harness. Verify manually via `cargo tauri dev` across states:
online/offline/pending/error servers; SSH metrics + net row; K8s cluster
expand/collapse with pods, restart badges, crit/warn/ok bars, pod events;
anomaly highlight; Grafana alerts firing/suppressed/empty/unreachable; add form
(k8s + ssh field toggle, validation error); settings panel (intervals,
toggles incl. the redesigned switch, Grafana fields); context menu (server vs
pod variants); alias inline-edit; empty state. Confirm `prefers-reduced-motion`
disables the breathe. Walk the "re-skin existing classes" checklist so no
dynamic class is left unstyled (which would render as unstyled text).

## Files

- `ui/styles.css` — full rewrite to the new theme (largest change).
- `ui/index.html` — simplify `.ward-eye` inner markup only.
- `ui/app.js` — update `sparklineColor` palette only.

## Open items for the plan

- Decide the exact sparkline stroke (flat white-opacity vs per-level tint) —
  pick during implementation against the live render; default to a 22%-opacity
  level-tinted stroke.
- Confirm the window content width stays 380px (set in `app.js` `WIN_WIDTH`); the
  new spacing is designed to fit it. No `app.js` width change planned.

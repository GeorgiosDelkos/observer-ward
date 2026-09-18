# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.1.0] - 2026-09-18

First tagged shape of the app: a macOS menu-bar dashboard for Kubernetes, SSH hosts, and Grafana alerts.

### Added

- Kubernetes cluster and per-pod metrics (Metrics API, restarts, PVC, events)
- SSH host metrics over key-based auth (`top` / `free` / `df` / `/proc/net/dev`)
- Grafana firing-alert ingestion; token in the OS keychain
- Tray popover with refined dark UI, dual poll intervals, autostart, threshold notifications
- In-app remove with confirmation; expanded pod lists capped to three visible rows and sorted A–Z

### Fixed

- macOS 27 tray left-click swallowed by an attached status-item menu; popover now opens on click, Quit lives in the footer

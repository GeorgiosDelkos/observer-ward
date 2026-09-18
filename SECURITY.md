# Security

Observer Ward is a local macOS menu-bar app. It stores:

- Non-secret config in `~/.config/observer-ward/config.json` (cluster names, kubeconfig *paths*, SSH host/user/key *paths*)
- The Grafana service-account token in the macOS Keychain (never in `config.json`)

SSH private keys and kubeconfig files stay where you pointed the app; they are not copied into the config directory.

## Reporting a vulnerability

Please use [GitHub private vulnerability reporting](https://github.com/GeorgiosDelkos/observer-ward/security/advisories/new) on this repository.

Do not open a public issue for a security report.

## Supported versions

Only the current `main` branch (and any tagged release cut from it) is supported.

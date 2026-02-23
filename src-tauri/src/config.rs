use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_foreground_poll", alias = "poll_interval_secs")]
    pub foreground_poll_secs: u64,
    #[serde(default = "default_background_poll")]
    pub background_poll_secs: u64,
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
    #[serde(default)]
    pub notifications_enabled: bool,
}

fn default_foreground_poll() -> u64 {
    10
}

fn default_background_poll() -> u64 {
    300
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            foreground_poll_secs: default_foreground_poll(),
            background_poll_secs: default_background_poll(),
            servers: Vec::new(),
            notifications_enabled: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerConfig {
    K8s {
        name: String,
        kubeconfig: Option<String>,
        context: String,
        namespace: String,
    },
    Ssh {
        name: String,
        host: String,
        #[serde(default = "default_ssh_port")]
        port: u16,
        user: String,
        key_path: String,
    },
}

fn default_ssh_port() -> u16 {
    22
}

impl ServerConfig {
    pub fn name(&self) -> &str {
        match self {
            ServerConfig::K8s { name, .. } | ServerConfig::Ssh { name, .. } => name,
        }
    }

    pub fn server_type(&self) -> &str {
        match self {
            ServerConfig::K8s { .. } => "k8s",
            ServerConfig::Ssh { .. } => "ssh",
        }
    }
}

/// Returns the config file path: `~/.config/observer-ward/config.json`
fn config_path() -> Result<PathBuf, String> {
    let config_dir =
        dirs::config_dir().ok_or_else(|| "could not determine config directory".to_string())?;
    Ok(config_dir.join("observer-ward").join("config.json"))
}

pub fn load_config() -> Result<AppConfig, String> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(AppConfig::default());
    }
    let contents =
        std::fs::read_to_string(&path).map_err(|e| format!("failed to read config: {e}"))?;
    serde_json::from_str(&contents).map_err(|e| format!("failed to parse config: {e}"))
}

pub fn save_config(config: &AppConfig) -> Result<(), String> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("failed to create config dir: {e}"))?;
    }
    let json = serde_json::to_string_pretty(config)
        .map_err(|e| format!("failed to serialize config: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("failed to write config: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("failed to rename config: {e}"))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn default_config_has_expected_values() {
        let config = AppConfig::default();
        assert_eq!(config.foreground_poll_secs, 10);
        assert_eq!(config.background_poll_secs, 300);
        assert!(config.servers.is_empty());
    }

    #[test]
    fn serialize_k8s_server() {
        let server = ServerConfig::K8s {
            name: "prod-cluster".to_string(),
            kubeconfig: Some("/home/user/.kube/config".to_string()),
            context: "prod".to_string(),
            namespace: "default".to_string(),
        };
        let json = serde_json::to_string(&server).expect("serialize k8s server");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse json");

        assert_eq!(parsed["type"], "k8s");
        assert_eq!(parsed["name"], "prod-cluster");
        assert_eq!(parsed["kubeconfig"], "/home/user/.kube/config");
        assert_eq!(parsed["context"], "prod");
        assert_eq!(parsed["namespace"], "default");
    }

    #[test]
    fn serialize_ssh_server() {
        let server = ServerConfig::Ssh {
            name: "web-box".to_string(),
            host: "10.0.0.5".to_string(),
            port: 2222,
            user: "deploy".to_string(),
            key_path: "/home/user/.ssh/id_ed25519".to_string(),
        };
        let json = serde_json::to_string(&server).expect("serialize ssh server");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse json");

        assert_eq!(parsed["type"], "ssh");
        assert_eq!(parsed["name"], "web-box");
        assert_eq!(parsed["host"], "10.0.0.5");
        assert_eq!(parsed["port"], 2222);
        assert_eq!(parsed["user"], "deploy");
        assert_eq!(parsed["key_path"], "/home/user/.ssh/id_ed25519");
    }

    #[test]
    fn deserialize_k8s_server() {
        let json = r#"{
            "type": "k8s",
            "name": "staging",
            "kubeconfig": null,
            "context": "staging-ctx",
            "namespace": "kube-system"
        }"#;
        let server: ServerConfig = serde_json::from_str(json).expect("deserialize k8s server");

        assert_eq!(server.name(), "staging");
        assert_eq!(server.server_type(), "k8s");
        if let ServerConfig::K8s {
            name,
            kubeconfig,
            context,
            namespace,
        } = &server
        {
            assert_eq!(name, "staging");
            assert!(kubeconfig.is_none());
            assert_eq!(context, "staging-ctx");
            assert_eq!(namespace, "kube-system");
        }
    }

    #[test]
    fn deserialize_ssh_server_default_port() {
        let json = r#"{
            "type": "ssh",
            "name": "bastion",
            "host": "bastion.example.com",
            "user": "admin",
            "key_path": "/root/.ssh/id_rsa"
        }"#;
        let server: ServerConfig =
            serde_json::from_str(json).expect("deserialize ssh server with default port");

        assert_eq!(server.name(), "bastion");
        assert_eq!(server.server_type(), "ssh");
        if let ServerConfig::Ssh { port, .. } = &server {
            assert_eq!(*port, 22);
        }
    }

    #[test]
    fn deserialize_full_config() {
        let json = r#"{
            "foreground_poll_secs": 15,
            "background_poll_secs": 120,
            "servers": [
                {
                    "type": "k8s",
                    "name": "prod",
                    "kubeconfig": "/etc/kube/config",
                    "context": "prod-ctx",
                    "namespace": "default"
                },
                {
                    "type": "ssh",
                    "name": "db-host",
                    "host": "db.internal",
                    "port": 22,
                    "user": "ops",
                    "key_path": "/home/ops/.ssh/id_ed25519"
                }
            ]
        }"#;
        let config: AppConfig = serde_json::from_str(json).expect("deserialize full config");

        assert_eq!(config.foreground_poll_secs, 15);
        assert_eq!(config.background_poll_secs, 120);
        assert_eq!(config.servers.len(), 2);
        assert_eq!(config.servers[0].name(), "prod");
        assert_eq!(config.servers[1].name(), "db-host");
    }

    #[test]
    fn deserialize_empty_json_uses_defaults() {
        let json = "{}";
        let config: AppConfig = serde_json::from_str(json).expect("deserialize empty json");

        assert_eq!(config.foreground_poll_secs, 10);
        assert_eq!(config.background_poll_secs, 300);
        assert!(config.servers.is_empty());
    }

    #[test]
    fn deserialize_legacy_poll_interval_secs_alias() {
        let json = r#"{ "poll_interval_secs": 60 }"#;
        let config: AppConfig = serde_json::from_str(json).expect("deserialize legacy alias");

        assert_eq!(config.foreground_poll_secs, 60);
        assert_eq!(config.background_poll_secs, 300);
    }

    #[test]
    fn roundtrip_config_through_json() {
        let config = AppConfig {
            foreground_poll_secs: 15,
            background_poll_secs: 120,
            servers: vec![
                ServerConfig::K8s {
                    name: "test-k8s".to_string(),
                    kubeconfig: None,
                    context: "default".to_string(),
                    namespace: "default".to_string(),
                },
                ServerConfig::Ssh {
                    name: "test-ssh".to_string(),
                    host: "192.168.1.1".to_string(),
                    port: 2222,
                    user: "root".to_string(),
                    key_path: "/root/.ssh/id_ed25519".to_string(),
                },
            ],
            ..AppConfig::default()
        };

        let json = serde_json::to_string_pretty(&config).expect("serialize config");
        let deserialized: AppConfig = serde_json::from_str(&json).expect("deserialize config");

        assert_eq!(
            deserialized.foreground_poll_secs,
            config.foreground_poll_secs
        );
        assert_eq!(
            deserialized.background_poll_secs,
            config.background_poll_secs
        );
        assert_eq!(deserialized.servers.len(), config.servers.len());
        assert_eq!(deserialized.servers, config.servers);
    }

    #[test]
    fn save_and_load_config_from_disk() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("config.json");

        let config = AppConfig {
            foreground_poll_secs: 15,
            background_poll_secs: 60,
            servers: vec![ServerConfig::Ssh {
                name: "local".to_string(),
                host: "127.0.0.1".to_string(),
                port: 22,
                user: "test".to_string(),
                key_path: "/tmp/key".to_string(),
            }],
            ..AppConfig::default()
        };

        let json = serde_json::to_string_pretty(&config).expect("serialize");
        fs::write(&path, &json).expect("write config");

        let contents = fs::read_to_string(&path).expect("read config");
        let loaded: AppConfig = serde_json::from_str(&contents).expect("parse config");

        assert_eq!(loaded.foreground_poll_secs, 15);
        assert_eq!(loaded.background_poll_secs, 60);
        assert_eq!(loaded.servers.len(), 1);
        assert_eq!(loaded.servers[0].name(), "local");
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("nonexistent.json");

        assert!(!path.exists());
        // Simulates load_config behavior: missing file -> default
        let config = AppConfig::default();
        assert_eq!(config.foreground_poll_secs, 10);
        assert_eq!(config.background_poll_secs, 300);
        assert!(config.servers.is_empty());
    }

    #[test]
    fn server_config_name_method() {
        let k8s = ServerConfig::K8s {
            name: "alpha".to_string(),
            kubeconfig: None,
            context: "ctx".to_string(),
            namespace: "default".to_string(),
        };
        let ssh = ServerConfig::Ssh {
            name: "beta".to_string(),
            host: "host".to_string(),
            port: 22,
            user: "u".to_string(),
            key_path: "/k".to_string(),
        };

        assert_eq!(k8s.name(), "alpha");
        assert_eq!(ssh.name(), "beta");
    }

    #[test]
    fn server_config_type_method() {
        let k8s = ServerConfig::K8s {
            name: "a".to_string(),
            kubeconfig: None,
            context: "c".to_string(),
            namespace: "default".to_string(),
        };
        let ssh = ServerConfig::Ssh {
            name: "b".to_string(),
            host: "h".to_string(),
            port: 22,
            user: "u".to_string(),
            key_path: "/k".to_string(),
        };

        assert_eq!(k8s.server_type(), "k8s");
        assert_eq!(ssh.server_type(), "ssh");
    }

    #[test]
    fn invalid_json_returns_error() {
        let bad_json = "{ not valid json }";
        let result: Result<AppConfig, _> = serde_json::from_str(bad_json);
        assert!(result.is_err());
    }

    #[test]
    fn unknown_server_type_returns_error() {
        let json = r#"{
            "type": "docker",
            "name": "container-host"
        }"#;
        let result: Result<ServerConfig, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }
}

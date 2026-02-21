use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter};
use tokio::time::sleep;

use crate::config::{AppConfig, ServerConfig};
use crate::k8s_backend::K8sBackend;
use crate::metrics::{MetricsUpdate, ServerMetrics, ServerStatus};
use crate::ssh_backend::SshBackend;

const COLLECT_TIMEOUT: Duration = Duration::from_secs(30);
const BACKOFF_THRESHOLD: u32 = 3;
const BACKOFF_DURATION: Duration = Duration::from_secs(120);

struct FailureState {
    count: u32,
    last_attempt: Instant,
}

pub struct Poller {
    app_handle: AppHandle,
    config_state: Arc<Mutex<AppConfig>>,
    ssh_backends: HashMap<String, SshBackend>,
    k8s_backends: HashMap<String, K8sBackend>,
    failures: HashMap<String, FailureState>,
}

impl Poller {
    pub fn new(app_handle: AppHandle, config_state: Arc<Mutex<AppConfig>>) -> Self {
        Self {
            app_handle,
            config_state,
            ssh_backends: HashMap::new(),
            k8s_backends: HashMap::new(),
            failures: HashMap::new(),
        }
    }

    pub async fn run(&mut self) {
        loop {
            let snapshot = self.config_state.lock().ok().map(|c| c.clone());

            let Some(config) = snapshot else {
                tracing::error!("config lock poisoned");
                sleep(Duration::from_secs(5)).await;
                continue;
            };

            let servers = config.servers;
            let poll_interval = config.poll_interval_secs;

            self.cleanup_removed_backends(&servers);

            let mut all_metrics = Vec::with_capacity(servers.len());

            for server in &servers {
                let name = server.name();
                let server_type = server.server_type();

                if self.in_backoff(name) {
                    all_metrics.push(offline_metrics(name, server_type));
                    continue;
                }

                match self.collect_for_server(server).await {
                    Ok(metrics) => {
                        self.failures.remove(name);
                        all_metrics.push(metrics);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "failed to collect metrics \
                             for {name}: {e}"
                        );
                        self.record_failure(name);
                        all_metrics.push(offline_metrics(name, server_type));
                    }
                }
            }

            let update = MetricsUpdate {
                servers: all_metrics,
            };
            if let Err(e) = self.app_handle.emit("metrics-update", &update) {
                tracing::warn!("failed to emit metrics-update: {e}");
            }

            sleep(Duration::from_secs(poll_interval)).await;
        }
    }

    async fn collect_for_server(&mut self, server: &ServerConfig) -> Result<ServerMetrics, String> {
        let result = tokio::time::timeout(COLLECT_TIMEOUT, self.collect_inner(server)).await;

        if let Ok(inner) = result {
            inner
        } else {
            let name = server.name();
            self.drop_backend(name);
            Err(format!("timed out collecting metrics for {name}"))
        }
    }

    async fn collect_inner(&mut self, server: &ServerConfig) -> Result<ServerMetrics, String> {
        match server {
            ServerConfig::Ssh {
                name,
                host,
                port,
                user,
                key_path,
            } => {
                let backend = self.ssh_backends.entry(name.clone()).or_insert_with(|| {
                    SshBackend::new(host.clone(), *port, user.clone(), key_path.clone())
                });

                if !backend.is_connected() {
                    if let Err(e) = backend.connect().await {
                        backend.disconnect().await;
                        return Err(e);
                    }
                }

                let result = backend.collect_metrics(name).await;
                if result.is_err() {
                    backend.disconnect().await;
                }
                result
            }
            ServerConfig::K8s {
                name,
                kubeconfig,
                context,
            } => {
                let backend = self
                    .k8s_backends
                    .entry(name.clone())
                    .or_insert_with(|| K8sBackend::new(kubeconfig.clone(), context.clone()));

                if !backend.is_connected() {
                    backend.connect().await?;
                }

                backend.collect_metrics(name).await
            }
        }
    }

    fn in_backoff(&self, name: &str) -> bool {
        let Some(state) = self.failures.get(name) else {
            return false;
        };
        state.count >= BACKOFF_THRESHOLD && state.last_attempt.elapsed() < BACKOFF_DURATION
    }

    fn record_failure(&mut self, name: &str) {
        let state = self
            .failures
            .entry(name.to_string())
            .or_insert_with(|| FailureState {
                count: 0,
                last_attempt: Instant::now(),
            });
        state.count += 1;
        state.last_attempt = Instant::now();
    }

    fn drop_backend(&mut self, name: &str) {
        self.ssh_backends.remove(name);
        self.k8s_backends.remove(name);
    }

    fn cleanup_removed_backends(&mut self, servers: &[ServerConfig]) {
        let active_names: Vec<&str> = servers.iter().map(ServerConfig::name).collect();

        self.ssh_backends
            .retain(|name, _| active_names.contains(&name.as_str()));
        self.k8s_backends
            .retain(|name, _| active_names.contains(&name.as_str()));
        self.failures
            .retain(|name, _| active_names.contains(&name.as_str()));
    }
}

fn offline_metrics(name: &str, server_type: &str) -> ServerMetrics {
    ServerMetrics {
        server_name: name.to_string(),
        server_type: server_type.to_string(),
        status: ServerStatus::Offline,
        ..ServerMetrics::default()
    }
}

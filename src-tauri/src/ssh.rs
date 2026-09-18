//! SSH remote metrics collection.

use std::sync::Arc;
use std::time::{Duration, Instant};

use russh::client;
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::load_secret_key;
use russh::{Channel, ChannelMsg, Disconnect};

use crate::metrics::{ServerMetrics, ServerStatus};

mod error;
mod host_key;
mod parse;

pub(crate) use error::SshError;

use error::MetricsParseError;
use host_key::SshHandler;
use parse::{parse_cpu, parse_disk, parse_memory, parse_network};

const SEPARATOR: &str = "---SEPARATOR---";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(25);

/// Hard ceiling on bytes buffered from a single SSH command. The metrics
/// command emits a few KB; this cap bounds memory if a compromised or
/// misbehaving server streams unbounded output within the command
/// timeout (boundary-validation, axiom `rust_api_axiom_25`).
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Bracket an unbracketed IPv6 literal for OpenSSH/`russh` address forms.
#[must_use]
pub(crate) fn ssh_cli_host(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// Format `host:port` for russh. IPv6 literals must be bracketed.
fn ssh_connect_addr(host: &str, port: u16) -> String {
    format!("{}:{port}", ssh_cli_host(host))
}

const METRICS_COMMAND: &str = "\
    top -bn1 | head -5; \
    echo '---SEPARATOR---'; \
    free -b; \
    echo '---SEPARATOR---'; \
    df -B1 /; \
    echo '---SEPARATOR---'; \
    cat /proc/net/dev";
/// SSH backend that collects metrics from a single remote server.
pub(crate) struct SshBackend {
    session: Option<client::Handle<SshHandler>>,
    host: String,
    port: u16,
    user: String,
    key_path: String,
    prev_net_bytes: Option<(u64, u64)>,
    prev_poll_time: Option<Instant>,
}
impl SshBackend {
    pub(crate) fn new(host: String, port: u16, user: String, key_path: String) -> Self {
        Self {
            session: None,
            host,
            port,
            user,
            key_path,
            prev_net_bytes: None,
            prev_poll_time: None,
        }
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.session.is_some()
    }

    pub(crate) fn matches_config(&self, host: &str, port: u16, user: &str, key_path: &str) -> bool {
        self.host == host && self.port == port && self.user == user && self.key_path == key_path
    }

    /// Establish an SSH connection and authenticate with a key.
    ///
    /// # Errors
    ///
    /// Returns [`SshError`] if the private key cannot be loaded, the TCP
    /// connection or SSH handshake fails, or authentication is rejected.
    pub(crate) async fn connect(&mut self) -> Result<(), SshError> {
        let key_path = crate::config::expand_tilde(&self.key_path);
        let key = tokio::task::spawn_blocking(move || load_secret_key(&key_path, None))
            .await
            .map_err(|_| SshError::KeyLoadCancelled)?
            .map_err(|source| SshError::LoadKey {
                path: self.key_path.clone(),
                source,
            })?;

        let config = Arc::new(client::Config::default());
        let addr = ssh_connect_addr(&self.host, self.port);

        let handler = SshHandler {
            host: self.host.clone(),
            port: self.port,
        };

        let mut handle = client::connect(config, &addr, handler)
            .await
            .map_err(|source| SshError::Connect { addr, source })?;

        let key_with_alg = PrivateKeyWithHashAlg::new(Arc::new(key), None);

        let auth_result = handle
            .authenticate_publickey(&self.user, key_with_alg)
            .await
            .map_err(SshError::Auth)?;

        if !auth_result.success() {
            return Err(SshError::AuthRejected {
                user: self.user.clone(),
            });
        }

        self.session = Some(handle);
        Ok(())
    }

    /// Collect CPU, memory, disk, and network metrics.
    #[expect(
        clippy::cast_precision_loss,
        reason = "byte deltas fit comfortably in f64 \
                  mantissa for rate calculation"
    )]
    pub(crate) async fn collect_metrics(
        &mut self,
        server_name: &str,
    ) -> Result<ServerMetrics, SshError> {
        let output = self.exec_command(METRICS_COMMAND).await?;
        let sections: Vec<&str> = output.split(SEPARATOR).collect();

        if sections.len() < 4 {
            return Err(SshError::Parse(MetricsParseError::SectionCount {
                expected: 4,
                got: sections.len(),
            }));
        }

        let cpu = parse_cpu(sections[0])?;
        let memory = parse_memory(sections[1])?;
        let disk = parse_disk(sections[2])?;
        let (rx_bytes, tx_bytes) = parse_network(sections[3])?;

        let now = Instant::now();
        let (rx_per_sec, tx_per_sec) = match (self.prev_net_bytes, self.prev_poll_time) {
            (Some((prev_rx, prev_tx)), Some(prev_time)) => {
                let elapsed = now.duration_since(prev_time).as_secs_f64();
                if elapsed > 0.0 {
                    let rx_rate = rx_bytes.saturating_sub(prev_rx) as f64 / elapsed;
                    let tx_rate = tx_bytes.saturating_sub(prev_tx) as f64 / elapsed;
                    #[expect(
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss,
                        reason = "rates from byte deltas are \
                                  always small positive"
                    )]
                    (rx_rate as u64, tx_rate as u64)
                } else {
                    (0, 0)
                }
            }
            (Some(_) | None, None) | (None, Some(_)) => (0, 0),
        };

        self.prev_net_bytes = Some((rx_bytes, tx_bytes));
        self.prev_poll_time = Some(now);

        Ok(ServerMetrics {
            server_name: server_name.to_string(),
            server_type: "ssh".to_string(),
            status: ServerStatus::Online,
            cpu_percent: cpu,
            memory_percent: memory,
            disk_percent: disk,
            net_rx_bytes_per_sec: rx_per_sec,
            net_tx_bytes_per_sec: tx_per_sec,
            ..ServerMetrics::default()
        })
    }

    /// Execute a command over SSH and return stdout.
    ///
    /// Applies an internal per-command timeout and explicitly
    /// closes the channel to prevent resource leaks.
    async fn exec_command(&self, cmd: &str) -> Result<String, SshError> {
        let session = self.session.as_ref().ok_or(SshError::NotConnected)?;

        let mut channel: Channel<client::Msg> = session
            .channel_open_session()
            .await
            .map_err(SshError::OpenChannel)?;

        channel.exec(true, cmd).await.map_err(SshError::Exec)?;

        let result = read_channel_output(&mut channel).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), channel.close()).await;
        result
    }

    /// Close the SSH session.
    pub(crate) async fn disconnect(&mut self) {
        if let Some(session) = self.session.take() {
            let _ = session
                .disconnect(Disconnect::ByApplication, "closing", "")
                .await;
        }
    }
}

/// Read all stdout data from an SSH channel with a timeout.
///
/// Returns [`SshError::Timeout`] if the channel does not send EOF/Close
/// within [`COMMAND_TIMEOUT`], or [`SshError::OutputTooLarge`] if the
/// server streams more than [`MAX_OUTPUT_BYTES`]. The size check runs
/// before each append so a hostile server cannot grow the buffer past
/// the cap between the check and the copy.
async fn read_channel_output(channel: &mut Channel<client::Msg>) -> Result<String, SshError> {
    let deadline = tokio::time::Instant::now() + COMMAND_TIMEOUT;
    let mut stdout = Vec::new();

    loop {
        match tokio::time::timeout_at(deadline, channel.wait()).await {
            Ok(Some(ChannelMsg::Data { data })) => {
                if stdout.len().saturating_add(data.len()) > MAX_OUTPUT_BYTES {
                    return Err(SshError::OutputTooLarge {
                        limit: MAX_OUTPUT_BYTES,
                    });
                }
                stdout.extend_from_slice(&data);
            }
            Ok(Some(ChannelMsg::Eof | ChannelMsg::Close) | None) => break,
            Ok(Some(_)) => {}
            Err(_) => {
                return Err(SshError::Timeout);
            }
        }
    }

    String::from_utf8(stdout).map_err(SshError::NonUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_connect_addr_ipv4_and_hostname() {
        assert_eq!(ssh_connect_addr("10.0.0.5", 22), "10.0.0.5:22");
        assert_eq!(ssh_connect_addr("box.local", 2222), "box.local:2222");
    }

    #[test]
    fn ssh_connect_addr_brackets_ipv6() {
        assert_eq!(ssh_connect_addr("2001:db8::1", 22), "[2001:db8::1]:22");
        assert_eq!(ssh_connect_addr("[::1]", 22), "[::1]:22");
    }
}

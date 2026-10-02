//! SSH remote metrics collection.

use std::sync::Arc;
use std::time::{Duration, Instant};

use russh::client;
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::load_secret_key;
use russh::{Channel, ChannelMsg, Disconnect};

use crate::config::{SshTarget, expand_tilde};
use crate::metrics::{HostMetrics, NetSample, Usage};

mod error;
mod host_key;
mod parse;

pub(crate) use error::SshError;

use error::MetricsParseError;
use host_key::SshHandler;
use parse::{parse_cpu, parse_disk, parse_memory, parse_network};

const SEPARATOR: &str = "---SEPARATOR---";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(25);

/// How long a channel close may take before it is abandoned.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Hard ceiling on bytes buffered from a single SSH command. The metrics
/// command emits a few KB; this cap bounds memory if a compromised or
/// misbehaving server streams unbounded output within the command
/// timeout.
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

/// Run on the remote host; the sections are split on [`SEPARATOR`].
const METRICS_COMMAND: &str = "\
    top -bn1 | head -5; \
    echo '---SEPARATOR---'; \
    free -b; \
    echo '---SEPARATOR---'; \
    df -P -B1 /; \
    echo '---SEPARATOR---'; \
    cat /proc/net/dev";

/// SSH backend that collects metrics from a single remote server, keeping
/// the session open between polls.
pub(crate) struct SshBackend {
    target: SshTarget,
    session: Option<client::Handle<SshHandler>>,
    prev_net: Option<NetSample>,
}

impl SshBackend {
    pub(crate) fn new(target: SshTarget) -> Self {
        Self {
            target,
            session: None,
            prev_net: None,
        }
    }

    pub(crate) fn target(&self) -> &SshTarget {
        &self.target
    }

    /// Collect metrics, connecting first if needed. Any failure drops the
    /// session so the next poll reconnects from scratch.
    ///
    /// # Errors
    ///
    /// [`SshError`] if connecting, running the command, or parsing its
    /// output fails.
    pub(crate) async fn collect(&mut self) -> Result<HostMetrics, SshError> {
        let result = self.try_collect().await;
        if result.is_err() {
            self.disconnect().await;
        }
        result
    }

    async fn try_collect(&mut self) -> Result<HostMetrics, SshError> {
        if self.session.is_none() {
            self.session = Some(self.connect().await?);
        }
        let output = self.exec_command(METRICS_COMMAND).await?;
        let sample = parse_metrics_output(&output)?;

        let now = NetSample {
            rx_bytes: sample.rx_bytes,
            tx_bytes: sample.tx_bytes,
            at: Instant::now(),
        };
        let net = self.prev_net.and_then(|prev| now.rate_since(&prev));
        self.prev_net = Some(now);

        Ok(HostMetrics {
            usage: sample.usage,
            net,
        })
    }

    /// Establish an SSH connection and authenticate with a key.
    async fn connect(&self) -> Result<client::Handle<SshHandler>, SshError> {
        let target = &self.target;
        let key_path = expand_tilde(&target.key_path);
        let load_path = key_path.clone();
        let key = tokio::task::spawn_blocking(move || load_secret_key(&load_path, None))
            .await
            .map_err(SshError::KeyLoadTask)?
            .map_err(|source| SshError::LoadKey {
                path: key_path,
                source,
            })?;

        let config = Arc::new(client::Config::default());
        let addr = ssh_connect_addr(&target.host, target.port);
        let handler = SshHandler::new(&target.host, target.port);
        let mut handle = client::connect(config, &addr, handler)
            .await
            .map_err(|source| SshError::Connect { addr, source })?;

        let key_with_alg = PrivateKeyWithHashAlg::new(Arc::new(key), None);
        let auth_result = handle
            .authenticate_publickey(&target.user, key_with_alg)
            .await
            .map_err(SshError::Auth)?;
        if !auth_result.success() {
            return Err(SshError::AuthRejected {
                user: target.user.clone(),
            });
        }

        Ok(handle)
    }

    /// Execute a command over SSH and return stdout. The channel is
    /// always closed afterwards so it cannot leak.
    async fn exec_command(&self, cmd: &str) -> Result<String, SshError> {
        let session = self.session.as_ref().ok_or(SshError::NotConnected)?;
        let mut channel: Channel<client::Msg> = session
            .channel_open_session()
            .await
            .map_err(SshError::OpenChannel)?;
        channel.exec(true, cmd).await.map_err(SshError::Exec)?;

        let result = read_channel_output(&mut channel).await;
        match tokio::time::timeout(CLOSE_TIMEOUT, channel.close()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::debug!("closing SSH channel failed: {e}"),
            Err(_) => tracing::debug!("closing SSH channel timed out"),
        }
        result
    }

    async fn disconnect(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        // Bounded like the channel close: a dead peer must not eat the time
        // the poller allows for the whole collection.
        let disconnect = session.disconnect(Disconnect::ByApplication, "closing", "");
        match tokio::time::timeout(CLOSE_TIMEOUT, disconnect).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::debug!("SSH disconnect from {} failed: {e}", self.target.host),
            Err(_) => tracing::debug!("SSH disconnect from {} timed out", self.target.host),
        }
    }
}

/// Parsed output of [`METRICS_COMMAND`]: usage plus the cumulative
/// network counters a rate is computed from.
#[derive(Debug, PartialEq)]
struct MetricsSample {
    usage: Usage,
    rx_bytes: u64,
    tx_bytes: u64,
}

fn parse_metrics_output(output: &str) -> Result<MetricsSample, MetricsParseError> {
    let sections: Vec<&str> = output.split(SEPARATOR).collect();
    let [top, free, df, net, ..] = sections[..] else {
        return Err(MetricsParseError::SectionCount {
            expected: 4,
            got: sections.len(),
        });
    };

    let (rx_bytes, tx_bytes) = parse_network(net)?;
    Ok(MetricsSample {
        usage: Usage {
            cpu_percent: parse_cpu(top)?,
            memory_percent: parse_memory(free)?,
            disk_percent: parse_disk(df)?,
        },
        rx_bytes,
        tx_bytes,
    })
}

/// Read all stdout data from an SSH channel with a timeout.
///
/// Returns [`SshError::Timeout`] if the channel does not send EOF/Close
/// within [`COMMAND_TIMEOUT`], or [`SshError::OutputTooLarge`] if the
/// server streams more than [`MAX_OUTPUT_BYTES`]. The size check runs
/// before each append so the buffer never grows past the cap.
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
            Err(_) => return Err(SshError::Timeout),
        }
    }

    String::from_utf8(stdout).map_err(SshError::NonUtf8)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
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

    #[test]
    fn parses_full_metrics_output() {
        let output = format!(
            "%Cpu(s):  3.0 us,  1.0 sy,  0.0 ni, 96.0 id\n{SEPARATOR}\n\
             total used free\nMem: 1000 250 750\n{SEPARATOR}\n\
             Filesystem 1-blocks Used Available Capacity Mounted\n\
             /dev/sda1 100 42 58 42% /\n{SEPARATOR}\n\
             Inter-|   Receive\n face |bytes packets\n\
             eth0: 100 1 0 0 0 0 0 0 200 2 0 0 0 0 0 0\n"
        );

        let sample = parse_metrics_output(&output).expect("parse");

        assert!((sample.usage.cpu_percent - 4.0).abs() < 1e-9);
        assert!((sample.usage.memory_percent - 25.0).abs() < 1e-9);
        assert!((sample.usage.disk_percent - 42.0).abs() < 1e-9);
        assert_eq!((sample.rx_bytes, sample.tx_bytes), (100, 200));
    }

    #[test]
    fn missing_sections_are_reported() {
        let err = parse_metrics_output("only one section").expect_err("too few sections");

        assert!(matches!(
            err,
            MetricsParseError::SectionCount {
                expected: 4,
                got: 1
            }
        ));
    }
}

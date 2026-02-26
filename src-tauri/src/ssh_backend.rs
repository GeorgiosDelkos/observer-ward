use std::sync::Arc;
use std::time::{Duration, Instant};

use russh::client;
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::load_secret_key;
use russh::{Channel, ChannelMsg, Disconnect};

use crate::metrics::{ServerMetrics, ServerStatus};

const SEPARATOR: &str = "---SEPARATOR---";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(25);

const METRICS_COMMAND: &str = "\
    top -bn1 | head -5; \
    echo '---SEPARATOR---'; \
    free -b; \
    echo '---SEPARATOR---'; \
    df -B1 /; \
    echo '---SEPARATOR---'; \
    cat /proc/net/dev";

/// SSH backend that collects metrics from a single remote server.
pub struct SshBackend {
    session: Option<client::Handle<SshHandler>>,
    host: String,
    port: u16,
    user: String,
    key_path: String,
    prev_net_bytes: Option<(u64, u64)>,
    prev_poll_time: Option<Instant>,
}

struct SshHandler {
    host: String,
    port: u16,
}

impl client::Handler for SshHandler {
    type Error = russh::Error;

    /// Verify the server's host key using TOFU (Trust On First
    /// Use). Known keys are checked against `~/.ssh/known_hosts`.
    /// Unknown hosts are learned automatically on first
    /// connection; changed keys are rejected.
    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        use russh::keys::known_hosts::{check_known_hosts, learn_known_hosts};

        match check_known_hosts(&self.host, self.port, server_public_key) {
            Ok(true) => Ok(true),
            Ok(false) => {
                tracing::warn!("host key not recognized for {}:{}", self.host, self.port);
                Ok(false)
            }
            Err(russh::keys::Error::KeyChanged { line }) => {
                tracing::error!(
                    "HOST KEY CHANGED for {}:{} \
                     (known_hosts line {line})",
                    self.host,
                    self.port
                );
                Ok(false)
            }
            Err(e) => {
                tracing::info!(
                    "no known_hosts entry for {}:{} ({e}), \
                     learning key (TOFU)",
                    self.host,
                    self.port
                );
                if let Err(e) = learn_known_hosts(&self.host, self.port, server_public_key) {
                    tracing::warn!(
                        "failed to save host key for {}:{}: {e}",
                        self.host,
                        self.port
                    );
                }
                Ok(true)
            }
        }
    }
}

impl SshBackend {
    pub fn new(host: String, port: u16, user: String, key_path: String) -> Self {
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

    pub fn is_connected(&self) -> bool {
        self.session.is_some()
    }

    pub fn matches_config(&self, host: &str, port: u16, user: &str, key_path: &str) -> bool {
        self.host == host && self.port == port && self.user == user && self.key_path == key_path
    }

    /// Establish an SSH connection and authenticate with a key.
    pub async fn connect(&mut self) -> Result<(), String> {
        let key = load_secret_key(&self.key_path, None)
            .map_err(|e| format!("failed to load SSH key '{}': {e}", self.key_path))?;

        let config = Arc::new(client::Config::default());
        let addr = format!("{}:{}", self.host, self.port);

        let handler = SshHandler {
            host: self.host.clone(),
            port: self.port,
        };

        let mut handle = client::connect(config, &addr, handler)
            .await
            .map_err(|e| format!("SSH connection to {addr} failed: {e}"))?;

        let key_with_alg = PrivateKeyWithHashAlg::new(Arc::new(key), None);

        let auth_result = handle
            .authenticate_publickey(&self.user, key_with_alg)
            .await
            .map_err(|e| format!("SSH authentication failed: {e}"))?;

        if !auth_result.success() {
            return Err(format!(
                "SSH authentication rejected for user '{}'",
                self.user
            ));
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
    pub async fn collect_metrics(&mut self, server_name: &str) -> Result<ServerMetrics, String> {
        let output = self.exec_command(METRICS_COMMAND).await?;
        let sections: Vec<&str> = output.split(SEPARATOR).collect();

        if sections.len() < 4 {
            return Err(format!(
                "expected 4 sections in metrics output, \
                 got {}",
                sections.len()
            ));
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
            _ => (0, 0),
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
    async fn exec_command(&self, cmd: &str) -> Result<String, String> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| "SSH session not connected".to_string())?;

        let mut channel: Channel<client::Msg> = session
            .channel_open_session()
            .await
            .map_err(|e| format!("failed to open channel: {e}"))?;

        channel
            .exec(true, cmd)
            .await
            .map_err(|e| format!("failed to exec command: {e}"))?;

        let result = read_channel_output(&mut channel).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), channel.close()).await;
        result
    }

    /// Close the SSH session.
    pub async fn disconnect(&mut self) {
        if let Some(session) = self.session.take() {
            let _ = session
                .disconnect(Disconnect::ByApplication, "closing", "")
                .await;
        }
    }
}

/// Read all stdout data from an SSH channel with a timeout.
///
/// Returns an error if the channel does not send EOF/Close
/// within [`COMMAND_TIMEOUT`].
async fn read_channel_output(channel: &mut Channel<client::Msg>) -> Result<String, String> {
    let deadline = tokio::time::Instant::now() + COMMAND_TIMEOUT;
    let mut stdout = Vec::new();

    loop {
        match tokio::time::timeout_at(deadline, channel.wait()).await {
            Ok(Some(ChannelMsg::Data { data })) => {
                stdout.extend_from_slice(&data);
            }
            Ok(Some(ChannelMsg::Eof | ChannelMsg::Close) | None) => break,
            Ok(Some(_)) => {}
            Err(_) => {
                return Err("SSH command timed out".to_string());
            }
        }
    }

    String::from_utf8(stdout).map_err(|e| format!("non-UTF-8 command output: {e}"))
}

/// Extract CPU usage from `top -bn1` output.
///
/// Looks for the `%Cpu(s):` line and computes 100 - idle%.
fn parse_cpu(top_output: &str) -> Result<f64, String> {
    for line in top_output.lines() {
        let trimmed = line.trim();
        if !trimmed.contains("Cpu") {
            continue;
        }

        for part in trimmed.split(',') {
            let part = part.trim();
            if part.contains("id") {
                let idle: f64 = part
                    .split_whitespace()
                    .next()
                    .ok_or_else(|| "no idle value in Cpu line".to_string())?
                    .parse()
                    .map_err(|e| format!("failed to parse CPU idle: {e}"))?;
                return Ok(100.0 - idle);
            }
        }

        return Err("Cpu line found but no idle value".to_string());
    }

    Err("no Cpu line found in top output".to_string())
}

/// Extract memory usage percentage from `free -b` output.
///
/// Parses the "Mem:" line, taking total (col 1) and used
/// (col 2).
fn parse_memory(free_output: &str) -> Result<f64, String> {
    for line in free_output.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("Mem:") {
            continue;
        }

        let cols: Vec<&str> = trimmed.split_whitespace().collect();
        if cols.len() < 3 {
            return Err("Mem line has too few columns".to_string());
        }

        let total: f64 = cols[1]
            .parse()
            .map_err(|e| format!("failed to parse memory total: {e}"))?;
        let used: f64 = cols[2]
            .parse()
            .map_err(|e| format!("failed to parse memory used: {e}"))?;

        if total <= 0.0 {
            return Err("memory total is zero or negative".to_string());
        }

        return Ok(used / total * 100.0);
    }

    Err("no Mem line found in free output".to_string())
}

/// Extract disk usage percentage from `df -B1 /` output.
///
/// Parses the second line and extracts the `Use%` column.
fn parse_disk(df_output: &str) -> Result<f64, String> {
    let lines: Vec<&str> = df_output.lines().filter(|l| !l.trim().is_empty()).collect();

    if lines.len() < 2 {
        return Err("df output has fewer than 2 lines".to_string());
    }

    let data_line = lines[1];
    for part in data_line.split_whitespace() {
        if let Some(pct) = part.strip_suffix('%') {
            let val: f64 = pct
                .parse()
                .map_err(|e| format!("failed to parse disk percentage: {e}"))?;
            return Ok(val);
        }
    }

    Err("no percentage column found in df output".to_string())
}

/// Extract total (rx, tx) byte counts from
/// `/proc/net/dev`.
///
/// Sums `rx_bytes` (col 1) and `tx_bytes` (col 9) across
/// all non-loopback interfaces.
fn parse_network(proc_net_dev: &str) -> Result<(u64, u64), String> {
    let mut total_rx: u64 = 0;
    let mut total_tx: u64 = 0;
    let mut found_interface = false;

    for line in proc_net_dev.lines() {
        let trimmed = line.trim();
        let Some((iface, rest)) = trimmed.split_once(':') else {
            continue;
        };

        let iface = iface.trim();
        if iface == "lo" {
            continue;
        }

        let cols: Vec<&str> = rest.split_whitespace().collect();
        if cols.len() < 10 {
            continue;
        }

        let rx: u64 = cols[0]
            .parse()
            .map_err(|e| format!("failed to parse rx bytes for {iface}: {e}"))?;
        let tx: u64 = cols[8]
            .parse()
            .map_err(|e| format!("failed to parse tx bytes for {iface}: {e}"))?;

        total_rx = total_rx.saturating_add(rx);
        total_tx = total_tx.saturating_add(tx);
        found_interface = true;
    }

    if !found_interface {
        return Err("no non-loopback interfaces found in \
             /proc/net/dev"
            .to_string());
    }

    Ok((total_rx, total_tx))
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::*;

    fn assert_f64_near(left: f64, right: f64, epsilon: f64) {
        assert!(
            (left - right).abs() < epsilon,
            "expected ~{right}, got {left}"
        );
    }

    // --- CPU parsing ---

    #[test]
    fn parse_cpu_typical_output() {
        let input = "\
top - 14:30:01 up 42 days,  3:22,  1 user,  load average: 0.15, 0.10, 0.05
Tasks: 195 total,   1 running, 194 sleeping,   0 stopped,   0 zombie
%Cpu(s):  3.2 us,  1.0 sy,  0.0 ni, 95.3 id,  0.5 wa,  0.0 hi,  0.0 si,  0.0 st
MiB Mem :  15991.7 total,   4056.3 free,   8127.5 used,   3807.9 buff/cache
MiB Swap:   2048.0 total,   2048.0 free,      0.0 used.   7864.2 avail Mem";

        let cpu = parse_cpu(input).expect("parse_cpu");
        assert_f64_near(cpu, 4.7, 0.01);
    }

    #[test]
    fn parse_cpu_high_usage() {
        let input = "%Cpu(s): 45.0 us, 30.0 sy,  0.0 ni, \
                      5.0 id,  0.0 wa, 10.0 hi, 10.0 si, \
                      0.0 st";
        let cpu = parse_cpu(input).expect("parse_cpu");
        assert_f64_near(cpu, 95.0, 0.01);
    }

    #[test]
    fn parse_cpu_zero_idle() {
        let input = "%Cpu(s): 50.0 us, 50.0 sy,  0.0 ni, \
                      0.0 id,  0.0 wa,  0.0 hi,  0.0 si, \
                      0.0 st";
        let cpu = parse_cpu(input).expect("parse_cpu");
        assert_f64_near(cpu, 100.0, 0.01);
    }

    #[test]
    fn parse_cpu_full_idle() {
        let input = "%Cpu(s):  0.0 us,  0.0 sy,  0.0 ni,\
                     100.0 id,  0.0 wa,  0.0 hi,  0.0 si,\
                       0.0 st";
        let cpu = parse_cpu(input).expect("parse_cpu");
        assert_f64_near(cpu, 0.0, 0.01);
    }

    #[test]
    fn parse_cpu_no_cpu_line() {
        let input = "some random output\nno cpu here\n";
        let err = parse_cpu(input).unwrap_err();
        assert!(err.contains("no Cpu line"), "{err}");
    }

    // --- Memory parsing ---

    #[test]
    fn parse_memory_typical_output() {
        let input = "\
              total        used        free      shared  buff/cache   available
Mem:    16777216000  8388608000  4194304000   104857600  4194304000  8388608000
Swap:    2147483648           0  2147483648";

        let mem = parse_memory(input).expect("parse_memory");
        assert_f64_near(mem, 50.0, 0.01);
    }

    #[test]
    fn parse_memory_high_usage() {
        let input = "Mem:    16000000000  15200000000  \
                      800000000   0  0  800000000";
        let mem = parse_memory(input).expect("parse_memory");
        assert_f64_near(mem, 95.0, 0.01);
    }

    #[test]
    fn parse_memory_low_usage() {
        let input = "Mem:    16000000000   1600000000  \
                     14400000000   0  0  14400000000";
        let mem = parse_memory(input).expect("parse_memory");
        assert_f64_near(mem, 10.0, 0.01);
    }

    #[test]
    fn parse_memory_no_mem_line() {
        let input = "Swap:    2147483648   0   2147483648";
        let err = parse_memory(input).unwrap_err();
        assert!(err.contains("no Mem line"), "{err}");
    }

    #[test]
    fn parse_memory_too_few_columns() {
        let input = "Mem: 1000";
        let err = parse_memory(input).unwrap_err();
        assert!(err.contains("too few columns"), "{err}");
    }

    // --- Disk parsing ---

    #[test]
    fn parse_disk_typical_output() {
        let input = "\
Filesystem     1B-blocks       Used  Available Use% Mounted on
/dev/sda1    107374182400 64424509440 42949672960  60% /";

        let disk = parse_disk(input).expect("parse_disk");
        assert_f64_near(disk, 60.0, 0.01);
    }

    #[test]
    fn parse_disk_nearly_full() {
        let input = "\
Filesystem     1B-blocks       Used  Available Use% Mounted on
/dev/nvme0n1p2 500107862016 475102268416 25005593600  96% /";

        let disk = parse_disk(input).expect("parse_disk");
        assert_f64_near(disk, 96.0, 0.01);
    }

    #[test]
    fn parse_disk_one_percent() {
        let input = "\
Filesystem     1B-blocks       Used  Available Use% Mounted on
/dev/sda1    107374182400  1073741824 106300440576   1% /";

        let disk = parse_disk(input).expect("parse_disk");
        assert_f64_near(disk, 1.0, 0.01);
    }

    #[test]
    fn parse_disk_too_few_lines() {
        let input = "Filesystem     1B-blocks";
        let err = parse_disk(input).unwrap_err();
        assert!(err.contains("fewer than 2"), "{err}");
    }

    #[test]
    fn parse_disk_no_percentage() {
        let input = "\
Filesystem     1B-blocks       Used  Available
/dev/sda1    107374182400 64424509440 42949672960";

        let err = parse_disk(input).unwrap_err();
        assert!(err.contains("no percentage"), "{err}");
    }

    // --- Network parsing ---

    #[test]
    fn parse_network_typical_output() {
        let input = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:  123456    1000    0    0    0     0          0         0   123456    1000    0    0    0     0       0          0
  eth0: 9876543   50000    0    0    0     0          0         0  1234567   30000    0    0    0     0       0          0";

        let (rx, tx) = parse_network(input).expect("parse_network");
        assert_eq!(rx, 9_876_543);
        assert_eq!(tx, 1_234_567);
    }

    #[test]
    fn parse_network_multiple_interfaces() {
        let input = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:  500000    5000    0    0    0     0          0         0   500000    5000    0    0    0     0       0          0
  eth0: 1000000   10000    0    0    0     0          0         0   200000    5000    0    0    0     0       0          0
 wlan0: 3000000   20000    0    0    0     0          0         0   800000   10000    0    0    0     0       0          0";

        let (rx, tx) = parse_network(input).expect("parse_network");
        assert_eq!(rx, 4_000_000);
        assert_eq!(tx, 1_000_000);
    }

    #[test]
    fn parse_network_only_loopback() {
        let input = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:  123456    1000    0    0    0     0          0         0   123456    1000    0    0    0     0       0          0";

        let err = parse_network(input).unwrap_err();
        assert!(err.contains("no non-loopback"), "{err}");
    }

    #[test]
    fn parse_network_no_interfaces_at_all() {
        let input = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed";

        let err = parse_network(input).unwrap_err();
        assert!(err.contains("no non-loopback"), "{err}");
    }

    #[test]
    fn parse_network_docker_and_veth() {
        let input = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:       0       0    0    0    0     0          0         0        0       0    0    0    0     0       0          0
  eth0: 5000000   40000    0    0    0     0          0         0  2000000   20000    0    0    0     0       0          0
docker0: 100000    1000    0    0    0     0          0         0    50000     500    0    0    0     0       0          0
veth123: 100000    1000    0    0    0     0          0         0    50000     500    0    0    0     0       0          0";

        let (rx, tx) = parse_network(input).expect("parse_network");
        assert_eq!(rx, 5_200_000);
        assert_eq!(tx, 2_100_000);
    }

    #[test]
    fn parse_network_large_counters() {
        let input = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:       0       0    0    0    0     0          0         0        0       0    0    0    0     0       0          0
  eth0: 18446744073709551000 99999 0 0 0 0 0 0 18446744073709551000 99999 0 0 0 0 0 0";

        let (rx, tx) = parse_network(input).expect("parse_network");
        assert_eq!(rx, 18_446_744_073_709_551_000);
        assert_eq!(tx, 18_446_744_073_709_551_000);
    }
}

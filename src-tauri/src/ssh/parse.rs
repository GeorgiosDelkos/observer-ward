//! Parsers for `top` / `free` / `df` / `/proc/net/dev` command output.

use super::error::MetricsParseError;

/// Extract CPU usage from `top -bn1` output.
///
/// Looks for the `%Cpu(s):` line and computes 100 - idle%.
pub(super) fn parse_cpu(top_output: &str) -> Result<f64, MetricsParseError> {
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
                    .ok_or(MetricsParseError::NoCpuIdle)?
                    .parse()
                    .map_err(MetricsParseError::CpuIdle)?;
                return Ok(100.0 - idle);
            }
        }

        return Err(MetricsParseError::NoCpuIdle);
    }

    Err(MetricsParseError::NoCpuLine)
}

/// Extract memory usage percentage from `free -b` output.
///
/// Parses the "Mem:" line, taking total (col 1) and used
/// (col 2).
pub(super) fn parse_memory(free_output: &str) -> Result<f64, MetricsParseError> {
    for line in free_output.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("Mem:") {
            continue;
        }

        let cols: Vec<&str> = trimmed.split_whitespace().collect();
        if cols.len() < 3 {
            return Err(MetricsParseError::MemColumns);
        }

        let total: f64 = cols[1]
            .parse()
            .map_err(|source| MetricsParseError::MemValue {
                what: "total",
                source,
            })?;
        let used: f64 = cols[2]
            .parse()
            .map_err(|source| MetricsParseError::MemValue {
                what: "used",
                source,
            })?;

        if total <= 0.0 {
            return Err(MetricsParseError::MemTotalZero);
        }

        return Ok(used / total * 100.0);
    }

    Err(MetricsParseError::NoMemLine)
}

/// Extract disk usage percentage from `df -B1 /` output.
///
/// Parses the second line and extracts the `Use%` column.
pub(super) fn parse_disk(df_output: &str) -> Result<f64, MetricsParseError> {
    let lines: Vec<&str> = df_output.lines().filter(|l| !l.trim().is_empty()).collect();

    if lines.len() < 2 {
        return Err(MetricsParseError::DiskLines);
    }

    let data_line = lines[1];
    for part in data_line.split_whitespace() {
        if let Some(pct) = part.strip_suffix('%') {
            let val: f64 = pct.parse().map_err(MetricsParseError::DiskPercent)?;
            return Ok(val);
        }
    }

    Err(MetricsParseError::DiskNoPercent)
}

/// Extract total (rx, tx) byte counts from
/// `/proc/net/dev`.
///
/// Sums `rx_bytes` (col 1) and `tx_bytes` (col 9) across
/// all non-loopback interfaces.
pub(super) fn parse_network(proc_net_dev: &str) -> Result<(u64, u64), MetricsParseError> {
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
            .map_err(|source| MetricsParseError::NetValue {
                what: "rx",
                iface: iface.to_string(),
                source,
            })?;
        let tx: u64 = cols[8]
            .parse()
            .map_err(|source| MetricsParseError::NetValue {
                what: "tx",
                iface: iface.to_string(),
                source,
            })?;

        total_rx = total_rx.saturating_add(rx);
        total_tx = total_tx.saturating_add(tx);
        found_interface = true;
    }

    if !found_interface {
        return Err(MetricsParseError::NoInterfaces);
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
        assert!(matches!(err, MetricsParseError::NoCpuLine), "{err:?}");
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
        assert!(matches!(err, MetricsParseError::NoMemLine), "{err:?}");
    }

    #[test]
    fn parse_memory_too_few_columns() {
        let input = "Mem: 1000";
        let err = parse_memory(input).unwrap_err();
        assert!(matches!(err, MetricsParseError::MemColumns), "{err:?}");
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
        assert!(matches!(err, MetricsParseError::DiskLines), "{err:?}");
    }

    #[test]
    fn parse_disk_no_percentage() {
        let input = "\
Filesystem     1B-blocks       Used  Available
/dev/sda1    107374182400 64424509440 42949672960";

        let err = parse_disk(input).unwrap_err();
        assert!(matches!(err, MetricsParseError::DiskNoPercent), "{err:?}");
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
        assert!(matches!(err, MetricsParseError::NoInterfaces), "{err:?}");
    }

    #[test]
    fn parse_network_no_interfaces_at_all() {
        let input = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed";

        let err = parse_network(input).unwrap_err();
        assert!(matches!(err, MetricsParseError::NoInterfaces), "{err:?}");
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

    use proptest::prelude::*;

    proptest! {
        // A compromised server can return anything between the
        // separators; the parsers must never panic on arbitrary input.
        // The strategy includes newlines (`[\s\S]`, not `.`) so the
        // line-oriented parse paths past the early guards are exercised.
        #[test]
        fn parsers_never_panic(s in "[\\s\\S]{0,256}") {
            let _ = parse_cpu(&s);
            let _ = parse_memory(&s);
            let _ = parse_disk(&s);
            let _ = parse_network(&s);
        }
    }
}

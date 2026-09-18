//! SSH backend error types.

/// Failure categories for the SSH backend. Each variant preserves its
/// underlying cause in the source chain (axiom `rust_quality_57`); the
/// poller flattens the chain only when logging.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum SshError {
    #[error("failed to load SSH key {path}")]
    LoadKey {
        path: String,
        #[source]
        source: russh::keys::Error,
    },
    #[error("SSH key loading was cancelled")]
    KeyLoadCancelled,
    #[error("SSH connection to {addr} failed")]
    Connect {
        addr: String,
        #[source]
        source: russh::Error,
    },
    #[error("SSH authentication failed")]
    Auth(#[source] russh::Error),
    #[error("SSH authentication rejected for user {user}")]
    AuthRejected { user: String },
    #[error("SSH session is not connected")]
    NotConnected,
    #[error("failed to open SSH channel")]
    OpenChannel(#[source] russh::Error),
    #[error("failed to execute SSH command")]
    Exec(#[source] russh::Error),
    #[error("SSH command timed out")]
    Timeout,
    #[error("SSH command output exceeded {limit} bytes")]
    OutputTooLarge { limit: usize },
    #[error("SSH command output was not valid UTF-8")]
    NonUtf8(#[source] std::string::FromUtf8Error),
    #[error(transparent)]
    Parse(#[from] MetricsParseError),
}

/// Failure categories for parsing the remote metrics command output.
/// Carries the offending field and underlying numeric-parse cause as
/// typed fields rather than a formatted string (axiom `rust_quality_63`).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum MetricsParseError {
    #[error("expected {expected} sections in metrics output, got {got}")]
    SectionCount { expected: usize, got: usize },
    #[error("no Cpu line found in top output")]
    NoCpuLine,
    #[error("Cpu line found but no idle value")]
    NoCpuIdle,
    #[error("failed to parse CPU idle value")]
    CpuIdle(#[source] std::num::ParseFloatError),
    #[error("no Mem line found in free output")]
    NoMemLine,
    #[error("Mem line has too few columns")]
    MemColumns,
    #[error("failed to parse memory {what} value")]
    MemValue {
        what: &'static str,
        #[source]
        source: std::num::ParseFloatError,
    },
    #[error("memory total is zero or negative")]
    MemTotalZero,
    #[error("df output has fewer than 2 lines")]
    DiskLines,
    #[error("no percentage column found in df output")]
    DiskNoPercent,
    #[error("failed to parse disk percentage")]
    DiskPercent(#[source] std::num::ParseFloatError),
    #[error("no non-loopback interfaces found in /proc/net/dev")]
    NoInterfaces,
    #[error("failed to parse {what} bytes for interface {iface}")]
    NetValue {
        what: &'static str,
        iface: String,
        #[source]
        source: std::num::ParseIntError,
    },
}

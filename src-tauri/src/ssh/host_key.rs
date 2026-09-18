//! SSH `known_hosts` TOFU decision table and russh handler.

use russh::client;

pub(super) struct SshHandler {
    pub(super) host: String,
    pub(super) port: u16,
}

/// Outcome of a `known_hosts` lookup, mapped from russh so the decision
/// table can be unit-tested without a live SSH handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostKeyCheck {
    Match,
    Changed {
        line: usize,
    },
    /// Presented key did not match any recorded key of the same type.
    /// russh returns this both for a truly unknown host *and* for a
    /// known host whose stored key is a different algorithm.
    NoMatch,
    LookupFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostKeyDecision {
    Accept,
    Reject,
    Learn,
}

/// TOFU policy: accept a matching recorded key; reject a same-type
/// change; learn only when no keys are recorded for the host at all.
///
/// russh's `check_known_hosts` returns `Ok(false)` for an unknown host
/// (empty / missing `known_hosts`, or host not listed). Treating that
/// as reject — the previous behaviour — made first-time connections
/// fail unless the user had already ssh'd from a terminal.
fn decide_host_key(check: HostKeyCheck, recorded_key_count: usize) -> HostKeyDecision {
    match check {
        HostKeyCheck::Match => HostKeyDecision::Accept,
        HostKeyCheck::Changed { .. } => HostKeyDecision::Reject,
        HostKeyCheck::NoMatch | HostKeyCheck::LookupFailed => {
            if recorded_key_count > 0 {
                HostKeyDecision::Reject
            } else {
                HostKeyDecision::Learn
            }
        }
    }
}
impl client::Handler for SshHandler {
    type Error = russh::Error;

    /// Verify the server's host key using TOFU (Trust On First
    /// Use). Known keys are checked against `~/.ssh/known_hosts`.
    /// Unknown hosts are learned automatically on first
    /// connection; changed keys are rejected.
    ///
    /// Security note: the first key seen for a host is trusted without
    /// out-of-band verification, so a man-in-the-middle present at the
    /// very first connection would be learned as legitimate. Every
    /// subsequent key change for that host is rejected.
    #[expect(
        clippy::unused_async_trait_impl,
        reason = "russh Handler requires async fn; known_hosts lookup is synchronous"
    )]
    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        use russh::keys::known_hosts::{check_known_hosts, known_host_keys, learn_known_hosts};

        let check = match check_known_hosts(&self.host, self.port, server_public_key) {
            Ok(true) => HostKeyCheck::Match,
            Ok(false) => HostKeyCheck::NoMatch,
            Err(russh::keys::Error::KeyChanged { line }) => HostKeyCheck::Changed { line },
            Err(_) => HostKeyCheck::LookupFailed,
        };
        let recorded_key_count =
            known_host_keys(&self.host, self.port).map_or(0, |keys| keys.len());

        match decide_host_key(check, recorded_key_count) {
            HostKeyDecision::Accept => Ok(true),
            HostKeyDecision::Reject => {
                if let HostKeyCheck::Changed { line } = check {
                    tracing::error!(
                        "HOST KEY CHANGED for {}:{} \
                         (known_hosts line {line})",
                        self.host,
                        self.port
                    );
                } else {
                    tracing::warn!("host key not recognized for {}:{}", self.host, self.port);
                }
                Ok(false)
            }
            HostKeyDecision::Learn => {
                tracing::info!(
                    "no known_hosts entry for {}:{}, \
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_host_is_learned() {
        assert_eq!(
            decide_host_key(HostKeyCheck::NoMatch, 0),
            HostKeyDecision::Learn
        );
    }

    #[test]
    fn lookup_failure_with_no_recorded_keys_is_learned() {
        assert_eq!(
            decide_host_key(HostKeyCheck::LookupFailed, 0),
            HostKeyDecision::Learn
        );
    }

    #[test]
    fn matching_recorded_key_is_accepted() {
        assert_eq!(
            decide_host_key(HostKeyCheck::Match, 1),
            HostKeyDecision::Accept
        );
    }

    #[test]
    fn same_type_key_change_is_rejected() {
        assert_eq!(
            decide_host_key(HostKeyCheck::Changed { line: 4 }, 1),
            HostKeyDecision::Reject
        );
    }

    #[test]
    fn known_host_with_unmatched_algorithm_is_rejected() {
        assert_eq!(
            decide_host_key(HostKeyCheck::NoMatch, 1),
            HostKeyDecision::Reject
        );
    }
}

//! SSH `known_hosts` TOFU decision table and russh handler.

use std::path::{Path, PathBuf};

use russh::client;
use russh::keys::PublicKey;
use russh::keys::known_hosts::{known_host_keys_path, learn_known_hosts_path};

pub(super) struct SshHandler {
    host: String,
    port: u16,
    known_hosts: Option<PathBuf>,
}

impl SshHandler {
    /// Verify against `~/.ssh/known_hosts`. `host` may be a bracketed IPv6
    /// literal; `known_hosts` stores it bare, so the brackets are dropped.
    pub(super) fn new(host: &str, port: u16) -> Self {
        Self {
            host: known_hosts_host(host).to_string(),
            port,
            known_hosts: dirs::home_dir().map(|home| home.join(".ssh").join("known_hosts")),
        }
    }
}

/// The host form `known_hosts` records: an IPv6 literal without brackets
/// (russh adds `[host]:port` itself for non-default ports).
fn known_hosts_host(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

/// Outcome of a `known_hosts` lookup for one presented key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostKeyCheck {
    /// A recorded key equals the presented one.
    Match,
    /// A recorded key of the same algorithm differs: the classic
    /// "REMOTE HOST IDENTIFICATION HAS CHANGED" case.
    Changed { line: usize },
    /// The host is known, but only under other key algorithms.
    NoMatch,
    /// The file is missing or lists nothing for this host.
    Unknown,
    /// The file exists but could not be read or parsed. Its contents are
    /// unknown, so this must never be treated like `Unknown`.
    LookupFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostKeyDecision {
    Accept,
    Reject,
    Learn,
}

/// TOFU policy: accept a recorded key, learn only a host with no recorded
/// keys at all, and reject everything else. Failing closed on
/// `LookupFailed` matters: an unreadable file or an entry russh cannot
/// parse would otherwise look like a first contact, and since learning
/// would not fix the unparseable entry, every later connection would
/// accept any key.
fn decide_host_key(check: HostKeyCheck) -> HostKeyDecision {
    match check {
        HostKeyCheck::Match => HostKeyDecision::Accept,
        HostKeyCheck::Unknown => HostKeyDecision::Learn,
        HostKeyCheck::Changed { .. } | HostKeyCheck::NoMatch | HostKeyCheck::LookupFailed => {
            HostKeyDecision::Reject
        }
    }
}

/// Look `key` up in the `known_hosts` file at `path`.
///
/// russh's lookup returns an empty list for *any* open failure, which
/// would make a permission error indistinguishable from a missing file,
/// so the file is opened here first and only `NotFound` counts as empty.
fn check_known_hosts(path: &Path, host: &str, port: u16, key: &PublicKey) -> HostKeyCheck {
    match std::fs::File::open(path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HostKeyCheck::Unknown,
        Err(e) => {
            tracing::warn!("cannot read {}: {e}", path.display());
            return HostKeyCheck::LookupFailed;
        }
    }

    let recorded = match known_host_keys_path(host, port, path) {
        Ok(recorded) => recorded,
        Err(e) => {
            tracing::warn!("cannot parse {} for {host}:{port}: {e}", path.display());
            return HostKeyCheck::LookupFailed;
        }
    };

    if recorded.is_empty() {
        return HostKeyCheck::Unknown;
    }
    if recorded.iter().any(|(_, known)| known == key) {
        return HostKeyCheck::Match;
    }
    recorded
        .iter()
        .find(|(_, known)| known.algorithm() == key.algorithm())
        .map_or(HostKeyCheck::NoMatch, |&(line, _)| HostKeyCheck::Changed {
            line,
        })
}

impl client::Handler for SshHandler {
    type Error = russh::Error;

    /// Verify the server's host key using TOFU (Trust On First Use).
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
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        let (host, port) = (self.host.as_str(), self.port);
        let Some(path) = self.known_hosts.as_deref() else {
            tracing::warn!("no home directory, cannot verify host key for {host}:{port}");
            return Ok(false);
        };

        let check = check_known_hosts(path, host, port, server_public_key);
        match decide_host_key(check) {
            HostKeyDecision::Accept => Ok(true),
            HostKeyDecision::Reject => {
                log_rejection(check, host, port);
                Ok(false)
            }
            HostKeyDecision::Learn => {
                tracing::info!("no known_hosts entry for {host}:{port}, learning key (TOFU)");
                if let Err(e) = learn_known_hosts_path(host, port, server_public_key, path) {
                    tracing::warn!("failed to save host key for {host}:{port}: {e}");
                }
                Ok(true)
            }
        }
    }
}

fn log_rejection(check: HostKeyCheck, host: &str, port: u16) {
    match check {
        HostKeyCheck::Changed { line } => {
            tracing::error!(
                "HOST KEY CHANGED for {host}:{port} (known_hosts entry {line}, comment lines not counted)"
            );
        }
        HostKeyCheck::NoMatch => {
            tracing::warn!("host key for {host}:{port} does not match any recorded algorithm");
        }
        HostKeyCheck::LookupFailed => {
            tracing::warn!("known_hosts unusable, refusing host key for {host}:{port}");
        }
        HostKeyCheck::Match | HostKeyCheck::Unknown => {}
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::*;

    const ED_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIDycHHcyGYpjMSjryJMZnj8VW3vMaGXrJhpx2LI77tJk";
    const ED_B: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIMPTQHEjtDcwWq5BXYA2mbO+OS77TWQ1FWTZOWrR+12u";
    const EC_A: &str = "AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBFHoHGTpZw+rtPY3QMlrSq5gmA8DsPGt9/8ExIOBgbwNL16t9z/ica7c3LP8YM1rKH5F+oVMZEri+qz9r3400cs=";

    fn key(b64: &str) -> PublicKey {
        russh::keys::parse_public_key_base64(b64).expect("test key parses")
    }

    fn known_hosts(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("known_hosts");
        std::fs::write(&path, contents).expect("write known_hosts");
        (dir, path)
    }

    #[test]
    fn decision_table() {
        assert_eq!(
            decide_host_key(HostKeyCheck::Match),
            HostKeyDecision::Accept
        );
        assert_eq!(
            decide_host_key(HostKeyCheck::Unknown),
            HostKeyDecision::Learn
        );
        assert_eq!(
            decide_host_key(HostKeyCheck::Changed { line: 4 }),
            HostKeyDecision::Reject
        );
        assert_eq!(
            decide_host_key(HostKeyCheck::NoMatch),
            HostKeyDecision::Reject
        );
        assert_eq!(
            decide_host_key(HostKeyCheck::LookupFailed),
            HostKeyDecision::Reject
        );
    }

    #[test]
    fn missing_file_is_unknown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("known_hosts");

        let check = check_known_hosts(&path, "box", 22, &key(ED_A));

        assert_eq!(check, HostKeyCheck::Unknown);
    }

    #[test]
    fn unlisted_host_is_unknown() {
        let (_dir, path) = known_hosts(&format!("other ssh-ed25519 {ED_A}\n"));

        assert_eq!(
            check_known_hosts(&path, "box", 22, &key(ED_A)),
            HostKeyCheck::Unknown
        );
    }

    #[test]
    fn recorded_key_matches() {
        let (_dir, path) = known_hosts(&format!("box ssh-ed25519 {ED_A}\n"));

        assert_eq!(
            check_known_hosts(&path, "box", 22, &key(ED_A)),
            HostKeyCheck::Match
        );
    }

    #[test]
    fn non_default_port_uses_bracket_form() {
        let (_dir, path) = known_hosts(&format!("[box]:2222 ssh-ed25519 {ED_A}\n"));

        assert_eq!(
            check_known_hosts(&path, "box", 2222, &key(ED_A)),
            HostKeyCheck::Match
        );
        assert_eq!(
            check_known_hosts(&path, "box", 22, &key(ED_A)),
            HostKeyCheck::Unknown
        );
    }

    #[test]
    fn same_algorithm_change_is_detected() {
        let (_dir, path) = known_hosts(&format!("# comment\nbox ssh-ed25519 {ED_A}\n"));

        // russh numbers entries, skipping comment lines.

        assert_eq!(
            check_known_hosts(&path, "box", 22, &key(ED_B)),
            HostKeyCheck::Changed { line: 1 }
        );
    }

    #[test]
    fn other_algorithm_only_is_no_match() {
        let (_dir, path) = known_hosts(&format!("box ecdsa-sha2-nistp256 {EC_A}\n"));

        assert_eq!(
            check_known_hosts(&path, "box", 22, &key(ED_A)),
            HostKeyCheck::NoMatch
        );
    }

    #[test]
    fn unparseable_entry_fails_closed() {
        // The regression this guards: a broken entry used to count as "no
        // recorded keys" and get learned, accepting any key forever.
        let (_dir, path) = known_hosts("box ssh-ed25519 not-base64!!\n");

        let check = check_known_hosts(&path, "box", 22, &key(ED_A));

        assert_eq!(check, HostKeyCheck::LookupFailed);
        assert_eq!(decide_host_key(check), HostKeyDecision::Reject);
    }

    #[test]
    fn unreadable_file_fails_closed() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_dir, path) = known_hosts(&format!("box ssh-ed25519 {ED_A}\n"));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");
        if std::fs::File::open(&path).is_ok() {
            // Running as root: permissions are not enforced, nothing to test.
            return;
        }

        assert_eq!(
            check_known_hosts(&path, "box", 22, &key(ED_B)),
            HostKeyCheck::LookupFailed
        );
    }

    #[test]
    fn ipv6_brackets_are_dropped_for_lookup() {
        assert_eq!(known_hosts_host("[::1]"), "::1");
        assert_eq!(known_hosts_host("::1"), "::1");
        assert_eq!(known_hosts_host("box.local"), "box.local");
    }
}

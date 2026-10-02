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

/// The host pattern `known_hosts` uses: `host` on port 22, `[host]:port`
/// otherwise.
fn known_hosts_pattern(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

/// A `known_hosts` line that names the host but that russh's parser does
/// not evaluate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UncheckedEntry {
    /// `@cert-authority` / `@revoked`: russh ignores markers, so a revoked
    /// key could otherwise be accepted.
    Marker,
    /// A wildcard pattern or a line not separated by single spaces, which
    /// russh skips. Treating the host as unknown would let TOFU replace a
    /// key the user did record.
    Unparsed,
}

/// Scan `contents` for lines naming `pattern` that russh cannot check.
/// Hashed (`|1|`) entries are left to russh, which does check them when
/// they are written in OpenSSH's own format.
fn unchecked_entry(contents: &str, pattern: &str) -> Option<UncheckedEntry> {
    let mut found = None;
    for raw in contents.lines() {
        let mut fields = raw.split_whitespace();
        let Some(first) = fields.next().filter(|f| !f.starts_with('#')) else {
            continue;
        };
        let (marker, hosts) = if first.starts_with('@') {
            (true, fields.next().unwrap_or_default())
        } else {
            (false, first)
        };
        if !hosts.split(',').any(|p| names_host(p, pattern)) {
            continue;
        }
        if marker {
            return Some(UncheckedEntry::Marker);
        }
        // russh splits the raw line on single spaces and compares host
        // patterns literally.
        let russh_reads_it = !raw.contains('\t')
            && raw.split(' ').next() == Some(hosts)
            && !hosts.contains(['*', '?']);
        if !russh_reads_it {
            found = Some(UncheckedEntry::Unparsed);
        }
    }
    found
}

/// Whether one host pattern (plain or `*`/`?` wildcard, not negated or
/// hashed) matches `host`.
fn names_host(pattern: &str, host: &str) -> bool {
    if pattern.starts_with('!') || pattern.starts_with("|1|") {
        return false;
    }
    glob_match(pattern.as_bytes(), host.as_bytes())
}

fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|skip| glob_match(rest, &text[skip..])),
        Some((&p, rest)) => text
            .split_first()
            .is_some_and(|(&t, text_rest)| (p == b'?' || p == t) && glob_match(rest, text_rest)),
    }
}

/// Look `key` up in the `known_hosts` file at `path`.
///
/// russh's lookup returns an empty list for *any* open failure, which
/// would make a permission error indistinguishable from a missing file,
/// so the file is opened here first and only `NotFound` counts as empty.
fn check_known_hosts(path: &Path, host: &str, port: u16, key: &PublicKey) -> HostKeyCheck {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HostKeyCheck::Unknown,
        Err(e) => {
            tracing::warn!("cannot read {}: {e}", path.display());
            return HostKeyCheck::LookupFailed;
        }
    };

    let recorded = match known_host_keys_path(host, port, path) {
        Ok(recorded) => recorded,
        Err(e) => {
            tracing::warn!("cannot parse {} for {host}:{port}: {e}", path.display());
            return HostKeyCheck::LookupFailed;
        }
    };

    match unchecked_entry(&contents, &known_hosts_pattern(host, port)) {
        Some(UncheckedEntry::Marker) => {
            tracing::warn!(
                "{} has a @cert-authority/@revoked line for {host}:{port}, which is \
                 not supported; refusing to decide",
                path.display()
            );
            return HostKeyCheck::LookupFailed;
        }
        Some(UncheckedEntry::Unparsed) if recorded.is_empty() => {
            tracing::warn!(
                "{} names {host}:{port} in a line russh cannot check (tabs, extra \
                 spaces or a wildcard); refusing to learn a new key",
                path.display()
            );
            return HostKeyCheck::LookupFailed;
        }
        Some(UncheckedEntry::Unparsed) | None => {}
    }

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
                // Accepting a key that could not be recorded would make every
                // later connection a "first" one, accepting any key forever.
                match learn_known_hosts_path(host, port, server_public_key, path) {
                    Ok(()) => Ok(true),
                    Err(e) => {
                        tracing::error!(
                            "refusing {host}:{port}: could not record its host key in {}: {e}",
                            path.display()
                        );
                        Ok(false)
                    }
                }
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
    fn tab_separated_entry_blocks_learning() {
        let (_dir, path) = known_hosts(&format!("box\tssh-ed25519 {ED_A}\n"));

        assert_eq!(
            check_known_hosts(&path, "box", 22, &key(ED_B)),
            HostKeyCheck::LookupFailed
        );
    }

    #[test]
    fn wildcard_entry_blocks_learning() {
        let (_dir, path) = known_hosts(&format!("*.prod.internal ssh-ed25519 {ED_A}\n"));

        assert_eq!(
            check_known_hosts(&path, "web.prod.internal", 22, &key(ED_B)),
            HostKeyCheck::LookupFailed
        );
        assert_eq!(
            check_known_hosts(&path, "web.staging.internal", 22, &key(ED_B)),
            HostKeyCheck::Unknown
        );
    }

    #[test]
    fn marker_lines_for_the_host_fail_closed() {
        let (_dir, path) = known_hosts(&format!(
            "box ssh-ed25519 {ED_A}\n@revoked box ssh-ed25519 {ED_A}\n"
        ));

        assert_eq!(
            check_known_hosts(&path, "box", 22, &key(ED_A)),
            HostKeyCheck::LookupFailed
        );
    }

    #[test]
    fn unrelated_odd_lines_do_not_interfere() {
        let (_dir, path) = known_hosts(&format!(
            "other\tssh-ed25519 {ED_B}\n!box,* ssh-ed25519 {ED_B}\nbox ssh-ed25519 {ED_A}\n"
        ));

        assert_eq!(
            check_known_hosts(&path, "box", 22, &key(ED_A)),
            HostKeyCheck::Match
        );
    }

    #[test]
    fn glob_patterns() {
        assert!(glob_match(b"*.example.com", b"a.example.com"));
        assert!(glob_match(b"web-?", b"web-1"));
        assert!(!glob_match(b"web-?", b"web-12"));
        assert!(glob_match(b"[box]:2222", b"[box]:2222"));
        assert!(!names_host("!box", "box"));
        assert_eq!(known_hosts_pattern("box", 2222), "[box]:2222");
    }

    #[test]
    fn ipv6_brackets_are_dropped_for_lookup() {
        assert_eq!(known_hosts_host("[::1]"), "::1");
        assert_eq!(known_hosts_host("::1"), "::1");
        assert_eq!(known_hosts_host("box.local"), "box.local");
    }
}

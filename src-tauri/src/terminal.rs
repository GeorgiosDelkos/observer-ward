//! Launch kubectl/ssh in Warp or Terminal.app.
//!
//! Commands are built only from saved server config plus a pod name, and
//! every interpolated value is checked by [`validate_shell_safe`] before
//! it is single-quoted into the command line.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::config::{K8sTarget, SshTarget, expand_tilde};

/// How long the temp script handed to Warp is kept before deletion. Warp
/// reads it on launch, which can take a while when it is not running yet.
const WARP_SCRIPT_TTL: Duration = Duration::from_mins(1);

#[derive(Debug, thiserror::Error)]
pub(crate) enum TerminalError {
    #[error("{field} is empty")]
    Empty { field: &'static str },
    #[error("{field} must not start with '-'")]
    LeadingDash { field: &'static str },
    #[error("{field} contains characters that are unsafe in a shell command")]
    UnsafeCharacters { field: &'static str },
    #[error("{field} is not valid UTF-8")]
    NonUtf8 { field: &'static str },
    #[error("'{0}' is not a valid pod name")]
    InvalidPodName(String),
    #[error("failed to {action}")]
    Io {
        action: &'static str,
        #[source]
        source: std::io::Error,
    },
}

/// Allow only characters that are inert inside single quotes in a shell
/// command and inside an `AppleScript` string, and refuse a leading `-`
/// so a value can never be read as a command-line option.
fn validate_shell_safe(value: &str, field: &'static str) -> Result<(), TerminalError> {
    if value.is_empty() {
        return Err(TerminalError::Empty { field });
    }
    if value.starts_with('-') {
        return Err(TerminalError::LeadingDash { field });
    }
    let safe = |c: char| {
        c.is_alphanumeric()
            || matches!(
                c,
                '-' | '_' | '.' | '/' | '@' | ':' | '~' | '+' | ' ' | '[' | ']'
            )
    };
    if !value.chars().all(safe) {
        return Err(TerminalError::UnsafeCharacters { field });
    }
    Ok(())
}

fn safe_path<'a>(path: &'a Path, field: &'static str) -> Result<&'a str, TerminalError> {
    let path = path.to_str().ok_or(TerminalError::NonUtf8 { field })?;
    validate_shell_safe(path, field)?;
    Ok(path)
}

/// Pod names are DNS-1123 subdomains: lowercase alphanumerics, `-` and
/// `.`, starting and ending alphanumeric, at most 253 characters.
fn validate_pod_name(pod: &str) -> Result<(), TerminalError> {
    let edge_ok = |c: Option<char>| c.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let valid = pod.len() <= 253
        && edge_ok(pod.chars().next())
        && edge_ok(pod.chars().last())
        && pod
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.');
    if valid {
        Ok(())
    } else {
        Err(TerminalError::InvalidPodName(pod.to_string()))
    }
}

/// `ssh` invocation for a saved SSH server.
pub(crate) fn ssh_command(target: &SshTarget) -> Result<String, TerminalError> {
    let host = crate::ssh::ssh_cli_host(&target.host);
    let key_path = expand_tilde(&target.key_path);
    validate_shell_safe(&host, "host")?;
    validate_shell_safe(&target.user, "user")?;
    let key_path = safe_path(&key_path, "key_path")?;

    Ok(format!(
        "ssh -p {} -i '{key_path}' '{}'@'{host}'",
        target.port, target.user
    ))
}

/// `kubectl logs -f` invocation for `pod` on a saved cluster.
pub(crate) fn pod_logs_command(target: &K8sTarget, pod: &str) -> Result<String, TerminalError> {
    validate_pod_name(pod)?;
    validate_shell_safe(&target.namespace, "namespace")?;
    validate_shell_safe(&target.context, "context")?;

    let mut cmd = format!(
        "kubectl logs -f '{pod}' -n '{}' --context '{}'",
        target.namespace, target.context
    );
    if let Some(kubeconfig) = &target.kubeconfig {
        let kubeconfig = expand_tilde(kubeconfig);
        let kubeconfig = safe_path(&kubeconfig, "kubeconfig")?;
        cmd.push_str(" --kubeconfig '");
        cmd.push_str(kubeconfig);
        cmd.push('\'');
    }
    Ok(cmd)
}

pub(crate) fn run_in_terminal(cmd: &str) -> Result<(), TerminalError> {
    if Path::new("/Applications/Warp.app").exists() {
        run_in_warp(cmd)
    } else {
        run_in_terminal_app(cmd)
    }
}

/// Monotonic per-process counter making each temp-script filename unique.
/// Combined with pid + millis it prevents `create_new` from spuriously
/// failing when two terminal launches land in the same millisecond.
static SCRIPT_SEQ: AtomicU64 = AtomicU64::new(0);

fn run_in_warp(cmd: &str) -> Result<(), TerminalError> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let tmp = std::env::temp_dir().join(format!(
        "ow-cmd-{}-{}-{}.sh",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis()),
        SCRIPT_SEQ.fetch_add(1, Ordering::Relaxed)
    ));

    // Create the script exclusively (O_EXCL via create_new) so a symlink
    // pre-planted at this predictable path cannot redirect the write, and
    // with mode 0o700 so only the owner can read/execute it.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&tmp)
        .map_err(|source| TerminalError::Io {
            action: "create temp script",
            source,
        })?;
    file.write_all(format!("#!/bin/bash\n{cmd}\n").as_bytes())
        .map_err(|source| TerminalError::Io {
            action: "write temp script",
            source,
        })?;
    drop(file);

    std::process::Command::new("open")
        .args(["-a", "Warp"])
        .arg(&tmp)
        .spawn()
        .map_err(|source| TerminalError::Io {
            action: "open Warp",
            source,
        })?;

    std::thread::spawn(move || {
        std::thread::sleep(WARP_SCRIPT_TTL);
        if let Err(e) = std::fs::remove_file(&tmp) {
            tracing::debug!("failed to remove temp script {}: {e}", tmp.display());
        }
    });
    Ok(())
}

fn run_in_terminal_app(cmd: &str) -> Result<(), TerminalError> {
    let escaped = cmd.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "tell application \"Terminal\"\n\
         activate\n\
         do script \"{escaped}\"\n\
         end tell"
    );
    std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .spawn()
        .map_err(|source| TerminalError::Io {
            action: "open Terminal",
            source,
        })?;
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn ssh_target(host: &str, user: &str) -> SshTarget {
        SshTarget {
            name: "box".into(),
            host: host.to_string(),
            port: 2222,
            user: user.to_string(),
            key_path: PathBuf::from("/home/me/.ssh/id_ed25519"),
        }
    }

    fn k8s_target(kubeconfig: Option<&str>) -> K8sTarget {
        K8sTarget {
            name: "prod".into(),
            kubeconfig: kubeconfig.map(PathBuf::from),
            context: "prod-ctx".to_string(),
            namespace: "default".to_string(),
        }
    }

    #[test]
    fn validate_shell_safe_accepts_typical_values() {
        for value in [
            "prod-ctx",
            "user_name",
            "/home/user/.ssh/id",
            "10.0.0.5",
            "[::1]",
        ] {
            validate_shell_safe(value, "field").expect(value);
        }
    }

    #[test]
    fn validate_shell_safe_rejects_injection() {
        for value in ["foo;rm", "foo'bar", "$(id)", "a`b`", "-oProxyCommand=x", ""] {
            assert!(validate_shell_safe(value, "field").is_err(), "{value:?}");
        }
    }

    #[test]
    fn pod_names_follow_dns_1123() {
        validate_pod_name("web-7d4f9c-x2x9z").expect("typical pod");
        validate_pod_name("a.b-1").expect("dots allowed");
        for bad in ["", "-web", "web-", "Web", "web_1", "--context=evil", "a b"] {
            assert!(validate_pod_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_pod_name(&"a".repeat(254)).is_err());
    }

    #[test]
    fn ssh_command_quotes_every_value() {
        let cmd = ssh_command(&ssh_target("2001:db8::1", "deploy")).expect("command");

        assert_eq!(
            cmd,
            "ssh -p 2222 -i '/home/me/.ssh/id_ed25519' 'deploy'@'[2001:db8::1]'"
        );
    }

    #[test]
    fn ssh_command_rejects_option_injection() {
        assert!(ssh_command(&ssh_target("-oProxyCommand=x", "u")).is_err());
        assert!(ssh_command(&ssh_target("box", "-v")).is_err());
    }

    #[test]
    fn pod_logs_command_with_and_without_kubeconfig() {
        let cmd = pod_logs_command(&k8s_target(None), "web-1").expect("command");
        assert_eq!(
            cmd,
            "kubectl logs -f 'web-1' -n 'default' --context 'prod-ctx'"
        );

        let cmd = pod_logs_command(&k8s_target(Some("/etc/kube/prod")), "web-1").expect("command");
        assert!(cmd.ends_with(" --kubeconfig '/etc/kube/prod'"), "{cmd}");
    }

    #[test]
    fn pod_logs_command_rejects_flag_shaped_pod_names() {
        assert!(pod_logs_command(&k8s_target(None), "--kubeconfig=/evil").is_err());
    }
}

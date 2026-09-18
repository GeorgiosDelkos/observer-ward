//! Launch kubectl/ssh in Warp or Terminal.app.

use std::sync::atomic::{AtomicU64, Ordering};

/// Only allow characters safe for interpolation into
/// `AppleScript` `do script` strings and shell commands.
pub(crate) fn validate_shell_safe(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{field} is empty"));
    }
    if !value.chars().all(|c| {
        c.is_alphanumeric()
            || matches!(
                c,
                '-' | '_' | '.' | '/' | '@' | ':' | '~' | '+' | ' ' | '[' | ']'
            )
    }) {
        return Err(format!("{field} contains unsafe characters"));
    }
    Ok(())
}

pub(crate) fn run_in_terminal(cmd: &str) -> Result<(), String> {
    if std::path::Path::new("/Applications/Warp.app").exists() {
        run_in_warp(cmd)
    } else {
        run_in_terminal_app(cmd)
    }
}

/// Monotonic per-process counter making each temp-script filename unique.
/// Combined with pid + millis it prevents `create_new` from spuriously
/// failing when two terminal launches land in the same millisecond.
static SCRIPT_SEQ: AtomicU64 = AtomicU64::new(0);

fn run_in_warp(cmd: &str) -> Result<(), String> {
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
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&tmp)
            .map_err(|e| format!("failed to create temp script: {e}"))?;
        file.write_all(format!("#!/bin/bash\n{cmd}\n").as_bytes())
            .map_err(|e| format!("failed to write temp script: {e}"))?;
    }
    let path_str = tmp
        .to_str()
        .ok_or_else(|| "temp path is not valid UTF-8".to_string())?;
    std::process::Command::new("open")
        .args(["-a", "Warp", path_str])
        .spawn()
        .map_err(|e| format!("failed to open Warp: {e}"))?;
    let cleanup_path = tmp.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(10));
        let _ = std::fs::remove_file(&cleanup_path);
    });
    Ok(())
}

fn run_in_terminal_app(cmd: &str) -> Result<(), String> {
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
        .map_err(|e| format!("failed to open Terminal: {e}"))?;
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use super::validate_shell_safe;

    #[test]
    fn validate_shell_safe_accepts_typical_values() {
        validate_shell_safe("prod-ctx", "context").expect("context");
        validate_shell_safe("user_name", "user").expect("user");
        validate_shell_safe("/home/user/.ssh/id_ed25519", "key").expect("key");
        validate_shell_safe("10.0.0.5", "host").expect("host");
        validate_shell_safe("[::1]", "host").expect("ipv6");
        validate_shell_safe("/Users/me/.ssh/id", "key").expect("expanded key");
    }

    #[test]
    fn validate_shell_safe_rejects_quotes_and_semicolons() {
        assert!(validate_shell_safe("foo;rm", "host").is_err());
        assert!(validate_shell_safe("foo'bar", "user").is_err());
        assert!(validate_shell_safe("", "host").is_err());
    }
}

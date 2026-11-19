//! Kubeconfig `exec` credential plugins (kubectl-bao, aws, gke-gcloud-auth-plugin,
//! kubelogin), run from a menu-bar app rather than a terminal. Two things differ:
//!
//! - PATH. An app started by launchd gets `/usr/bin:/bin:/usr/sbin:/sbin`, so a plugin
//!   named without a path (`command: kubectl-bao`) and the tools it calls (`bao` and `jq`
//!   from Homebrew) are not found. Every plugin gets the user's login-shell PATH, unless
//!   the kubeconfig sets PATH for it.
//! - Interactive login. A plugin whose session expired may open a browser to sign in.
//!   Polls run plugins non-interactively (`interactiveMode: Never`, which kube passes on
//!   as `spec.interactive: false` in `KUBERNETES_EXEC_INFO`), so a background poll never
//!   opens one: the plugin is expected to fail instead, and the cluster offers a login
//!   that runs it interactively ([`login`]), once, on request.
//!
//! The second point holds only for plugins that read `spec.interactive`: kube does not
//! stop a plugin from opening a browser. kubectl-bao as of
//! <https://github.com/thenervelab/ci-actions/blob/dc2cf234b1276c861c32c722e81f73a042699111/kubectl-bao/kubectl-bao>
//! ignores it, so until it does, a poll whose session has expired starts a browser
//! sign-in. kube runs the plugin on a blocking thread that the poll timeout does not
//! cancel, so the next poll may start another while the first waits.

use std::collections::HashMap;
use std::error::Error;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use kube::client::AuthError;
use kube::config::{ExecConfig, ExecInteractiveMode, Kubeconfig};
use tokio::process::Command;
use tokio::sync::OnceCell;

use super::K8sError;

/// How long the login shell may take to print PATH. Arbitrary: rc files that wait for
/// input must not stall every connection, and a few seconds covers slow ones.
const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(5);

/// How long an interactive login may take. Arbitrary: enough for a browser sign-in with
/// a second factor.
const LOGIN_TIMEOUT: Duration = Duration::from_mins(5);

/// Most of a plugin's stderr kept in an error, in characters. Arbitrary: one or two
/// lines of explanation fit; a dumped stack trace does not flood the card.
const STDERR_LIMIT: usize = 400;

/// Printed before PATH, so lines that rc files print first (greetings, prompt themes)
/// are skipped.
const PATH_MARKER: &str = "__observer_ward_path__=";

/// The user's login-shell PATH, read once per app run. `None` when the shell could not
/// be run; plugins then get the app's own PATH, and the next call tries again, so one
/// slow start (rc files at login, a busy disk) does not last the whole run.
pub(super) async fn login_path() -> Option<&'static str> {
    static LOGIN_PATH: OnceCell<String> = OnceCell::const_new();
    let path = LOGIN_PATH.get_or_try_init(|| async { read_login_path().await.ok_or(()) });
    path.await.ok().map(String::as_str)
}

/// Run `$SHELL` as an interactive login shell and read its PATH. Interactive as well as
/// login, because zsh users commonly set PATH in `~/.zshrc`, which only interactive
/// shells read. Assumes a POSIX-like shell (zsh, bash): fish joins `"$PATH"` with spaces.
async fn read_login_path() -> Option<String> {
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/zsh".into());
    let script = format!("printf '%s%s' '{PATH_MARKER}' \"$PATH\"");
    let mut command = Command::new(&shell);
    command
        .args(["-l", "-i", "-c", &script])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let output = match tokio::time::timeout(LOGIN_SHELL_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            tracing::warn!("cannot run login shell {}: {e}", shell.display());
            return None;
        }
        Err(_) => {
            tracing::warn!(
                "login shell {} did not print PATH within {LOGIN_SHELL_TIMEOUT:?}",
                shell.display()
            );
            return None;
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path = stdout.rsplit_once(PATH_MARKER)?.1;
    (!path.is_empty()).then(|| path.to_string())
}

/// Make every exec plugin in `kubeconfig` fit for a background poll: non-interactive,
/// and with `path` as PATH when given.
pub(super) fn prepare_for_polling(kubeconfig: &mut Kubeconfig, path: Option<&str>) {
    let plugins = kubeconfig
        .auth_infos
        .iter_mut()
        .filter_map(|named| named.auth_info.as_mut()?.exec.as_mut());
    for exec in plugins {
        exec.interactive_mode = Some(ExecInteractiveMode::Never);
        if let Some(path) = path {
            set_path(exec, path);
        }
    }
}

/// Add `PATH=path` to the plugin's environment. A PATH the kubeconfig sets wins.
fn set_path(exec: &mut ExecConfig, path: &str) {
    let env = exec.env.get_or_insert_with(Vec::new);
    if env
        .iter()
        .any(|var| var.get("name").is_some_and(|name| name == "PATH"))
    {
        return;
    }
    env.push(HashMap::from([
        ("name".to_string(), "PATH".to_string()),
        ("value".to_string(), path.to_string()),
    ]));
}

/// Replace a failed plugin run, anywhere in `error`'s cause chain, with
/// [`K8sError::CredentialPlugin`]. kube's own message for it is the `Debug` print of the
/// whole command, with every environment variable set on it, and of the raw output bytes.
pub(super) fn plugin_failure(error: K8sError) -> K8sError {
    let mut cause: Option<&(dyn Error + 'static)> = Some(&error);
    while let Some(current) = cause {
        if let Some(AuthError::AuthExecRun { out, .. }) = current.downcast_ref::<AuthError>() {
            return K8sError::CredentialPlugin {
                detail: stderr_summary(&out.stderr),
            };
        }
        cause = current.source();
    }
    error
}

/// What the plugin printed to stderr, trimmed and cut to [`STDERR_LIMIT`] characters.
fn stderr_summary(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        return "it printed nothing".to_string();
    }
    let mut summary: String = text.chars().take(STDERR_LIMIT).collect();
    if summary.len() < text.len() {
        summary.push('…');
    }
    summary
}

/// Run the exec plugin of `context` interactively, so it can sign in (open a browser,
/// wait for the callback) and cache its session for the next poll. Waits up to
/// [`LOGIN_TIMEOUT`]. The credential it prints is discarded: the poller's client runs the
/// plugin again and gets it from the plugin's cache.
///
/// # Errors
///
/// [`K8sError`] when the kubeconfig cannot be read, the context's user has no exec
/// plugin, or the plugin fails, cannot start, or does not finish in time.
pub(crate) async fn login(path: Option<PathBuf>, context: String) -> Result<(), K8sError> {
    login_with(path, context, login_path().await).await
}

/// [`login`], with `login_path` as the plugin's PATH.
async fn login_with(
    path: Option<PathBuf>,
    context: String,
    login_path: Option<&str>,
) -> Result<(), K8sError> {
    let exec = tokio::task::spawn_blocking(move || {
        let kubeconfig = super::kubeconfig::load(path.as_deref())?;
        plugin_of(&kubeconfig, &context)
    })
    .await
    .map_err(K8sError::KubeconfigTask)??;

    let mut command = interactive_command(exec, login_path)?;
    let output = tokio::time::timeout(LOGIN_TIMEOUT, command.output())
        .await
        .map_err(|_| K8sError::LoginTimeout {
            limit: LOGIN_TIMEOUT,
        })?
        .map_err(K8sError::StartCredentialPlugin)?;
    if !output.status.success() {
        return Err(K8sError::CredentialPlugin {
            detail: stderr_summary(&output.stderr),
        });
    }
    Ok(())
}

/// The exec plugin of the user that `context` names.
fn plugin_of(kubeconfig: &Kubeconfig, context: &str) -> Result<ExecConfig, K8sError> {
    let user = kubeconfig
        .contexts
        .iter()
        .find(|named| named.name == context)
        .and_then(|named| named.context.as_ref()?.user.as_deref());
    user.and_then(|user| {
        kubeconfig
            .auth_infos
            .iter()
            .find(|named| named.name == user)
    })
    .and_then(|named| named.auth_info.as_ref()?.exec.clone())
    .ok_or_else(|| K8sError::NoCredentialPlugin {
        context: context.to_string(),
    })
}

/// The plugin as kubectl would run it from a terminal, with `KUBERNETES_EXEC_INFO`
/// saying a user is present. stdout is captured and dropped: it holds the token.
///
/// `provideClusterInfo` is not honoured: `spec.cluster` would need the cluster resolved
/// from the kubeconfig, and kubectl-bao does not read it. kube honours it in polls.
fn interactive_command(mut exec: ExecConfig, path: Option<&str>) -> Result<Command, K8sError> {
    if let Some(path) = path {
        set_path(&mut exec, path);
    }
    let Some(program) = &exec.command else {
        return Err(K8sError::NoPluginCommand);
    };
    let exec_info = serde_json::json!({
        "apiVersion": exec.api_version,
        "kind": "ExecCredential",
        "spec": { "interactive": true },
    });

    let mut command = Command::new(program);
    command
        .args(exec.args.iter().flatten())
        .envs(
            exec.env
                .iter()
                .flatten()
                .filter_map(|var| Some((var.get("name")?, var.get("value")?))),
        )
        .env("KUBERNETES_EXEC_INFO", exec_info.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for name in exec.drop_env.iter().flatten() {
        command.env_remove(name);
    }
    Ok(command)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "panicking on failure is standard in tests"
)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::login_with;
    use crate::error::error_chain;
    use crate::k8s::{K8sError, connect_with};

    const CREDENTIAL: &str = r#"{"apiVersion":"client.authentication.k8s.io/v1","kind":"ExecCredential","status":{"token":"t","expirationTimestamp":"2099-01-01T00:00:00Z"}}"#;

    /// A plugin directory and a kubeconfig whose context `ctx` runs `plugin`
    /// by bare name, as `kubectl-bao setup` writes it, and whose context
    /// `plain` uses a static token.
    struct Fixture {
        dir: tempfile::TempDir,
        bin: PathBuf,
        kubeconfig: PathBuf,
    }

    impl Fixture {
        /// `body` runs after the plugin records its exec info and PATH.
        fn new(body: &str) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let bin = dir.path().join("bin");
            std::fs::create_dir(&bin).expect("bin dir");
            let record = dir.path().join("record");
            let script = format!(
                "#!/bin/sh\nprintf '%s\\n%s\\n' \"$KUBERNETES_EXEC_INFO\" \"$PATH\" > '{}'\n{body}\n",
                record.display()
            );
            let plugin = bin.join("fake-plugin");
            std::fs::write(&plugin, script).expect("write plugin");
            std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755))
                .expect("make plugin executable");

            let kubeconfig = dir.path().join("kubeconfig.yaml");
            std::fs::write(&kubeconfig, KUBECONFIG).expect("write kubeconfig");
            Self {
                dir,
                bin,
                kubeconfig,
            }
        }

        fn path(&self) -> String {
            format!("{}:/usr/bin:/bin", self.bin.display())
        }

        /// The exec info JSON and PATH the plugin saw.
        fn record(&self) -> (serde_json::Value, String) {
            let text =
                std::fs::read_to_string(self.dir.path().join("record")).expect("the plugin ran");
            let (info, path) = text.split_once('\n').expect("two lines");
            let info = serde_json::from_str(info).expect("exec info is JSON");
            (info, path.trim_end().to_string())
        }
    }

    const KUBECONFIG: &str = r#"
apiVersion: v1
kind: Config
clusters:
- name: c
  cluster:
    server: "https://127.0.0.1:1"
users:
- name: bao
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1
      command: fake-plugin
      args: ["h-test"]
      interactiveMode: IfAvailable
- name: static
  user:
    token: "not-a-real-token"
contexts:
- name: ctx
  context: {cluster: c, user: bao}
- name: plain
  context: {cluster: c, user: static}
"#;

    fn ok_plugin() -> Fixture {
        Fixture::new(&format!("printf '%s' '{CREDENTIAL}'"))
    }

    fn expired_plugin() -> Fixture {
        Fixture::new("echo 'kubectl-bao: OpenBao session expired: log in again' >&2\nexit 1")
    }

    #[tokio::test]
    async fn polling_finds_a_bare_plugin_on_the_login_path_and_never_asks_for_a_user() {
        // The reported bug: launched from Finder, the app's PATH lacks
        // ~/.local/bin, so `command: kubectl-bao` was never found.
        let fixture = ok_plugin();

        connect_with(
            Some(fixture.kubeconfig.clone()),
            "ctx",
            Some(fixture.path()),
        )
        .await
        .expect("the plugin is found and its credential accepted");

        let (info, path) = fixture.record();
        assert_eq!(info["spec"]["interactive"], false, "{info}");
        assert_eq!(path, fixture.path());
    }

    #[tokio::test]
    async fn a_failed_plugin_reports_its_stderr_without_its_environment() {
        let fixture = expired_plugin();

        let err = connect_with(
            Some(fixture.kubeconfig.clone()),
            "ctx",
            Some(fixture.path()),
        )
        .await
        .map(drop)
        .expect_err("the plugin fails");

        assert!(err.needs_login());
        let rendered = error_chain(&err);
        assert_eq!(
            rendered,
            "credential plugin failed: kubectl-bao: OpenBao session expired: log in again"
        );
        assert!(!rendered.contains("KUBERNETES_EXEC_INFO"), "{rendered}");
    }

    #[tokio::test]
    async fn login_runs_the_plugin_interactively() {
        let fixture = ok_plugin();

        login_with(
            Some(fixture.kubeconfig.clone()),
            "ctx".to_string(),
            Some(&fixture.path()),
        )
        .await
        .expect("login succeeds");

        let (info, path) = fixture.record();
        assert_eq!(info["spec"]["interactive"], true, "{info}");
        assert_eq!(
            info["apiVersion"], "client.authentication.k8s.io/v1",
            "{info}"
        );
        assert_eq!(path, fixture.path());
    }

    #[tokio::test]
    async fn login_reports_a_failed_plugin_and_a_context_without_one() {
        let fixture = expired_plugin();

        let err = login_with(
            Some(fixture.kubeconfig.clone()),
            "ctx".to_string(),
            Some(&fixture.path()),
        )
        .await
        .expect_err("the plugin fails");
        assert!(matches!(
            &err,
            K8sError::CredentialPlugin { detail } if detail.ends_with("log in again")
        ));

        let err = login_with(Some(fixture.kubeconfig.clone()), "ctx".to_string(), None)
            .await
            .expect_err("without the login path, a bare plugin name is not found");
        assert_eq!(err.to_string(), "failed to start credential plugin");

        let err = login_with(Some(fixture.kubeconfig.clone()), "plain".to_string(), None)
            .await
            .expect_err("a static token has no plugin");
        assert_eq!(
            err.to_string(),
            "context plain has no credential plugin to log in with"
        );
    }

    #[test]
    fn a_path_the_kubeconfig_sets_wins() {
        let mut exec: kube::config::ExecConfig = serde_json::from_value(serde_json::json!({
            "command": "kubectl-bao",
            "env": [{"name": "PATH", "value": "/opt/custom"}],
        }))
        .expect("exec config");

        super::set_path(&mut exec, "/login/path");

        let env = exec.env.expect("env kept");
        assert_eq!(env.len(), 1);
        assert_eq!(env[0]["value"], "/opt/custom");
    }

    #[test]
    fn long_stderr_is_cut() {
        let summary = super::stderr_summary("x".repeat(1000).as_bytes());
        assert_eq!(summary.chars().count(), super::STDERR_LIMIT + 1);
        assert!(summary.ends_with('…'));
        assert_eq!(super::stderr_summary(b"  \n"), "it printed nothing");
    }
}

//! Native runtime implementation
//!
//! Executes commands directly on the host system without container isolation.
//! This is the fallback when no container runtime is configured.

use async_trait::async_trait;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

use super::types::{CommandOutput, ContainerConfig, ContainerRuntime, RuntimeError, RuntimeResult};

/// Native runtime that executes commands directly on the host
#[derive(Debug, Clone, Default)]
pub struct NativeRuntime;

impl NativeRuntime {
    /// Create a new native runtime
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ContainerRuntime for NativeRuntime {
    fn name(&self) -> &str {
        "native"
    }

    async fn is_available(&self) -> bool {
        // Native runtime is always available
        true
    }

    async fn execute(
        &self,
        command: &str,
        config: &ContainerConfig,
    ) -> RuntimeResult<CommandOutput> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);

        // Set working directory if specified
        if let Some(ref workdir) = config.workdir {
            cmd.current_dir(workdir);
        }

        // Scrub environment: forward a minimal whitelist plus explicit
        // config.env, so host secrets (cloud credentials, tokens) are not
        // leaked to subprocesses (#644).
        apply_scrubbed_env(&mut cmd, &config.env);

        // Own process group, so a timeout can kill the whole tree (#644).
        #[cfg(unix)]
        cmd.process_group(0);

        // Capture output
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| RuntimeError::ExecutionFailed(e.to_string()))?;
        // Drain pipes concurrently with the wait so large output cannot fill
        // the pipe buffers and deadlock; a timeout can still kill the group.
        let read_out = drain_pipe(child.stdout.take());
        let read_err = drain_pipe(child.stderr.take());

        let waited = tokio::time::timeout(Duration::from_secs(config.timeout_secs), async {
            tokio::join!(child.wait(), read_out, read_err)
        })
        .await;

        let (status, out_buf, err_buf) = match waited {
            Ok(result) => result,
            Err(_elapsed) => {
                kill_process_tree(&mut child);
                // Reap promptly so the killed group does not linger as zombie.
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
                return Err(RuntimeError::Timeout(config.timeout_secs));
            }
        };
        let status = status.map_err(|e| RuntimeError::ExecutionFailed(e.to_string()))?;

        Ok(CommandOutput::new(
            String::from_utf8_lossy(&out_buf).to_string(),
            String::from_utf8_lossy(&err_buf).to_string(),
            status.code(),
        ))
    }
}

/// Drain a child output pipe into a buffer (best effort on read errors).
async fn drain_pipe<R: tokio::io::AsyncRead + Unpin>(mut pipe: Option<R>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(p) = pipe.as_mut() {
        use tokio::io::AsyncReadExt;
        let _ = p.read_to_end(&mut buf).await;
    }
    buf
}

/// Environment variables forwarded from the host to native subprocesses.
/// Everything else (cloud credentials, API tokens, ...) is stripped (#644).
const ENV_PASSTHROUGH: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "SHELL",
    "TERM",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "TZ",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    // Trust anchors and agent sockets some child tools depend on.
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "SSH_AUTH_SOCK",
];

/// Clear the inherited environment and apply the whitelist plus `extra`.
fn apply_scrubbed_env(cmd: &mut Command, extra: &[(String, String)]) {
    cmd.env_clear();
    for key in ENV_PASSTHROUGH {
        if let Ok(value) = std::env::var(key) {
            cmd.env(key, value);
        }
    }
    for (key, value) in extra {
        cmd.env(key, value);
    }
}

/// Kill a spawned child and its whole process group (best effort).
///
/// The child was started with `process_group(0)`, so its PID is the PGID.
fn kill_process_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // Safety: kill(2) with a negative PID signals the process group.
            unsafe {
                let _ = libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
    // Fallback (and non-unix): kill the direct child only.
    let _ = child.start_kill();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_native_runtime_available() {
        let runtime = NativeRuntime::new();
        assert!(runtime.is_available().await);
    }

    #[tokio::test]
    async fn test_native_runtime_name() {
        let runtime = NativeRuntime::new();
        assert_eq!(runtime.name(), "native");
    }

    #[tokio::test]
    async fn test_native_runtime_echo() {
        let runtime = NativeRuntime::new();
        let config = ContainerConfig::new();

        let output = runtime.execute("echo hello", &config).await.unwrap();
        assert!(output.success());
        assert_eq!(output.stdout.trim(), "hello");
    }

    #[tokio::test]
    async fn test_native_runtime_with_workdir() {
        let runtime = NativeRuntime::new();
        let config = ContainerConfig::new().with_workdir(std::path::PathBuf::from("/tmp"));

        let output = runtime.execute("pwd", &config).await.unwrap();
        assert!(output.success());
        // On macOS /tmp is symlinked to /private/tmp
        assert!(output.stdout.contains("tmp"));
    }

    #[tokio::test]
    async fn test_native_runtime_with_env() {
        let runtime = NativeRuntime::new();
        let config = ContainerConfig::new().with_env("TEST_VAR", "test_value");

        let output = runtime.execute("echo $TEST_VAR", &config).await.unwrap();
        assert!(output.success());
        assert_eq!(output.stdout.trim(), "test_value");
    }

    #[tokio::test]
    async fn test_native_runtime_stderr() {
        let runtime = NativeRuntime::new();
        let config = ContainerConfig::new();

        let output = runtime.execute("echo error >&2", &config).await.unwrap();
        assert!(output.success());
        assert!(output.stderr.contains("error"));
    }

    #[tokio::test]
    async fn test_native_runtime_exit_code() {
        let runtime = NativeRuntime::new();
        let config = ContainerConfig::new();

        let output = runtime.execute("exit 42", &config).await.unwrap();
        assert!(!output.success());
        assert_eq!(output.exit_code, Some(42));
    }

    #[tokio::test]
    async fn test_native_runtime_timeout() {
        let runtime = NativeRuntime::new();
        let config = ContainerConfig::new().with_timeout(1);

        let result = runtime.execute("sleep 10", &config).await;
        assert!(matches!(result, Err(RuntimeError::Timeout(1))));
    }

    #[tokio::test]
    async fn test_native_runtime_env_scrubbed() {
        std::env::set_var("ZEPTOCLAW_SCRUB_PROBE", "leak");
        let rt = NativeRuntime::new();
        let config = ContainerConfig::new();
        let out = rt
            .execute("echo ${ZEPTOCLAW_SCRUB_PROBE:-clean}", &config)
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "clean");
    }
}

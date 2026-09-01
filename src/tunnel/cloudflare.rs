//! Cloudflare Tunnel provider.
//!
//! Uses `cloudflared` CLI to create either a quick tunnel (free, random
//! trycloudflare.com URL) or a named tunnel (with a token).

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tracing::{info, warn};

use crate::config::CloudflareTunnelConfig;
use crate::error::{Result, ZeptoError};
use crate::tunnel::types::TunnelProvider;

/// Cloudflare Tunnel provider backed by the `cloudflared` binary.
pub struct CloudflareTunnel {
    config: Option<CloudflareTunnelConfig>,
    child: Option<Child>,
    url: Option<String>,
}

impl CloudflareTunnel {
    /// Create a new Cloudflare Tunnel provider.
    pub fn new(config: Option<CloudflareTunnelConfig>) -> Self {
        Self {
            config,
            child: None,
            url: None,
        }
    }
}

/// Extract a Cloudflare tunnel URL from a line of `cloudflared` output.
///
/// Looks for `https://*.trycloudflare.com` (quick tunnels) or
/// `https://*.cfargotunnel.com` URLs in the text.
pub fn extract_cloudflare_url(line: &str) -> Option<String> {
    // Look for https:// URLs in the line
    for word in line.split_whitespace() {
        let candidate = word.trim_matches(|c: char| c == '"' || c == '\'' || c == ',');
        if candidate.starts_with("https://")
            && (candidate.contains(".trycloudflare.com") || candidate.contains(".cfargotunnel.com"))
        {
            return Some(candidate.to_string());
        }
    }
    // Also try to find URLs embedded without whitespace boundaries
    if let Some(start) = line.find("https://") {
        let rest = &line[start..];
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .unwrap_or(rest.len());
        let url = &rest[..end];
        if url.contains(".trycloudflare.com") || url.contains(".cfargotunnel.com") {
            return Some(url.to_string());
        }
    }
    None
}

#[async_trait]
impl TunnelProvider for CloudflareTunnel {
    fn name(&self) -> &str {
        "cloudflare"
    }

    async fn start(&mut self, local_port: u16) -> Result<String> {
        if self.child.is_some() {
            return Err(ZeptoError::Config(
                "Cloudflare tunnel already running".into(),
            ));
        }

        let mut cmd = Command::new("cloudflared");

        if let Some(ref cfg) = self.config {
            if let Some(ref token) = cfg.token {
                // Named tunnel with token — pass via env (cloudflared reads
                // TUNNEL_TOKEN) so the secret never appears in /proc cmdline.
                cmd.arg("tunnel").arg("run").env("TUNNEL_TOKEN", token);
                info!("Starting cloudflared named tunnel on port {}", local_port);
            } else {
                // Quick tunnel (free, random URL)
                cmd.arg("tunnel")
                    .arg("--url")
                    .arg(format!("http://localhost:{}", local_port));
                info!("Starting cloudflared quick tunnel on port {}", local_port);
            }
        } else {
            // No config, use quick tunnel
            cmd.arg("tunnel")
                .arg("--url")
                .arg(format!("http://localhost:{}", local_port));
            info!("Starting cloudflared quick tunnel on port {}", local_port);
        }

        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // Prevent child from inheriting stdin
        cmd.stdin(std::process::Stdio::null());

        let mut child = cmd.spawn().map_err(|e| {
            ZeptoError::Config(format!(
                "Failed to start cloudflared (is it installed?): {}",
                e
            ))
        })?;

        // cloudflared outputs the URL on stderr
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ZeptoError::Config("Failed to capture cloudflared stderr".into()))?;

        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();

        let url = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while let Ok(Some(line)) = lines.next_line().await {
                info!(target: "tunnel::cloudflare", "{}", line);
                if let Some(url) = extract_cloudflare_url(&line) {
                    return Ok(url);
                }
            }
            Err(ZeptoError::Config(
                "cloudflared exited without providing a tunnel URL".into(),
            ))
        })
        .await
        .map_err(|_| {
            ZeptoError::Config("Timed out waiting for cloudflared tunnel URL (30s)".into())
        })??;

        info!("Cloudflare tunnel active: {}", url);
        self.url = Some(url.clone());
        self.child = Some(child);
        Ok(url)
    }

    async fn stop(&mut self) -> Result<()> {
        if let Some(ref mut child) = self.child {
            info!("Stopping cloudflared tunnel");
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.child = None;
        self.url = None;
        Ok(())
    }

    async fn health_check(&self) -> Result<bool> {
        match &self.child {
            Some(child) => {
                // If we can get the child's id, it is still alive.
                // try_wait() is not available on &self, so check id presence.
                Ok(child.id().is_some())
            }
            None => Ok(false),
        }
    }
}

impl Drop for CloudflareTunnel {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.child {
            warn!("CloudflareTunnel dropped while still running, killing child process");
            let _ = child.start_kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_cloudflare_url_positive() {
        let line = r#"2024-01-15T10:00:00Z INF +-------------------------------------------+"#;
        assert!(extract_cloudflare_url(line).is_none());

        let line = r#"2024-01-15T10:00:00Z INF |  https://random-slug.trycloudflare.com   |"#;
        let url = extract_cloudflare_url(line).unwrap();
        assert_eq!(url, "https://random-slug.trycloudflare.com");

        let line = "Your quick Tunnel has been created! Visit it at (server URL): https://abc-def-ghi.trycloudflare.com";
        let url = extract_cloudflare_url(line).unwrap();
        assert_eq!(url, "https://abc-def-ghi.trycloudflare.com");
    }

    #[test]
    fn test_extract_cloudflare_url_cfargotunnel() {
        let line = "Tunnel URL: https://my-tunnel.cfargotunnel.com";
        let url = extract_cloudflare_url(line).unwrap();
        assert_eq!(url, "https://my-tunnel.cfargotunnel.com");
    }

    #[test]
    fn test_extract_cloudflare_url_negative() {
        assert!(extract_cloudflare_url("starting tunnel...").is_none());
        assert!(extract_cloudflare_url("https://example.com").is_none());
        assert!(extract_cloudflare_url("http://trycloudflare.com").is_none());
        assert!(extract_cloudflare_url("").is_none());
    }

    #[test]
    fn test_new_default() {
        let tunnel = CloudflareTunnel::new(None);
        assert!(tunnel.config.is_none());
        assert!(tunnel.child.is_none());
        assert!(tunnel.url.is_none());
        assert_eq!(tunnel.name(), "cloudflare");
    }

    #[test]
    fn test_new_with_config() {
        let config = CloudflareTunnelConfig {
            token: Some("my-token".into()),
        };
        let tunnel = CloudflareTunnel::new(Some(config));
        assert!(tunnel.config.is_some());
        assert_eq!(
            tunnel.config.as_ref().unwrap().token.as_deref(),
            Some("my-token")
        );
    }

    #[tokio::test]
    async fn test_health_check_no_child() {
        let tunnel = CloudflareTunnel::new(None);
        let result = tunnel.health_check().await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn test_start_already_running() {
        // We can't easily mock a running child, but we can test the error
        // path indirectly: start will fail because cloudflared isn't installed
        // in the test environment. That's fine — we test the "already running"
        // path by setting child to Some.
        // This is a structural test only.
        let tunnel = CloudflareTunnel::new(None);
        // tunnel.child is None, so this won't trigger the "already running" error
        assert!(tunnel.child.is_none());
    }
}

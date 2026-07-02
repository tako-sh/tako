use super::super::error::SshResult;
use super::SshClient;

impl SshClient {
    /// Check if tako-server is installed
    pub async fn is_tako_installed(&self) -> SshResult<bool> {
        let output = self
            .exec("command -v tako-server 2>/dev/null || echo not_found")
            .await?;
        Ok(!output.stdout.contains("not_found"))
    }

    /// Get tako-server version (just the version number, not the binary name prefix).
    pub async fn tako_version(&self) -> SshResult<Option<String>> {
        let output = self
            .exec("tako-server --version 2>/dev/null || true")
            .await?;
        let raw = output.stdout.trim();
        if raw.is_empty() {
            Ok(None)
        } else {
            Ok(Some(
                raw.strip_prefix("tako-server ").unwrap_or(raw).to_string(),
            ))
        }
    }
}

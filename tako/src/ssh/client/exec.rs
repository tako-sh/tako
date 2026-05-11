use russh::ChannelMsg;

use super::super::error::{SshError, SshResult};
use super::{CommandOutput, SshClient};

impl SshClient {
    /// Execute a command on the remote server
    pub async fn exec(&self, command: &str) -> SshResult<CommandOutput> {
        self.exec_with_stdin(command, &[]).await
    }

    /// Execute a command on the remote server while providing stdin bytes.
    pub(super) async fn exec_with_stdin(
        &self,
        command: &str,
        stdin: &[u8],
    ) -> SshResult<CommandOutput> {
        let handle = self.handle.as_ref().ok_or(SshError::NotConnected)?;

        let mut channel = handle
            .channel_open_session()
            .await
            .map_err(|e| SshError::Channel(e.to_string()))?;

        channel
            .exec(true, command)
            .await
            .map_err(|e| SshError::CommandFailed(e.to_string()))?;

        if !stdin.is_empty() {
            channel
                .data(stdin)
                .await
                .map_err(|e| SshError::CommandFailed(e.to_string()))?;
            channel
                .eof()
                .await
                .map_err(|e| SshError::CommandFailed(e.to_string()))?;
        }

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = 0u32;
        let mut got_exit_status = false;

        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    stdout.extend_from_slice(&data);
                }
                Some(ChannelMsg::ExtendedData { data, ext }) if ext == 1 => {
                    stderr.extend_from_slice(&data);
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    exit_code = exit_status;
                    got_exit_status = true;
                }
                Some(ChannelMsg::Eof) => {}
                None => break,
                _ => {}
            }
        }

        if !got_exit_status {
            exit_code = 255;
        }

        Ok(CommandOutput {
            exit_code,
            stdout: String::from_utf8_lossy(&stdout).to_string(),
            stderr: String::from_utf8_lossy(&stderr).to_string(),
        })
    }

    /// Execute a command and return error if it fails
    pub async fn exec_checked(&self, command: &str) -> SshResult<CommandOutput> {
        self.exec_checked_with_stdin(command, &[]).await
    }

    pub(super) async fn exec_checked_with_stdin(
        &self,
        command: &str,
        stdin: &[u8],
    ) -> SshResult<CommandOutput> {
        let output = self.exec_with_stdin(command, stdin).await?;

        if !output.success() {
            return Err(SshError::NonZeroExit {
                code: output.exit_code,
                stderr: output.stderr.clone(),
            });
        }

        Ok(output)
    }

    /// Execute a command and stream output to callbacks
    pub async fn exec_streaming<F, G>(
        &self,
        command: &str,
        mut on_stdout: F,
        mut on_stderr: G,
    ) -> SshResult<u32>
    where
        F: FnMut(&[u8]),
        G: FnMut(&[u8]),
    {
        let handle = self.handle.as_ref().ok_or(SshError::NotConnected)?;

        let mut channel = handle
            .channel_open_session()
            .await
            .map_err(|e| SshError::Channel(e.to_string()))?;

        channel
            .exec(true, command)
            .await
            .map_err(|e| SshError::CommandFailed(e.to_string()))?;

        let mut exit_code = 0u32;

        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    on_stdout(&data);
                }
                Some(ChannelMsg::ExtendedData { data, ext }) if ext == 1 => {
                    on_stderr(&data);
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    exit_code = exit_status;
                }
                Some(ChannelMsg::Eof) => {}
                None => break,
                _ => {}
            }
        }

        Ok(exit_code)
    }
}

// todo: add support for signal handling
use std::process::Stdio;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time;

use crate::terminal::runner::{
    AsyncTerminalRunner, TerminalError, TerminalRunRequest, TerminalRunResult,
};

pub struct LocalTerminalRunner;

async fn read_stream(mut stream: impl AsyncReadExt + Unpin) -> Vec<u8> {
    let mut buffer = Vec::new();
    let _ = stream.read_to_end(&mut buffer).await;
    buffer
}

/// Truncate buffer to keep only the last `limit` bytes (drops oldest bytes).
/// Returns true if truncation occurred.
///
/// This function ensures we don't split UTF-8 characters when truncating
/// by using char_indices to find a valid character boundary.
fn truncate_buffer(buf: &mut Vec<u8>, limit: usize) -> bool {
    if buf.len() > limit {
        // Convert to string to work with character boundaries
        let s = String::from_utf8_lossy(buf);
        let excess = buf.len().saturating_sub(limit);

        // Find the first char boundary at or after `excess` bytes
        let start_idx = s
            .char_indices()
            .find(|(i, _)| *i >= excess)
            .map(|(i, _)| i)
            .unwrap_or(s.len());

        // Slice from that boundary and update buffer
        *buf = s[start_idx..].as_bytes().to_vec();

        true
    } else {
        false
    }
}

#[async_trait::async_trait]
impl AsyncTerminalRunner for LocalTerminalRunner {
    async fn run(&self, request: TerminalRunRequest) -> Result<TerminalRunResult, TerminalError> {
        // Build and spawn the command via the platform shell.
        #[cfg(unix)]
        let mut cmd = {
            let mut c = Command::new(crate::terminal::default_shell_path());
            c.arg("-lc").arg(&request.command);
            c
        };
        #[cfg(not(unix))]
        let mut cmd = {
            let inv = xai_grok_config::shell::shell_command_argv(&request.command);
            let mut c = Command::new(inv.program);
            c.args(&inv.args).envs(inv.env);
            c
        };
        cmd.current_dir(&request.cwd)
            .envs(&request.env)
            .envs(crate::terminal::pager_env())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Detach from the controlling terminal so child processes
        // (e.g. GPG pinentry) cannot open /dev/tty and corrupt the TUI.
        xai_grok_tools::util::detach_command(&mut cmd);

        #[allow(clippy::disallowed_methods)] // killed on the request timeout, direct child only
        let mut child = cmd
            .spawn()
            .map_err(|e| TerminalError::Other(format!("Failed to start shell: {e}")))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TerminalError::Other("Failed to capture stdout".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| TerminalError::Other("Failed to capture stderr".into()))?;

        let stdout_task = tokio::spawn(read_stream(stdout));
        let stderr_task = tokio::spawn(read_stream(stderr));

        let mut timed_out = false;

        let wait_result = time::timeout(request.timeout, child.wait()).await;
        let exit_status = match wait_result {
            Ok(status_res) => status_res
                .map_err(|e| TerminalError::Other(format!("Failed to wait for process: {e}")))?,
            Err(_) => {
                timed_out = true;
                if let Err(e) = child.start_kill() {
                    tracing::warn!("Failed to kill timed-out process: {e}");
                }
                child.wait().await.map_err(|e| {
                    TerminalError::Other(format!("Failed to wait for killed process: {e}"))
                })?
            }
        };

        let stdout_result = stdout_task.await.unwrap_or_else(|_| Vec::new());
        let stderr_result = stderr_task.await.unwrap_or_else(|_| Vec::new());

        // Combine stdout and stderr, then truncate if needed
        let mut combined = stdout_result;
        combined.extend(stderr_result);
        let truncated = truncate_buffer(&mut combined, request.output_byte_limit);

        let combined_output = String::from_utf8_lossy(&combined).into_owned();

        Ok(TerminalRunResult {
            combined_output,
            exit_code: exit_status.code(),
            truncated,
            signal: None,
            timed_out,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::DEFAULT_OUTPUT_BYTE_LIMIT;
    use crate::terminal::runner::TerminalRunRequest;
    use std::collections::HashMap;
    use xai_grok_paths::AbsPathBuf;

    fn make_request(command: &str) -> TerminalRunRequest {
        TerminalRunRequest {
            tool_call_id: agent_client_protocol::ToolCallId::new("test"),
            command: command.to_string(),
            cwd: AbsPathBuf::new(std::env::current_dir().unwrap()).unwrap(),
            env: HashMap::new(),
            timeout: std::time::Duration::from_secs(10),
            output_byte_limit: DEFAULT_OUTPUT_BYTE_LIMIT,
            stream: false,
            output_file: None,
        }
    }

    /// Verify that `detach_from_tty` prevents child processes from opening
    /// `/dev/tty`. After setsid(), the child has no controlling terminal.
    #[tokio::test]
    #[cfg(unix)]
    async fn test_child_cannot_open_dev_tty() {
        // Skip in CI / environments without a controlling terminal.
        if std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/tty")
            .is_err()
        {
            eprintln!("skipping: no controlling terminal");
            return;
        }

        let result = LocalTerminalRunner
            .run(make_request(
                "(exec 3>/dev/tty && echo ATTACHED || echo DETACHED) 2>/dev/null",
            ))
            .await
            .unwrap();

        assert_eq!(
            result.combined_output.trim(),
            "DETACHED",
            "child process should not be able to open /dev/tty after detach_from_tty()"
        );
    }

    /// Basic regression: commands still produce output and exit normally.
    #[tokio::test]
    async fn test_basic_command_output() {
        let result = LocalTerminalRunner
            .run(make_request("echo hello"))
            .await
            .unwrap();

        assert_eq!(result.combined_output.trim(), "hello");
        assert_eq!(result.exit_code, Some(0));
    }
}

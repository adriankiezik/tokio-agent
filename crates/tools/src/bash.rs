use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Child;
use tokio_agent_core::provider::BoxFuture;
use tokio_agent_core::tool::{Action, PermissionRequest, Tool, ToolCtx, ToolDef, ToolResult};

pub struct Bash;

#[derive(Debug, Deserialize)]
struct Args {
    command: String,
}

impl Tool for Bash {
    fn schema(&self) -> ToolDef {
        ToolDef {
            name: "bash".to_owned(),
            description: "Run a command with `bash -c` in the working directory. Returns combined \
                stdout and stderr with the exit status."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The command to run." }
                },
                "required": ["command"]
            }),
        }
    }

    fn permission(&self, input: &Value) -> PermissionRequest {
        let command = input
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("<missing>");
        PermissionRequest {
            tool: "bash".to_owned(),
            summary: format!("run: {command}"),
            action: Action::Execute,
        }
    }

    fn run<'a>(&'a self, input: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            if ctx.cancel.is_cancelled() {
                return ToolResult::error("cancelled by user");
            }
            let args: Args = match serde_json::from_value(input) {
                Ok(a) => a,
                Err(e) => return ToolResult::error(format!("invalid arguments: {e}")),
            };

            let mut command = tokio::process::Command::new("bash");
            command
                .arg("-c")
                .arg(&args.command)
                .current_dir(&ctx.cwd)
                .kill_on_drop(true)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            #[cfg(unix)]
            command.process_group(0);

            let child = match command.spawn() {
                Ok(c) => c,
                Err(e) => return ToolResult::error(format!("failed to spawn bash: {e}")),
            };

            let output = wait_for_output(child, ctx.cancel.clone()).await;

            match output {
                Ok(out) => {
                    let mut buf = String::new();
                    buf.push_str(&String::from_utf8_lossy(&out.stdout));
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    if !stderr.is_empty() {
                        if !buf.is_empty() && !buf.ends_with('\n') {
                            buf.push('\n');
                        }
                        buf.push_str(&stderr);
                    }
                    let code = out.status.code().unwrap_or(-1);
                    if out.status.success() {
                        ToolResult::ok(buf)
                    } else {
                        ToolResult::error(format!("{buf}\n[exit status {code}]"))
                    }
                }
                Err(WaitError::Cancelled) => ToolResult::error("cancelled by user"),
                Err(WaitError::Io(e)) => ToolResult::error(format!("bash failed: {e}")),
            }
        })
    }
}

enum WaitError {
    Cancelled,
    Io(std::io::Error),
}

#[cfg(unix)]
async fn wait_for_output(
    child: Child,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<std::process::Output, WaitError> {
    let pid = child.id();
    let mut wait = Box::pin(child.wait_with_output());
    tokio::select! {
        out = &mut wait => out.map_err(WaitError::Io),
        () = cancel.cancelled() => {
            terminate_process_tree(pid);
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            kill_process_tree(pid);
            let _ = wait.await;
            Err(WaitError::Cancelled)
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_output(
    child: Child,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<std::process::Output, WaitError> {
    let mut wait = tokio::spawn(child.wait_with_output());
    tokio::select! {
        result = &mut wait => result
            .map_err(|e| WaitError::Io(std::io::Error::other(e)))?
            .map_err(WaitError::Io),
        () = cancel.cancelled() => {
            wait.abort();
            let _ = wait.await;
            Err(WaitError::Cancelled)
        }
    }
}

#[cfg(unix)]
fn terminate_process_tree(pid: Option<u32>) {
    if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        );
    }
}

#[cfg(unix)]
fn kill_process_tree(pid: Option<u32>) {
    if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_stops_a_running_shell_promptly() {
        let mut command = tokio::process::Command::new("bash");
        command
            .arg("-c")
            .arg("sleep 30")
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        let child = command.spawn().expect("spawn bash");
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_task = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            cancel_task.cancel();
        });

        let started = std::time::Instant::now();
        let result = wait_for_output(child, cancel).await;
        assert!(matches!(result, Err(WaitError::Cancelled)));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_group_even_when_shell_exits_during_grace_period() {
        let sentinel = std::env::temp_dir().join(format!(
            "tokio-agent-bash-survivor-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut command = tokio::process::Command::new("bash");
        command
            .arg("-c")
            .arg(
                "trap 'exit 0' TERM; \
                 (trap '' TERM; exec >/dev/null 2>&1; sleep 1; touch \"$SENTINEL\") & wait",
            )
            .env("SENTINEL", &sentinel)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0);
        let child = command.spawn().expect("spawn bash");
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_task = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel_task.cancel();
        });

        assert!(matches!(
            wait_for_output(child, cancel).await,
            Err(WaitError::Cancelled)
        ));
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        assert!(
            !sentinel.exists(),
            "a descendant survived process-group cancellation"
        );
        let _ = std::fs::remove_file(sentinel);
    }
}

use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Child;
use tokio::sync::Notify;
use tokio_agent_core::provider::BoxFuture;
use tokio_agent_core::tool::{Tool, ToolCtx, ToolDef, ToolEffect, ToolResult};
use tokio_util::sync::CancellationToken;

const MIN_YIELD_MS: u64 = 250;
const MAX_YIELD_MS: u64 = 30_000;
const DEFAULT_YIELD_MS: u64 = 10_000;
const DEFAULT_TIMEOUT_MS: u64 = 10 * 60 * 1_000;
const MAX_RETAINED_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_MANAGED_PROCESSES: usize = 64;
const TERMINATION_GRACE: Duration = Duration::from_millis(500);
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(100);
const TERMINATION_WAIT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy)]
pub struct BashConfig {
    pub yield_time_ms: u64,
    pub timeout_ms: u64,
}

impl Default for BashConfig {
    fn default() -> Self {
        Self {
            yield_time_ms: DEFAULT_YIELD_MS,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

#[derive(Clone, Default)]
pub struct Bash {
    manager: ProcessManager,
    config: BashConfig,
}

#[derive(Clone)]
pub struct BashWait {
    manager: ProcessManager,
}

#[derive(Clone)]
pub struct BashKill {
    manager: ProcessManager,
}

pub fn tools(config: BashConfig) -> [Arc<dyn Tool>; 3] {
    let manager = ProcessManager::default();
    [
        Arc::new(Bash {
            manager: manager.clone(),
            config,
        }),
        Arc::new(BashWait {
            manager: manager.clone(),
        }),
        Arc::new(BashKill { manager }),
    ]
}

#[derive(Debug, Deserialize)]
struct Args {
    command: String,
    #[serde(default)]
    yield_time_ms: Option<u64>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ProcessArgs {
    process_id: u64,
    #[serde(default)]
    yield_time_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct KillArgs {
    process_id: u64,
}

impl Tool for Bash {
    fn schema(&self) -> ToolDef {
        ToolDef {
            name: "bash".to_owned(),
            description: format!(
                "Run a command with `bash -c` in the working directory. Returns when the command \
                 exits or after a short yield window, in which case it returns a process ID for \
                 `bash_wait` or `bash_kill`. The default yield is {} ms and the default hard \
                 timeout is {} ms.",
                clamp_yield(self.config.yield_time_ms),
                self.config.timeout_ms,
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The command to run." },
                    "yield_time_ms": {
                        "type": "integer",
                        "minimum": MIN_YIELD_MS,
                        "maximum": MAX_YIELD_MS,
                        "description": "How long to wait before returning a process ID for a command that is still running."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum total command runtime before the process group is terminated."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Execute
    }

    fn summary(&self, input: &Value) -> Option<String> {
        input
            .get("command")
            .and_then(Value::as_str)
            .map(|command| format!("run: {command}"))
    }

    fn run<'a>(&'a self, input: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            if ctx.cancel.is_cancelled() {
                return ToolResult::error("cancelled by user");
            }
            let args: Args = match serde_json::from_value(input) {
                Ok(args) => args,
                Err(error) => return ToolResult::error(format!("invalid arguments: {error}")),
            };
            let timeout_ms = args.timeout_ms.unwrap_or(self.config.timeout_ms);
            if timeout_ms == 0 {
                return ToolResult::error("timeout_ms must be greater than zero");
            }
            let yield_time_ms =
                clamp_yield(args.yield_time_ms.unwrap_or(self.config.yield_time_ms));
            let progress = ctx.progress_callback();
            let entry = match self
                .manager
                .spawn(
                    &args.command,
                    &ctx.cwd,
                    Duration::from_millis(timeout_ms),
                    ctx.cancel.clone(),
                    progress,
                )
                .await
            {
                Ok(entry) => entry,
                Err(error) => return ToolResult::error(error),
            };

            let snapshot = entry
                .wait(Duration::from_millis(yield_time_ms), &ctx.cancel)
                .await;
            entry.stop_reporting_progress();
            if ctx.cancel.is_cancelled() {
                entry.cancel();
                let _ = entry
                    .wait(TERMINATION_WAIT, &CancellationToken::new())
                    .await;
                self.manager.remove(entry.id);
                return ToolResult::error("cancelled by user");
            }

            let result = format_snapshot(entry.id, snapshot);
            if result.terminal {
                self.manager.remove(entry.id);
            }
            if result.is_error {
                ToolResult::error(result.text)
            } else {
                ToolResult::ok(result.text)
            }
        })
    }
}

impl Tool for BashWait {
    fn schema(&self) -> ToolDef {
        ToolDef {
            name: "bash_wait".to_owned(),
            description: "Wait briefly for a background Bash process and return new output. Returns immediately when the process exits. Polls are capped at 30 seconds.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "process_id": { "type": "integer", "description": "Process ID returned by `bash`." },
                    "yield_time_ms": {
                        "type": "integer",
                        "minimum": MIN_YIELD_MS,
                        "maximum": MAX_YIELD_MS,
                        "description": "How long to wait for completion before yielding again. Defaults to 5000 ms."
                    }
                },
                "required": ["process_id"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    fn summary(&self, input: &Value) -> Option<String> {
        Some(format!("poll process: {}", process_id_for_summary(input)))
    }

    fn run<'a>(&'a self, input: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let args: ProcessArgs = match serde_json::from_value(input) {
                Ok(args) => args,
                Err(error) => return ToolResult::error(format!("invalid arguments: {error}")),
            };
            let Some(entry) = self.manager.get(args.process_id) else {
                return ToolResult::error(format!("unknown bash process ID {}", args.process_id));
            };
            let wait = Duration::from_millis(clamp_yield(args.yield_time_ms.unwrap_or(5_000)));
            let snapshot = entry.wait(wait, &ctx.cancel).await;
            if ctx.cancel.is_cancelled() {
                return ToolResult::error("cancelled by user");
            }
            let result = format_snapshot(entry.id, snapshot);
            if result.terminal {
                self.manager.remove(entry.id);
            }
            if result.is_error {
                ToolResult::error(result.text)
            } else {
                ToolResult::ok(result.text)
            }
        })
    }
}

impl Tool for BashKill {
    fn schema(&self) -> ToolDef {
        ToolDef {
            name: "bash_kill".to_owned(),
            description: "Terminate a background Bash process and its process group.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "process_id": { "type": "integer", "description": "Process ID returned by `bash`." }
                },
                "required": ["process_id"]
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    fn summary(&self, input: &Value) -> Option<String> {
        Some(format!(
            "terminate process: {}",
            process_id_for_summary(input)
        ))
    }

    fn run<'a>(&'a self, input: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let args: KillArgs = match serde_json::from_value(input) {
                Ok(args) => args,
                Err(error) => return ToolResult::error(format!("invalid arguments: {error}")),
            };
            let Some(entry) = self.manager.get(args.process_id) else {
                return ToolResult::error(format!("unknown bash process ID {}", args.process_id));
            };
            entry.cancel();
            let snapshot = entry.wait(TERMINATION_WAIT, &ctx.cancel).await;
            let result = format_snapshot(entry.id, snapshot);
            if result.terminal {
                self.manager.remove(entry.id);
            }
            ToolResult::ok(result.text)
        })
    }
}

fn process_id_for_summary(input: &Value) -> String {
    input
        .get("process_id")
        .and_then(Value::as_u64)
        .map_or_else(|| "<missing>".to_owned(), |id| id.to_string())
}

fn clamp_yield(value: u64) -> u64 {
    value.clamp(MIN_YIELD_MS, MAX_YIELD_MS)
}

#[derive(Clone, Default)]
struct ProcessManager {
    inner: Arc<ManagerInner>,
}

#[derive(Default)]
struct ManagerInner {
    next_id: AtomicU64,
    processes: Mutex<HashMap<u64, Arc<ProcessEntry>>>,
}

impl Drop for ManagerInner {
    fn drop(&mut self) {
        let processes = self
            .processes
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for entry in processes.values() {
            entry.cancel();
        }
    }
}

impl ProcessManager {
    async fn spawn(
        &self,
        command_text: &str,
        cwd: &std::path::Path,
        timeout: Duration,
        turn_cancel: CancellationToken,
        progress: Option<Arc<dyn Fn(String) + Send + Sync>>,
    ) -> Result<Arc<ProcessEntry>, String> {
        let mut command = tokio::process::Command::new("bash");
        remove_cargo_build_context(&mut command);
        command
            .arg("-c")
            .arg(command_text)
            .current_dir(cwd)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to spawn bash: {error}"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to capture bash stdout".to_owned())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "failed to capture bash stderr".to_owned())?;

        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let entry = Arc::new(ProcessEntry::new(id, progress));
        let at_capacity = {
            let mut processes = self
                .inner
                .processes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut at_capacity = false;
            if processes.len() >= MAX_MANAGED_PROCESSES {
                let evict = processes
                    .iter()
                    .filter(|(_, entry)| !entry.is_running())
                    .min_by_key(|(_, entry)| entry.started)
                    .map(|(id, _)| *id);
                if let Some(evict) = evict {
                    processes.remove(&evict);
                } else {
                    at_capacity = true;
                }
            }
            if !at_capacity {
                processes.insert(id, Arc::clone(&entry));
            }
            at_capacity
        };
        if at_capacity {
            terminate_process_tree(&mut child).await;
            return Err(format!(
                "too many running Bash processes (maximum {MAX_MANAGED_PROCESSES})"
            ));
        }
        let task_entry = Arc::clone(&entry);
        tokio::spawn(async move {
            drive_process(child, stdout, stderr, timeout, turn_cancel, task_entry).await;
        });
        Ok(entry)
    }

    fn get(&self, id: u64) -> Option<Arc<ProcessEntry>> {
        self.inner
            .processes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .cloned()
    }

    fn remove(&self, id: u64) {
        self.inner
            .processes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    }
}

struct ProcessEntry {
    id: u64,
    started: Instant,
    state: Mutex<ProcessState>,
    completed: Notify,
    cancel: CancellationToken,
    report_progress: AtomicBool,
    progress: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

impl ProcessEntry {
    fn new(id: u64, progress: Option<Arc<dyn Fn(String) + Send + Sync>>) -> Self {
        Self {
            id,
            started: Instant::now(),
            state: Mutex::new(ProcessState::default()),
            completed: Notify::new(),
            cancel: CancellationToken::new(),
            report_progress: AtomicBool::new(true),
            progress,
        }
    }

    fn push_output(&self, bytes: &[u8]) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .output
            .push(bytes);
        if self.report_progress.load(Ordering::Acquire)
            && let Some(progress) = &self.progress
        {
            progress(String::from_utf8_lossy(bytes).into_owned());
        }
    }

    fn finish(&self, status: ProcessStatus) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status = status;
        self.completed.notify_waiters();
        self.completed.notify_one();
    }

    fn cancel(&self) {
        self.cancel.cancel();
    }

    fn stop_reporting_progress(&self) {
        self.report_progress.store(false, Ordering::Release);
    }

    async fn wait(&self, duration: Duration, caller_cancel: &CancellationToken) -> Snapshot {
        if self.is_running() {
            tokio::select! {
                () = self.completed.notified() => {},
                () = tokio::time::sleep(duration) => {},
                () = caller_cancel.cancelled() => {},
            }
        }
        self.snapshot()
    }

    fn is_running(&self) -> bool {
        matches!(
            self.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .status,
            ProcessStatus::Running
        )
    }

    fn snapshot(&self) -> Snapshot {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (output, omitted_bytes) = state.output.drain();
        Snapshot {
            status: state.status.clone(),
            elapsed: self.started.elapsed(),
            output,
            omitted_bytes,
        }
    }
}

#[derive(Default)]
struct ProcessState {
    status: ProcessStatus,
    output: OutputBuffer,
}

#[derive(Clone, Default)]
enum ProcessStatus {
    #[default]
    Running,
    Exited(i32),
    TimedOut,
    Killed,
    Failed(String),
}

#[derive(Default)]
struct OutputBuffer {
    bytes: VecDeque<u8>,
    omitted_bytes: usize,
}

impl OutputBuffer {
    fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= MAX_RETAINED_OUTPUT_BYTES {
            self.omitted_bytes = self
                .omitted_bytes
                .saturating_add(self.bytes.len())
                .saturating_add(bytes.len() - MAX_RETAINED_OUTPUT_BYTES);
            self.bytes.clear();
            self.bytes.extend(
                bytes[bytes.len() - MAX_RETAINED_OUTPUT_BYTES..]
                    .iter()
                    .copied(),
            );
            return;
        }
        let overflow = self
            .bytes
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(MAX_RETAINED_OUTPUT_BYTES);
        if overflow > 0 {
            self.bytes.drain(..overflow);
            self.omitted_bytes = self.omitted_bytes.saturating_add(overflow);
        }
        self.bytes.extend(bytes.iter().copied());
    }

    fn drain(&mut self) -> (Vec<u8>, usize) {
        let bytes = self.bytes.drain(..).collect();
        let omitted = std::mem::take(&mut self.omitted_bytes);
        (bytes, omitted)
    }
}

struct Snapshot {
    status: ProcessStatus,
    elapsed: Duration,
    output: Vec<u8>,
    omitted_bytes: usize,
}

struct FormattedResult {
    text: String,
    terminal: bool,
    is_error: bool,
}

fn format_snapshot(process_id: u64, snapshot: Snapshot) -> FormattedResult {
    let (status, exit_code, terminal, is_error, guidance) = match snapshot.status {
        ProcessStatus::Running => (
            "running",
            None,
            false,
            false,
            format!(
                "Process is still running. Use `bash_wait` with process_id {process_id} to poll it, or `bash_kill` to terminate it."
            ),
        ),
        ProcessStatus::Exited(code) => ("exited", Some(code), true, code != 0, String::new()),
        ProcessStatus::TimedOut => (
            "timed_out",
            None,
            true,
            true,
            "The command exceeded its hard runtime limit and its process group was terminated."
                .to_owned(),
        ),
        ProcessStatus::Killed => (
            "killed",
            None,
            true,
            false,
            "The command and its process group were terminated.".to_owned(),
        ),
        ProcessStatus::Failed(error) => ("failed", None, true, true, error),
    };
    let output = String::from_utf8_lossy(&snapshot.output);
    let omission = if snapshot.omitted_bytes > 0 {
        format!(
            "[... {} earlier output bytes omitted ...]\n",
            snapshot.omitted_bytes
        )
    } else {
        String::new()
    };
    let exit = exit_code
        .map(|code| format!(", exit_code: {code}"))
        .unwrap_or_default();
    let guidance = if guidance.is_empty() {
        String::new()
    } else {
        format!("\n{guidance}")
    };
    FormattedResult {
        text: format!(
            "Process {process_id} {status} after {:.1}s{exit}.\n{omission}{output}{guidance}",
            snapshot.elapsed.as_secs_f64(),
        ),
        terminal,
        is_error,
    }
}

async fn drive_process(
    mut child: Child,
    stdout: impl AsyncRead + Unpin + Send + 'static,
    stderr: impl AsyncRead + Unpin + Send + 'static,
    timeout: Duration,
    turn_cancel: CancellationToken,
    entry: Arc<ProcessEntry>,
) {
    let stdout_task = tokio::spawn(read_stream(stdout, Arc::clone(&entry)));
    let stderr_task = tokio::spawn(read_stream(stderr, Arc::clone(&entry)));

    let status = tokio::select! {
        result = child.wait() => match result {
            Ok(status) => ProcessStatus::Exited(status.code().unwrap_or(-1)),
            Err(error) => ProcessStatus::Failed(format!("bash failed: {error}")),
        },
        () = tokio::time::sleep(timeout) => {
            terminate_process_tree(&mut child).await;
            ProcessStatus::TimedOut
        },
        () = entry.cancel.cancelled() => {
            terminate_process_tree(&mut child).await;
            ProcessStatus::Killed
        },
        () = turn_cancel.cancelled() => {
            terminate_process_tree(&mut child).await;
            ProcessStatus::Killed
        },
    };

    drain_reader(stdout_task).await;
    drain_reader(stderr_task).await;
    entry.finish(status);
}

async fn read_stream(mut stream: impl AsyncRead + Unpin, entry: Arc<ProcessEntry>) {
    let mut buffer = [0_u8; 8192];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => entry.push_output(&buffer[..read]),
        }
    }
}

async fn drain_reader(mut task: tokio::task::JoinHandle<()>) {
    if tokio::time::timeout(OUTPUT_DRAIN_GRACE, &mut task)
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
    }
}

#[cfg(unix)]
async fn terminate_process_tree(child: &mut Child) {
    let pid = child.id();
    terminate_process_group(pid);
    let _ = tokio::time::timeout(TERMINATION_GRACE, child.wait()).await;
    kill_process_group(pid);
    let _ = child.wait().await;
}

#[cfg(not(unix))]
async fn terminate_process_tree(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(unix)]
fn terminate_process_group(pid: Option<u32>) {
    signal_process_group(pid, nix::sys::signal::Signal::SIGTERM);
}

#[cfg(unix)]
fn kill_process_group(pid: Option<u32>) {
    signal_process_group(pid, nix::sys::signal::Signal::SIGKILL);
}

#[cfg(unix)]
fn signal_process_group(pid: Option<u32>, signal: nix::sys::signal::Signal) {
    if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), signal);
    }
}

fn remove_cargo_build_context(command: &mut tokio::process::Command) {
    for (name, _) in std::env::vars_os() {
        if is_cargo_build_context(&name) {
            command.env_remove(name);
        }
    }
}

fn is_cargo_build_context(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    matches!(
        name,
        "CARGO_BIN_NAME"
            | "CARGO_CRATE_NAME"
            | "CARGO_MANIFEST_DIR"
            | "CARGO_MANIFEST_PATH"
            | "CARGO_PRIMARY_PACKAGE"
            | "CARGO_TARGET_TMPDIR"
            | "OUT_DIR"
            | "RUST_RECURSION_COUNT"
    ) || name.starts_with("CARGO_BIN_EXE_")
        || name.starts_with("CARGO_CFG_")
        || name.starts_with("CARGO_FEATURE_")
        || name.starts_with("CARGO_PKG_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx(cancel: CancellationToken) -> ToolCtx {
        ToolCtx::new(PathBuf::from("."), cancel)
    }

    fn text(result: &ToolResult) -> &str {
        let tokio_agent_core::message::ToolOutput::Text(text) = &result.output;
        text
    }

    #[test]
    fn identifies_cargo_build_context_without_matching_user_configuration() {
        assert!(is_cargo_build_context("CARGO_MANIFEST_DIR".as_ref()));
        assert!(is_cargo_build_context("CARGO_PKG_VERSION".as_ref()));
        assert!(is_cargo_build_context("CARGO_FEATURE_TLS".as_ref()));
        assert!(is_cargo_build_context("RUST_RECURSION_COUNT".as_ref()));
        assert!(!is_cargo_build_context("CARGO_HOME".as_ref()));
        assert!(!is_cargo_build_context("CARGO_TARGET_DIR".as_ref()));
        assert!(!is_cargo_build_context("RUSTFLAGS".as_ref()));
    }

    #[tokio::test]
    async fn shell_does_not_inherit_cargo_build_context() {
        assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
        let bash = Bash::default();
        let result = bash
            .run(
                json!({
                    "command": "printf '%s:%s' \"${CARGO_MANIFEST_DIR-unset}\" \"$CARGO_HOME\"",
                    "yield_time_ms": 2_000,
                    "timeout_ms": 2_000
                }),
                &ctx(CancellationToken::new()),
            )
            .await;
        assert!(!result.is_error, "{}", text(&result));
        assert!(text(&result).contains("unset:"));
    }

    #[tokio::test]
    async fn long_command_yields_and_can_be_polled() {
        let manager = ProcessManager::default();
        let bash = Bash {
            manager: manager.clone(),
            config: BashConfig::default(),
        };
        let wait = BashWait { manager };
        let context = ctx(CancellationToken::new());
        let started = Instant::now();
        let running = bash
            .run(
                json!({"command": "printf start; sleep 0.4; printf done", "yield_time_ms": 50, "timeout_ms": 2_000}),
                &context,
            )
            .await;
        assert!(started.elapsed() < Duration::from_millis(350));
        assert!(text(&running).contains("still running"));
        assert!(text(&running).contains("start"));

        let done = wait
            .run(json!({"process_id": 1, "yield_time_ms": 1_000}), &context)
            .await;
        assert!(!done.is_error, "{}", text(&done));
        assert!(text(&done).contains("exited"));
        assert!(text(&done).contains("done"));
    }

    #[tokio::test]
    async fn hard_timeout_terminates_command() {
        let bash = Bash::default();
        let context = ctx(CancellationToken::new());
        let result = bash
            .run(
                json!({"command": "sleep 30", "yield_time_ms": 1_000, "timeout_ms": 30}),
                &context,
            )
            .await;
        assert!(result.is_error);
        assert!(text(&result).contains("timed_out"));
    }

    #[tokio::test]
    async fn kill_stops_a_background_process() {
        let manager = ProcessManager::default();
        let bash = Bash {
            manager: manager.clone(),
            config: BashConfig::default(),
        };
        let kill = BashKill { manager };
        let context = ctx(CancellationToken::new());
        let running = bash
            .run(
                json!({"command": "sleep 30", "yield_time_ms": 10, "timeout_ms": 60_000}),
                &context,
            )
            .await;
        assert!(text(&running).contains("still running"));
        let result = kill.run(json!({"process_id": 1}), &context).await;
        assert!(!result.is_error);
        assert!(text(&result).contains("killed"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_descendants_that_ignore_term() {
        let sentinel = std::env::temp_dir().join(format!(
            "tokio-agent-bash-survivor-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bash = Bash::default();
        let context = ctx(CancellationToken::new());
        let command = format!(
            "trap 'exit 0' TERM; (trap '' TERM; exec >/dev/null 2>&1; sleep 1; touch '{}') & wait",
            sentinel.display()
        );
        let result = bash
            .run(
                json!({"command": command, "yield_time_ms": 1_000, "timeout_ms": 30}),
                &context,
            )
            .await;
        assert!(result.is_error);
        assert!(text(&result).contains("timed_out"));
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        assert!(!sentinel.exists(), "a descendant survived the hard timeout");
        let _ = std::fs::remove_file(sentinel);
    }

    #[test]
    fn output_buffer_keeps_a_bounded_tail() {
        let mut output = OutputBuffer::default();
        output.push(&vec![b'a'; MAX_RETAINED_OUTPUT_BYTES + 100]);
        let (bytes, omitted) = output.drain();
        assert_eq!(bytes.len(), MAX_RETAINED_OUTPUT_BYTES);
        assert_eq!(omitted, 100);
    }
}

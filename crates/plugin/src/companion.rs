use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio_agent_extension_api::{
    COMPANION_PROTOCOL_VERSION, HOST_API_VERSION, HostRequest, HostResponse,
};

#[derive(Debug, thiserror::Error)]
pub enum CompanionError {
    #[error("tokio-agent-extension-host could not be located")]
    NotFound,
    #[error("extension companion I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("extension companion protocol failed: {0}")]
    Protocol(String),
    #[error("extension host rejected the request: {0}")]
    Host(String),
    #[error("extension companion exited")]
    Exited,
    #[error("extension companion did not respond before its protocol deadline")]
    Timeout,
    #[error("extension companion repeatedly failed and is disabled for this session")]
    CircuitOpen,
}

struct RunningCompanion {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// Lazy, session-scoped companion lifecycle. The process receives an empty
pub struct CompanionManager {
    executable: Option<PathBuf>,
    cache_directory: Option<PathBuf>,
    running: Option<RunningCompanion>,
    restarted: bool,
    circuit_open: bool,
    loads: BTreeMap<tokio_agent_extension_api::ExtensionId, HostRequest>,
    states: BTreeMap<tokio_agent_extension_api::ExtensionId, HostRequest>,
}

impl Default for CompanionManager {
    fn default() -> Self {
        Self {
            executable: locate_companion().ok(),
            cache_directory: dirs::cache_dir().map(|path| path.join("tokio-agent/wasmtime")),
            running: None,
            restarted: false,
            circuit_open: false,
            loads: BTreeMap::new(),
            states: BTreeMap::new(),
        }
    }
}

impl CompanionManager {
    #[must_use]
    pub fn with_executable(path: PathBuf) -> Self {
        Self {
            executable: Some(path),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn is_available(&self) -> bool {
        self.executable.is_some() && !self.circuit_open
    }

    pub async fn request(&mut self, request: &HostRequest) -> Result<HostResponse, CompanionError> {
        if self.circuit_open {
            return Err(CompanionError::CircuitOpen);
        }
        let response = match self.request_once(request).await {
            Ok(response) => Ok(response),
            Err(error) if !self.restarted => {
                tracing::warn!(%error, "extension companion failed; restarting once");
                self.stop().await;
                self.restarted = true;
                let result = async {
                    self.ensure_started().await?;
                    self.replay_loads().await?;
                    self.request_once(request).await
                }
                .await;
                result.inspect_err(|_| self.circuit_open = true)
            }
            Err(error) => {
                self.circuit_open = true;
                Err(error)
            }
        }?;
        if let HostResponse::Error { message, .. } = &response {
            return Err(CompanionError::Host(message.clone()));
        }
        match (request, &response) {
            (HostRequest::Load { extension, .. }, HostResponse::Loaded { .. }) => {
                self.loads.insert(extension.clone(), request.clone());
            }
            (HostRequest::Disable { extension, .. }, HostResponse::Disabled { .. }) => {
                self.loads.remove(extension);
                self.states.remove(extension);
            }
            (HostRequest::Shutdown, _) => {
                self.loads.clear();
                self.states.clear();
            }
            _ => {}
        }
        Ok(response)
    }

    async fn request_once(
        &mut self,
        request: &HostRequest,
    ) -> Result<HostResponse, CompanionError> {
        self.ensure_started().await?;
        let running = self.running.as_mut().ok_or(CompanionError::Exited)?;
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            exchange(running, request),
        )
        .await
        .map_err(|_| CompanionError::Timeout)?
    }

    pub fn forget_extension(&mut self, extension: &tokio_agent_extension_api::ExtensionId) {
        self.loads.remove(extension);
        self.states.remove(extension);
    }

    pub fn remember_session_state(
        &mut self,
        extension: tokio_agent_extension_api::ExtensionId,
        generation: u64,
        state: Vec<u8>,
    ) {
        self.states.insert(
            extension.clone(),
            HostRequest::RestoreSessionState {
                extension,
                generation,
                state,
            },
        );
    }

    async fn replay_loads(&mut self) -> Result<(), CompanionError> {
        let loads: Vec<_> = self.loads.values().cloned().collect();
        for load in loads {
            let running = self.running.as_mut().ok_or(CompanionError::Exited)?;
            let response =
                tokio::time::timeout(std::time::Duration::from_secs(5), exchange(running, &load))
                    .await
                    .map_err(|_| CompanionError::Timeout)??;
            if !matches!(response, HostResponse::Loaded { .. }) {
                return Err(CompanionError::Protocol(
                    "companion failed to restore a loaded extension".into(),
                ));
            }
        }
        let states: Vec<_> = self.states.values().cloned().collect();
        for restore in states {
            let running = self.running.as_mut().ok_or(CompanionError::Exited)?;
            let response = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                exchange(running, &restore),
            )
            .await
            .map_err(|_| CompanionError::Timeout)??;
            if !matches!(response, HostResponse::SessionStateRestored { .. }) {
                return Err(CompanionError::Protocol(
                    "companion failed to restore extension session state".into(),
                ));
            }
        }
        Ok(())
    }

    async fn ensure_started(&mut self) -> Result<(), CompanionError> {
        if self.running.is_some() {
            return Ok(());
        }
        let executable = self.executable.as_ref().ok_or(CompanionError::NotFound)?;
        let mut command = Command::new(executable);
        command.env_clear();
        if let Some(cache_directory) = &self.cache_directory {
            command.arg("--cache-dir").arg(cache_directory);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CompanionError::Protocol("missing companion stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CompanionError::Protocol("missing companion stdout".into()))?;
        self.running = Some(RunningCompanion {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        });
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            exchange(
                self.running.as_mut().ok_or(CompanionError::Exited)?,
                &HostRequest::Handshake {
                    protocol_version: COMPANION_PROTOCOL_VERSION,
                    host_api: HOST_API_VERSION.to_owned(),
                },
            ),
        )
        .await
        .map_err(|_| CompanionError::Timeout)??;
        match response {
            HostResponse::Handshake {
                protocol_version: COMPANION_PROTOCOL_VERSION,
                host_api,
            } if host_api == HOST_API_VERSION => Ok(()),
            _ => Err(CompanionError::Protocol("incompatible handshake".into())),
        }
    }

    pub async fn stop(&mut self) {
        if let Some(mut running) = self.running.take() {
            let _ = running.child.kill().await;
            let _ = running.child.wait().await;
        }
    }
}

async fn exchange(
    running: &mut RunningCompanion,
    request: &HostRequest,
) -> Result<HostResponse, CompanionError> {
    let mut encoded =
        serde_json::to_vec(request).map_err(|error| CompanionError::Protocol(error.to_string()))?;
    encoded.push(b'\n');
    running.stdin.write_all(&encoded).await?;
    running.stdin.flush().await?;
    let mut line = String::new();
    if running.stdout.read_line(&mut line).await? == 0 {
        return Err(CompanionError::Exited);
    }
    serde_json::from_str(&line).map_err(|error| CompanionError::Protocol(error.to_string()))
}

pub fn locate_companion() -> Result<PathBuf, CompanionError> {
    let name = if cfg!(windows) {
        "tokio-agent-extension-host.exe"
    } else {
        "tokio-agent-extension-host"
    };
    if let Ok(current) = std::env::current_exe()
        && let Some(parent) = current.parent()
    {
        let sibling = parent.join(name);
        if is_executable_file(&sibling) {
            return Ok(sibling);
        }
    }
    let Some(path) = std::env::var_os("PATH") else {
        return Err(CompanionError::NotFound);
    };
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable_file(candidate))
        .ok_or(CompanionError::NotFound)
}

fn is_executable_file(path: &Path) -> bool {
    path.metadata().is_ok_and(|metadata| metadata.is_file())
}

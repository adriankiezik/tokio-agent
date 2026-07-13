#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_agent_extension_api::{
    COMPANION_PROTOCOL_VERSION, ExtensionAction, ExtensionId, HOST_API_VERSION, HostRequest,
    HostResponse, RuntimeLimits, Sequenced,
};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Cache, CacheConfig, Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "extension",
    });
}

const INITIALIZATION_FUEL: u64 = 100_000_000;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ComponentToolResult {
    content: String,
    is_error: bool,
    #[serde(default)]
    actions: Vec<ExtensionAction>,
}

struct HostState {
    limits: StoreLimits,
    table: ResourceTable,
    wasi: WasiCtx,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

struct Instance {
    generation: u64,
    limits: RuntimeLimits,
    store: Store<HostState>,
    bindings: bindings::Extension,
    failures: u32,
}

struct Host {
    engine: Engine,
    runtime_component: Component,
    instances: BTreeMap<ExtensionId, Instance>,
    next_sequence: u64,
}

fn read_script(path: &str, maximum_bytes: usize) -> anyhow::Result<String> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("reading JavaScript extension metadata at {path}"))?;
    if !metadata.is_file() {
        anyhow::bail!("JavaScript extension entrypoint is not a file");
    }
    if metadata.len() > maximum_bytes as u64 {
        anyhow::bail!("JavaScript extension source exceeded its limit");
    }
    std::fs::read_to_string(path)
        .with_context(|| format!("reading UTF-8 JavaScript extension at {path}"))
}

impl Host {
    fn new(cache_directory: Option<&Path>) -> anyhow::Result<Self> {
        let mut config = Config::new();
        config
            .wasm_component_model(true)
            .consume_fuel(true)
            .epoch_interruption(true);
        if let Some(cache_directory) = cache_directory {
            let mut cache_config = CacheConfig::new();
            cache_config.with_directory(cache_directory);
            config.cache(Some(Cache::new(cache_config)?));
        }
        let engine = Engine::new(&config)?;
        let runtime_component = Component::new(
            &engine,
            include_bytes!("../runtime/js-runtime.component.wasm"),
        )?;
        Ok(Self {
            engine,
            runtime_component,
            instances: BTreeMap::new(),
            next_sequence: 0,
        })
    }

    async fn handle(&mut self, request: HostRequest) -> HostResponse {
        match self.try_handle(request).await {
            Ok(response) => response,
            Err((extension, message, retryable)) => HostResponse::Error {
                extension,
                message,
                retryable,
            },
        }
    }

    async fn try_handle(
        &mut self,
        request: HostRequest,
    ) -> Result<HostResponse, (Option<ExtensionId>, String, bool)> {
        match request {
            HostRequest::Handshake {
                protocol_version,
                host_api,
            } => {
                if protocol_version != COMPANION_PROTOCOL_VERSION || host_api != HOST_API_VERSION {
                    return Err((None, "incompatible companion protocol".into(), false));
                }
                Ok(HostResponse::Handshake {
                    protocol_version: COMPANION_PROTOCOL_VERSION,
                    host_api: HOST_API_VERSION.to_owned(),
                })
            }
            HostRequest::ValidateScript { script_path } => {
                let limits = RuntimeLimits::default();
                let source = read_script(&script_path, limits.maximum_payload_bytes as usize)
                    .map_err(|error| (None, error.to_string(), false))?;
                let mut instance = self
                    .load(0, limits)
                    .await
                    .map_err(|error| (None, error.to_string(), false))?;
                instance
                    .store
                    .set_fuel(INITIALIZATION_FUEL)
                    .map_err(|error| (None, error.to_string(), false))?;
                let validation = instance
                    .bindings
                    .call_validate_source(&mut instance.store, &source)
                    .map_err(|error| (None, error.to_string(), false))?;
                if !validation.is_empty() {
                    return Err((
                        None,
                        format!("invalid extension JavaScript: {validation}"),
                        false,
                    ));
                }
                let settings = serde_json::json!({
                    "_tokio_source": source,
                    "_tokio_extension_id": "validation.extension",
                    "_host_generation": 0,
                })
                .to_string();
                instance
                    .store
                    .set_fuel(INITIALIZATION_FUEL)
                    .map_err(|error| (None, error.to_string(), false))?;
                instance
                    .bindings
                    .call_load_state(&mut instance.store, &[], &[], &settings, "{}")
                    .map_err(|error| {
                        (
                            None,
                            format!("invalid extension JavaScript: {error}"),
                            false,
                        )
                    })?;
                Ok(HostResponse::ScriptValid)
            }
            HostRequest::Load {
                extension,
                generation,
                script_path,
                capabilities: _,
                limits,
                user_state,
                settings,
                startup_settings,
            } => {
                let source = read_script(&script_path, limits.maximum_payload_bytes as usize)
                    .map_err(|error| (Some(extension.clone()), error.to_string(), false))?;
                let settings_size = settings
                    .to_string()
                    .len()
                    .saturating_add(startup_settings.to_string().len())
                    .saturating_add(source.len());
                if user_state.len().saturating_add(settings_size)
                    > limits.maximum_payload_bytes as usize
                {
                    return Err((
                        Some(extension),
                        "extension load state exceeded its limit".into(),
                        false,
                    ));
                }
                let mut instance = self
                    .load(generation, limits)
                    .await
                    .map_err(|error| (Some(extension.clone()), error.to_string(), false))?;
                instance
                    .store
                    .set_fuel(INITIALIZATION_FUEL)
                    .map_err(|error| (Some(extension.clone()), error.to_string(), false))?;
                let validation = instance
                    .bindings
                    .call_validate_source(&mut instance.store, &source)
                    .map_err(|error| (Some(extension.clone()), error.to_string(), false))?;
                if !validation.is_empty() {
                    return Err((
                        Some(extension.clone()),
                        format!("invalid extension JavaScript: {validation}"),
                        false,
                    ));
                }
                let mut settings = settings;
                let object = settings.as_object_mut().ok_or_else(|| {
                    (
                        Some(extension.clone()),
                        "extension settings must be an object".into(),
                        false,
                    )
                })?;
                object.insert("_tokio_source".into(), source.into());
                object.insert("_tokio_extension_id".into(), extension.as_str().into());
                let settings = settings.to_string();
                let startup_settings = startup_settings.to_string();
                instance
                    .store
                    .set_fuel(INITIALIZATION_FUEL)
                    .map_err(|error| (Some(extension.clone()), error.to_string(), false))?;
                instance
                    .bindings
                    .call_load_state(
                        &mut instance.store,
                        &user_state,
                        &[],
                        &settings,
                        &startup_settings,
                    )
                    .map_err(|error| (Some(extension.clone()), error.to_string(), true))?;
                self.instances.insert(extension.clone(), instance);
                Ok(HostResponse::Loaded {
                    extension,
                    generation,
                })
            }
            HostRequest::InvokeCommand {
                extension,
                generation,
                handler,
                arguments,
            } => {
                let actions = self
                    .invoke_command(&extension, generation, &handler, &arguments)
                    .await?;
                Ok(HostResponse::Actions(actions))
            }
            HostRequest::InvokeTool {
                extension,
                generation,
                handler,
                arguments_json,
            } => {
                let result = self
                    .invoke_tool(&extension, generation, &handler, &arguments_json)
                    .await?;
                let actions = result
                    .actions
                    .into_iter()
                    .map(|value| {
                        let sequence = self.next_sequence;
                        self.next_sequence = self.next_sequence.saturating_add(1);
                        Sequenced {
                            sequence,
                            extension: extension.clone(),
                            generation,
                            value,
                        }
                    })
                    .collect();
                Ok(HostResponse::ToolResult {
                    content: result.content,
                    is_error: result.is_error,
                    actions,
                })
            }
            HostRequest::AuthorizeTool {
                extension,
                generation,
                handler,
                invocation,
            } => {
                let json = serde_json::to_string(&invocation)
                    .map_err(|error| (Some(extension.clone()), error.to_string(), false))?;
                let output = self
                    .invoke_gate(&extension, generation, &handler, &json, None)
                    .await?;
                let result = serde_json::from_str(&output).map_err(|error| {
                    (
                        Some(extension.clone()),
                        format!("invalid tool gate result: {error}"),
                        false,
                    )
                })?;
                Ok(HostResponse::ToolGateResult(result))
            }
            HostRequest::InteractionResponse {
                extension,
                generation,
                handler,
                invocation_id,
                response,
            } => {
                let json = serde_json::to_string(&response)
                    .map_err(|error| (Some(extension.clone()), error.to_string(), false))?;
                let output = self
                    .invoke_gate(
                        &extension,
                        generation,
                        &handler,
                        &json,
                        Some(&invocation_id),
                    )
                    .await?;
                let result = serde_json::from_str(&output).map_err(|error| {
                    (
                        Some(extension.clone()),
                        format!("invalid tool gate result: {error}"),
                        false,
                    )
                })?;
                Ok(HostResponse::ToolGateResult(result))
            }
            HostRequest::RestoreSessionState {
                extension,
                generation,
                state,
            } => {
                let engine = self.engine.clone();
                let instance = self.instance(&extension, generation)?;
                if state.len() > instance.limits.maximum_payload_bytes as usize {
                    return Err((
                        Some(extension),
                        "session state exceeded its limit".into(),
                        false,
                    ));
                }
                let callback_running = deadline_guard(
                    engine,
                    Duration::from_millis(instance.limits.callback_deadline_ms),
                );
                instance
                    .store
                    .set_fuel(instance.limits.fuel_per_callback)
                    .map_err(|error| (Some(extension.clone()), error.to_string(), true))?;
                instance.store.set_epoch_deadline(1);
                let result = instance
                    .bindings
                    .call_restore_session_state(&mut instance.store, &state);
                callback_running.store(false, Ordering::Release);
                result.map_err(|error| (Some(extension.clone()), error.to_string(), true))?;
                Ok(HostResponse::SessionStateRestored {
                    extension,
                    generation,
                })
            }
            HostRequest::SessionEvent(event) => {
                let extension = event.extension.clone();
                let generation = event.generation;
                let json = serde_json::to_string(&event.value)
                    .map_err(|error| (Some(extension.clone()), error.to_string(), false))?;
                let actions = self.invoke_event(&extension, generation, &json).await?;
                Ok(HostResponse::Actions(actions))
            }
            HostRequest::Disable {
                extension,
                generation,
            } => {
                if self
                    .instances
                    .get(&extension)
                    .is_some_and(|instance| instance.generation == generation)
                {
                    self.instances.remove(&extension);
                }
                Ok(HostResponse::Disabled {
                    extension,
                    generation,
                })
            }
            HostRequest::Shutdown => {
                self.instances.clear();
                Ok(HostResponse::Actions(Vec::new()))
            }
        }
    }

    async fn load(&self, generation: u64, limits: RuntimeLimits) -> anyhow::Result<Instance> {
        let mut linker = Linker::new(&self.engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
        let store_limits = StoreLimitsBuilder::new()
            .memory_size(limits.memory_bytes as usize)
            .instances(32)
            .tables(8)
            .build();
        let mut store = Store::new(
            &self.engine,
            HostState {
                limits: store_limits,
                table: ResourceTable::new(),
                wasi: {
                    let mut wasi = WasiCtxBuilder::new();
                    wasi.allow_tcp(false).allow_udp(false);
                    wasi.build()
                },
            },
        );
        store.limiter(|state| &mut state.limits);
        store.set_fuel(limits.fuel_per_callback)?;
        store.set_epoch_deadline(1);
        let bindings =
            bindings::Extension::instantiate(&mut store, &self.runtime_component, &linker)
                .context("instantiating JavaScript extension runtime")?;
        Ok(Instance {
            generation,
            limits,
            store,
            bindings,
            failures: 0,
        })
    }

    async fn invoke_command(
        &mut self,
        extension: &ExtensionId,
        generation: u64,
        handler: &str,
        arguments: &str,
    ) -> Result<Vec<Sequenced<ExtensionAction>>, (Option<ExtensionId>, String, bool)> {
        let engine = self.engine.clone();
        let instance = self.instance(extension, generation)?;
        if handler.len().saturating_add(arguments.len())
            > instance.limits.maximum_payload_bytes as usize
        {
            return Err((
                Some(extension.clone()),
                "extension input exceeded its limit".into(),
                false,
            ));
        }
        let deadline = Duration::from_millis(instance.limits.callback_deadline_ms);
        let callback_running = deadline_guard(engine, deadline);
        instance
            .store
            .set_fuel(instance.limits.fuel_per_callback)
            .map_err(|error| (Some(extension.clone()), error.to_string(), true))?;
        instance.store.set_epoch_deadline(1);
        let result = instance
            .bindings
            .call_on_command(&mut instance.store, handler, arguments);
        callback_running.store(false, Ordering::Release);
        self.finish_callback(extension, generation, result)
    }

    async fn invoke_event(
        &mut self,
        extension: &ExtensionId,
        generation: u64,
        event: &str,
    ) -> Result<Vec<Sequenced<ExtensionAction>>, (Option<ExtensionId>, String, bool)> {
        let engine = self.engine.clone();
        let instance = self.instance(extension, generation)?;
        if event.len() > instance.limits.maximum_payload_bytes as usize {
            return Err((
                Some(extension.clone()),
                "extension input exceeded its limit".into(),
                false,
            ));
        }
        let deadline = Duration::from_millis(instance.limits.callback_deadline_ms);
        let callback_running = deadline_guard(engine, deadline);
        instance
            .store
            .set_fuel(instance.limits.fuel_per_callback)
            .map_err(|error| (Some(extension.clone()), error.to_string(), true))?;
        instance.store.set_epoch_deadline(1);
        let result = instance.bindings.call_on_event(&mut instance.store, event);
        callback_running.store(false, Ordering::Release);
        self.finish_callback(extension, generation, result)
    }

    async fn invoke_gate(
        &mut self,
        extension: &ExtensionId,
        generation: u64,
        handler: &str,
        json: &str,
        invocation_id: Option<&str>,
    ) -> Result<String, (Option<ExtensionId>, String, bool)> {
        let engine = self.engine.clone();
        let instance = self.instance(extension, generation)?;
        if handler.len().saturating_add(json.len()) > instance.limits.maximum_payload_bytes as usize
        {
            return Err((
                Some(extension.clone()),
                "tool gate input exceeded its limit".into(),
                false,
            ));
        }
        let callback_running = deadline_guard(
            engine,
            Duration::from_millis(instance.limits.callback_deadline_ms),
        );
        instance
            .store
            .set_fuel(instance.limits.fuel_per_callback)
            .map_err(|error| (Some(extension.clone()), error.to_string(), true))?;
        instance.store.set_epoch_deadline(1);
        let result = match invocation_id {
            Some(id) => instance.bindings.call_on_interaction_response(
                &mut instance.store,
                handler,
                id,
                json,
            ),
            None => instance
                .bindings
                .call_authorize_tool(&mut instance.store, handler, json),
        };
        callback_running.store(false, Ordering::Release);
        result.map_err(|error| (Some(extension.clone()), error.to_string(), true))
    }

    async fn invoke_tool(
        &mut self,
        extension: &ExtensionId,
        generation: u64,
        handler: &str,
        arguments: &str,
    ) -> Result<ComponentToolResult, (Option<ExtensionId>, String, bool)> {
        let engine = self.engine.clone();
        let instance = self.instance(extension, generation)?;
        if handler.len().saturating_add(arguments.len())
            > instance.limits.maximum_payload_bytes as usize
        {
            return Err((
                Some(extension.clone()),
                "extension input exceeded its limit".into(),
                false,
            ));
        }
        let deadline = Duration::from_millis(instance.limits.callback_deadline_ms);
        let callback_running = deadline_guard(engine, deadline);
        instance
            .store
            .set_fuel(instance.limits.fuel_per_callback)
            .map_err(|error| (Some(extension.clone()), error.to_string(), true))?;
        instance.store.set_epoch_deadline(1);
        let result = instance
            .bindings
            .call_on_tool(&mut instance.store, handler, arguments);
        callback_running.store(false, Ordering::Release);
        let maximum = self.instances[extension].limits.maximum_payload_bytes as usize;
        let json = match result {
            Ok(json) => {
                self.instances
                    .get_mut(extension)
                    .expect("instance exists")
                    .failures = 0;
                json
            }
            Err(error) => {
                self.instances
                    .get_mut(extension)
                    .expect("instance exists")
                    .failures += 1;
                return Err((Some(extension.clone()), error.to_string(), true));
            }
        };
        if json.len() > maximum {
            return Err((
                Some(extension.clone()),
                "extension output exceeded its limit".into(),
                false,
            ));
        }
        serde_json::from_str(&json).map_err(|error| {
            (
                Some(extension.clone()),
                format!("invalid extension tool result: {error}"),
                false,
            )
        })
    }

    fn instance(
        &mut self,
        extension: &ExtensionId,
        generation: u64,
    ) -> Result<&mut Instance, (Option<ExtensionId>, String, bool)> {
        let instance = self.instances.get_mut(extension).ok_or_else(|| {
            (
                Some(extension.clone()),
                "extension is not loaded".into(),
                false,
            )
        })?;
        if instance.generation != generation {
            return Err((
                Some(extension.clone()),
                "stale extension generation".into(),
                false,
            ));
        }
        if instance.failures >= instance.limits.circuit_breaker_failures {
            return Err((
                Some(extension.clone()),
                "extension circuit breaker is open".into(),
                false,
            ));
        }
        Ok(instance)
    }

    fn finish_callback(
        &mut self,
        extension: &ExtensionId,
        generation: u64,
        result: wasmtime::Result<String>,
    ) -> Result<Vec<Sequenced<ExtensionAction>>, (Option<ExtensionId>, String, bool)> {
        let maximum = self.instances[extension].limits.maximum_payload_bytes as usize;
        let json = match result {
            Ok(json) => {
                self.instances
                    .get_mut(extension)
                    .expect("instance exists")
                    .failures = 0;
                json
            }
            Err(error) => {
                self.instances
                    .get_mut(extension)
                    .expect("instance exists")
                    .failures += 1;
                return Err((Some(extension.clone()), error.to_string(), true));
            }
        };
        if json.len() > maximum {
            return Err((
                Some(extension.clone()),
                "extension output exceeded its limit".into(),
                false,
            ));
        }
        let actions: Vec<ExtensionAction> = serde_json::from_str(&json).map_err(|error| {
            (
                Some(extension.clone()),
                format!("invalid extension actions: {error}"),
                false,
            )
        })?;
        if actions.len() > self.instances[extension].limits.maximum_pending_actions as usize {
            return Err((
                Some(extension.clone()),
                "too many extension actions".into(),
                false,
            ));
        }
        Ok(actions
            .into_iter()
            .map(|value| {
                let sequence = self.next_sequence;
                self.next_sequence = self.next_sequence.saturating_add(1);
                Sequenced {
                    sequence,
                    extension: extension.clone(),
                    generation,
                    value,
                }
            })
            .collect())
    }
}

fn deadline_guard(engine: Engine, deadline: Duration) -> Arc<AtomicBool> {
    let running = Arc::new(AtomicBool::new(true));
    let check = Arc::clone(&running);
    std::thread::spawn(move || {
        std::thread::sleep(deadline);
        if check.swap(false, Ordering::AcqRel) {
            engine.increment_epoch();
        }
    });
    running
}

fn cache_directory_from_args() -> anyhow::Result<Option<PathBuf>> {
    let mut args = std::env::args_os().skip(1);
    let Some(flag) = args.next() else {
        return Ok(None);
    };
    if flag != "--cache-dir" {
        anyhow::bail!("unknown argument `{}`", flag.to_string_lossy());
    }
    let directory = args.next().context("--cache-dir requires a path")?;
    if let Some(extra) = args.next() {
        anyhow::bail!("unexpected argument `{}`", extra.to_string_lossy());
    }
    Ok(Some(PathBuf::from(directory)))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cache_directory = cache_directory_from_args()?;
    let mut host = Host::new(cache_directory.as_deref())?;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let response = match serde_json::from_str::<HostRequest>(&line) {
            Ok(request) => {
                let shutdown = matches!(request, HostRequest::Shutdown);
                let response = host.handle(request).await;
                if shutdown {
                    let mut encoded = serde_json::to_vec(&response)?;
                    encoded.push(b'\n');
                    stdout.write_all(&encoded).await?;
                    stdout.flush().await?;
                    break;
                }
                response
            }
            Err(error) => HostResponse::Error {
                extension: None,
                message: format!("invalid host request: {error}"),
                retryable: false,
            },
        };
        let mut encoded = serde_json::to_vec(&response)?;
        encoded.push(b'\n');
        stdout.write_all(&encoded).await?;
        stdout.flush().await?;
    }
    Ok(())
}

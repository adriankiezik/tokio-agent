use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};

use futures::StreamExt;
use tokio_agent_core::{DynamicToolCatalog, SessionHookEffect, Tool, ToolCtx, ToolDef, ToolResult};
use tokio_agent_extension_api::{CommandId, ExtensionId, SessionEvent, ToolId};
use tokio_agent_plugin::{
    ExtensionManifest, RuntimeToolResult, SessionSupervisor, SupervisorEffect, SupervisorPolicy,
};

pub struct ProgrammablePackage {
    pub root: PathBuf,
    pub manifest: ExtensionManifest,
    pub settings: serde_json::Value,
    pub startup_settings: serde_json::Value,
    pub fingerprint: u128,
}

#[derive(Clone)]
pub struct ExtensionRuntime {
    tx: mpsc::Sender<Request>,
    generations: Arc<RwLock<BTreeMap<ExtensionId, u64>>>,
    initializing: Arc<AtomicBool>,
    startup_replies: Arc<Mutex<Vec<mpsc::Receiver<Vec<SupervisorEffect>>>>>,
    tools: Arc<RwLock<BTreeMap<(ExtensionId, String), String>>>,
    versions: Arc<RwLock<BTreeMap<ExtensionId, String>>>,
    dynamic: DynamicToolCatalog,
    registered: Arc<Mutex<BTreeMap<ToolId, String>>>,
    pending: Arc<Mutex<Vec<SessionHookEffect>>>,
    active_interactions:
        Arc<Mutex<BTreeMap<tokio_agent_extension_api::InteractionId, ExtensionId>>>,
    gate_target: Arc<RwLock<Option<GateTarget>>>,
    gate_slot: Arc<Mutex<Option<tokio_agent_core::ToolGateSlot>>>,
}

#[derive(Clone)]
struct GateTarget {
    extension: ExtensionId,
    generation: u64,
    authorize_handler: String,
    response_handler: String,
}

#[derive(Clone)]
struct CommandTarget {
    extension: ExtensionId,
    generation: u64,
    handler: String,
}

enum Request {
    Command {
        id: CommandId,
        arguments: String,
        reply: mpsc::Sender<anyhow::Result<Vec<SupervisorEffect>>>,
    },
    Event {
        event: SessionEvent,
        reply: mpsc::Sender<Vec<SupervisorEffect>>,
    },
    Poll {
        reply: mpsc::Sender<Vec<SupervisorEffect>>,
    },
    Load {
        packages: Vec<ProgrammablePackage>,
        reply: mpsc::Sender<anyhow::Result<()>>,
    },
    Tool {
        extension: ExtensionId,
        generation: u64,
        handler: String,
        arguments: String,
        reply: mpsc::Sender<anyhow::Result<RuntimeToolResult>>,
    },
    GateAuthorize {
        target: GateTarget,
        invocation: tokio_agent_extension_api::ToolGateInvocation,
        reply: mpsc::Sender<
            anyhow::Result<(
                tokio_agent_extension_api::ToolGateResponse,
                Vec<SupervisorEffect>,
            )>,
        >,
    },
    GateRespond {
        target: GateTarget,
        invocation_id: String,
        response: tokio_agent_extension_api::InteractionResponse,
        reply: mpsc::Sender<
            anyhow::Result<(
                tokio_agent_extension_api::ToolGateResponse,
                Vec<SupervisorEffect>,
            )>,
        >,
    },
    Shutdown {
        reply: mpsc::Sender<()>,
    },
}

const MAX_NETWORK_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_NETWORK_REDIRECTS: usize = 3;
const MAX_NETWORK_REQUESTS_PER_SETTLEMENT: usize = 32;
const MAX_CONCURRENT_NETWORK_REQUESTS: usize = 8;

async fn settle_network_effects(
    supervisor: &mut SessionSupervisor,
    effects: Vec<SupervisorEffect>,
) -> Vec<SupervisorEffect> {
    let mut pending: std::collections::VecDeque<_> = effects.into();
    let mut settled = Vec::new();
    let mut request_count = 0;
    loop {
        let mut requests = Vec::new();
        while let Some(effect) = pending.pop_front() {
            match effect {
                SupervisorEffect::NetworkRequest {
                    owner,
                    generation,
                    request,
                } if request_count < MAX_NETWORK_REQUESTS_PER_SETTLEMENT => {
                    request_count += 1;
                    requests.push((owner, generation, request));
                }
                SupervisorEffect::NetworkRequest { .. } => {
                    settled.push(SupervisorEffect::Notice {
                        level: tokio_agent_extension_api::NoticeLevel::Error,
                        text: "Extension network action chain exceeded its limit".into(),
                    });
                }
                effect => settled.push(effect),
            }
        }
        if requests.is_empty() {
            break;
        }
        let responses = futures::stream::iter(requests)
            .map(|(owner, generation, request)| async move {
                (owner, generation, fetch_public(request).await)
            })
            .buffer_unordered(MAX_CONCURRENT_NETWORK_REQUESTS)
            .collect::<Vec<_>>()
            .await;
        for (owner, generation, response) in responses {
            pending.extend(
                supervisor
                    .deliver_network_response(owner, generation, response)
                    .await
                    .into_iter()
                    .map(|result| {
                        result.unwrap_or_else(|error| SupervisorEffect::Notice {
                            level: tokio_agent_extension_api::NoticeLevel::Error,
                            text: format!("Extension network response failed: {error}"),
                        })
                    }),
            );
        }
    }
    settled
}

async fn fetch_public(
    request: tokio_agent_extension_api::NetworkRequest,
) -> tokio_agent_extension_api::NetworkResponse {
    let requested_url = request.url.clone();
    match try_fetch_public(&request.url).await {
        Ok((url, status, body)) => tokio_agent_extension_api::NetworkResponse {
            id: request.id,
            url,
            status: Some(status),
            body: Some(body),
            error: None,
        },
        Err(error) => tokio_agent_extension_api::NetworkResponse {
            id: request.id,
            url: requested_url,
            status: None,
            body: None,
            error: Some(error.to_string()),
        },
    }
}

async fn try_fetch_public(url: &str) -> anyhow::Result<(String, u16, String)> {
    let mut url = reqwest::Url::parse(url)?;
    for redirect in 0..=MAX_NETWORK_REDIRECTS {
        let (client, checked) = public_client(&url).await?;
        let mut response = client
            .get(checked.clone())
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/plain;q=0.9, */*;q=0.1",
            )
            .send()
            .await?;
        if response.status().is_redirection() {
            if redirect == MAX_NETWORK_REDIRECTS {
                anyhow::bail!("network request exceeded its redirect limit");
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .ok_or_else(|| anyhow::anyhow!("network redirect omitted Location"))?
                .to_str()?;
            url = checked.join(location)?;
            continue;
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_NETWORK_RESPONSE_BYTES as u64)
        {
            anyhow::bail!("network response exceeded 256 KiB");
        }
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len().saturating_add(chunk.len()) > MAX_NETWORK_RESPONSE_BYTES {
                anyhow::bail!("network response exceeded 256 KiB");
            }
            bytes.extend_from_slice(&chunk);
        }
        let body = String::from_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("network response was not UTF-8 text"))?;
        return Ok((checked.to_string(), status, body));
    }
    unreachable!("redirect loop returns or fails")
}

async fn public_client(url: &reqwest::Url) -> anyhow::Result<(reqwest::Client, reqwest::Url)> {
    if url.scheme() != "https" {
        anyhow::bail!("extension network requests require HTTPS");
    }
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("extension network URLs cannot contain credentials");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("extension network URL has no host"))?;
    let port = url.port_or_known_default().unwrap_or(443);
    let addresses: Vec<_> = tokio::net::lookup_host((host, port)).await?.collect();
    if addresses.is_empty() || addresses.iter().any(|address| !is_public_ip(address.ip())) {
        anyhow::bail!("extension network destination is not a public internet address");
    }
    let pinned = SocketAddr::new(addresses[0].ip(), port);
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .resolve(host, pinned)
        .user_agent("tokio-agent-extension/1")
        .build()?;
    Ok((client, url.clone()))
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map_or_else(|| is_public_ipv6(ip), is_public_ipv4),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || a == 0
        || a >= 240
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0)
        || (a == 198 && matches!(b, 18 | 19)))
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (ip.segments()[0] & 0xfe00) == 0xfc00
        || (ip.segments()[0] & 0xffc0) == 0xfe80
        || (ip.segments()[0] == 0x2001 && ip.segments()[1] == 0x0db8))
}

impl ExtensionRuntime {
    pub fn start(
        packages: Vec<ProgrammablePackage>,
        dynamic: DynamicToolCatalog,
    ) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let generations = Arc::new(RwLock::new(BTreeMap::new()));
        let worker_generations = Arc::clone(&generations);
        let initializing = Arc::new(AtomicBool::new(true));
        let worker_initializing = Arc::clone(&initializing);
        let mut tool_handlers = BTreeMap::new();
        let mut versions = BTreeMap::new();
        if packages
            .iter()
            .filter(|package| package.manifest.tool_gate.is_some())
            .count()
            > 1
        {
            anyhow::bail!("only one tool gate extension may be active");
        }
        let initial_gate = packages.iter().find_map(|package| {
            package.manifest.tool_gate.as_ref().map(|gate| GateTarget {
                extension: ExtensionId::new(&package.manifest.id),
                generation: 1,
                authorize_handler: gate.handler.clone(),
                response_handler: gate.interaction_handler.clone(),
            })
        });
        let gate_target = Arc::new(RwLock::new(initial_gate));
        let worker_gate_target = Arc::clone(&gate_target);
        for package in &packages {
            let extension = ExtensionId::new(&package.manifest.id);
            versions.insert(extension.clone(), package.manifest.version.clone());
            for tool in &package.manifest.tools {
                tool_handlers.insert((extension.clone(), tool.name.clone()), tool.handler.clone());
            }
        }
        std::thread::Builder::new()
            .name("extension-supervisor".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                let Ok(runtime) = runtime else {
                    worker_initializing.store(false, Ordering::Release);
                    return;
                };
                runtime.block_on(async move {
                    let mut supervisor = SessionSupervisor::new(SupervisorPolicy::default());
                    let mut commands = BTreeMap::new();
                    let mut startup_error = None;
                    let mut loaded_generations = BTreeMap::new();
                    let mut loaded_fingerprints = BTreeMap::new();
                    for package in packages {
                        match supervisor
                            .enable_programmable_with_settings(
                                &package.manifest,
                                &package.root,
                                Default::default(),
                                package.settings.clone(),
                                package.startup_settings.clone(),
                            )
                            .await
                        {
                            Ok(generation) => {
                                let extension = ExtensionId::new(&package.manifest.id);
                                loaded_generations.insert(extension.clone(), generation);
                                loaded_fingerprints.insert(extension.clone(), package.fingerprint);
                                if let Some(gate) = &package.manifest.tool_gate {
                                    *worker_gate_target.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(GateTarget {
                                        extension: extension.clone(), generation,
                                        authorize_handler: gate.handler.clone(),
                                        response_handler: gate.interaction_handler.clone(),
                                    });
                                }
                                for command in package
                                    .manifest
                                    .commands
                                    .iter()
                                    .filter(|command| command.handler.is_some())
                                {
                                    commands.insert(
                                        CommandId::new(format!(
                                            "{}:{}",
                                            package.manifest.id, command.name
                                        )),
                                        CommandTarget {
                                            extension: extension.clone(),
                                            generation,
                                            handler: command.handler.clone().expect("filtered"),
                                        },
                                    );
                                }
                            }
                            Err(error) => {
                                startup_error = Some(error.to_string());
                                break;
                            }
                        }
                    }
                    *worker_generations
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        loaded_generations.clone();
                    worker_initializing.store(false, Ordering::Release);
                    let mut startup_error_reported = false;
                    let mut shutdown_reply = None;
                    while let Ok(request) = rx.recv() {
                        match request {
                            Request::Command {
                                id,
                                arguments,
                                reply,
                            } => {
                                let mut result = if let Some(error) = &startup_error {
                                    Err(anyhow::anyhow!(error.clone()))
                                } else if let Some(target) = commands.get(&id).cloned() {
                                    supervisor
                                        .invoke_programmable_command(
                                            target.extension,
                                            target.generation,
                                            target.handler,
                                            arguments,
                                        )
                                        .await
                                        .map_err(anyhow::Error::new)
                                } else {
                                    Err(anyhow::anyhow!("unknown programmable command `{id}`"))
                                };
                                if let Ok(effects) = &mut result {
                                    *effects = settle_network_effects(&mut supervisor, std::mem::take(effects)).await;
                                    for effect in effects {
                                        if let SupervisorEffect::SubmitPrompt {
                                            automatic: true,
                                            owner,
                                            ..
                                        } = effect
                                        {
                                            supervisor.automatic_admitted(owner);
                                        }
                                    }
                                }
                                let _ = reply.send(result);
                            }
                            Request::Event { event, reply } => {
                                let effects: Vec<_> = if let Some(error) = &startup_error
                                    && !startup_error_reported
                                {
                                    startup_error_reported = true;
                                    vec![SupervisorEffect::Notice {
                                        level: tokio_agent_extension_api::NoticeLevel::Error,
                                        text: format!("Extension failed to start: {error}"),
                                    }]
                                } else {
                                    supervisor
                                        .broadcast(event)
                                        .await
                                        .into_iter()
                                        .map(|result| result.unwrap_or_else(|error| SupervisorEffect::Notice {
                                        level: tokio_agent_extension_api::NoticeLevel::Error,
                                        text: format!("Extension stopped: {error}"),
                                        }))
                                        .collect()
                                };
                                let effects = settle_network_effects(&mut supervisor, effects).await;
                                for effect in &effects {
                                    if let SupervisorEffect::SubmitPrompt {
                                        automatic: true,
                                        owner,
                                        ..
                                    } = effect
                                    {
                                        supervisor.automatic_admitted(owner);
                                    }
                                }
                                let _ = reply.send(effects);
                            }
                            Request::Poll { reply } => {
                                let events = supervisor.fire_due(std::time::Instant::now());
                                let mut effects = Vec::new();
                                for event in events {
                                    effects.extend(
                                        supervisor
                                            .deliver(event)
                                            .await
                                            .into_iter()
                                            .map(|result| result.unwrap_or_else(|error| SupervisorEffect::Notice {
                                                level: tokio_agent_extension_api::NoticeLevel::Error,
                                                text: format!("Extension stopped: {error}"),
                                            })),
                                    );
                                }
                                effects = settle_network_effects(&mut supervisor, effects).await;
                                for effect in &effects {
                                    if let SupervisorEffect::SubmitPrompt {
                                        automatic: true,
                                        owner,
                                        ..
                                    } = effect
                                    {
                                        supervisor.automatic_admitted(owner);
                                    }
                                }
                                let _ = reply.send(effects);
                            }
                            Request::Load { packages, reply } => {
                                let desired: std::collections::BTreeSet<_> = packages.iter().map(|package| ExtensionId::new(&package.manifest.id)).collect();
                                let changed: std::collections::BTreeSet<_> = packages.iter().filter_map(|package| {
                                    let id = ExtensionId::new(&package.manifest.id);
                                    loaded_fingerprints.get(&id).is_some_and(|fingerprint| *fingerprint != package.fingerprint).then_some(id)
                                }).collect();
                                let removed: Vec<_> = loaded_generations.keys().filter(|id| !desired.contains(*id) || changed.contains(*id)).cloned().collect();
                                for extension in removed {
                                    if supervisor.disable(&extension).await.is_ok() {
                                        loaded_generations.remove(&extension);
                                        loaded_fingerprints.remove(&extension);
                                        worker_generations.write().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&extension);
                                        commands.retain(|_, target| target.extension != extension);
                                        if worker_gate_target.read().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref().is_some_and(|target| target.extension == extension) {
                                            *worker_gate_target.write().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                                        }
                                    }
                                }
                                let mut result = Ok(());
                                for package in packages {
                                    let extension = ExtensionId::new(&package.manifest.id);
                                    if loaded_generations.contains_key(&extension) {
                                        continue;
                                    }
                                    match supervisor
                                        .enable_programmable_with_settings(
                                            &package.manifest,
                                            &package.root,
                                            Default::default(),
                                            package.settings.clone(),
                                            package.startup_settings.clone(),
                                        )
                                        .await
                                    {
                                        Ok(generation) => {
                                            loaded_generations.insert(extension.clone(), generation);
                                            loaded_fingerprints.insert(extension.clone(), package.fingerprint);
                                            worker_generations
                                                .write()
                                                .unwrap_or_else(
                                                    std::sync::PoisonError::into_inner,
                                                )
                                                .insert(extension.clone(), generation);
                                            if let Some(gate) = &package.manifest.tool_gate {
                                    *worker_gate_target.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(GateTarget {
                                        extension: extension.clone(), generation,
                                        authorize_handler: gate.handler.clone(),
                                        response_handler: gate.interaction_handler.clone(),
                                    });
                                }
                                for command in package
                                                .manifest
                                                .commands
                                                .iter()
                                                .filter(|command| command.handler.is_some())
                                            {
                                                commands.insert(
                                                    CommandId::new(format!(
                                                        "{}:{}",
                                                        package.manifest.id, command.name
                                                    )),
                                                    CommandTarget {
                                                        extension: extension.clone(),
                                                        generation,
                                                        handler: command
                                                            .handler
                                                            .clone()
                                                            .expect("filtered"),
                                                    },
                                                );
                                            }
                                        }
                                        Err(error) => {
                                            result = Err(anyhow::Error::new(error));
                                            break;
                                        }
                                    }
                                }
                                let _ = reply.send(result);
                            }
                            Request::GateAuthorize { target, invocation, reply } => {
                                let owner = target.extension.clone();
                                let generation = target.generation;
                                let mut result = supervisor.authorize_tool(
                                    target.extension, generation, target.authorize_handler, invocation,
                                ).await.and_then(|response| supervisor.apply_gate_response(owner, generation, response)).map_err(anyhow::Error::new);
                                if let Ok((_, effects)) = &mut result {
                                    *effects = settle_network_effects(&mut supervisor, std::mem::take(effects)).await;
                                }
                                let _ = reply.send(result);
                            }
                            Request::GateRespond { target, invocation_id, response, reply } => {
                                let owner = target.extension.clone();
                                let generation = target.generation;
                                let mut result = supervisor.respond_to_interaction(
                                    target.extension, generation, target.response_handler, invocation_id, response,
                                ).await.and_then(|response| supervisor.apply_gate_response(owner, generation, response)).map_err(anyhow::Error::new);
                                if let Ok((_, effects)) = &mut result {
                                    *effects = settle_network_effects(&mut supervisor, std::mem::take(effects)).await;
                                }
                                let _ = reply.send(result);
                            }
                            Request::Tool {                                extension,
                                generation,
                                handler,
                                arguments,
                                reply,
                            } => {
                                let mut result = if let Some(error) = &startup_error {
                                    Err(anyhow::anyhow!(error.clone()))
                                } else {
                                    supervisor
                                        .invoke_programmable_tool(
                                            extension, generation, handler, arguments,
                                        )
                                        .await
                                        .map_err(anyhow::Error::new)
                                };
                                if let Ok(result) = &mut result {
                                    result.effects = settle_network_effects(
                                        &mut supervisor,
                                        std::mem::take(&mut result.effects),
                                    )
                                    .await;
                                    for effect in &result.effects {
                                        if let SupervisorEffect::SubmitPrompt {
                                            automatic: true,
                                            owner,
                                            ..
                                        } = effect
                                        {
                                            supervisor.automatic_admitted(owner);
                                        }
                                    }
                                }
                                let _ = reply.send(result);
                            }
                            Request::Shutdown { reply } => {
                                shutdown_reply = Some(reply);
                                break;
                            }
                        }
                    }
                    supervisor.shutdown().await;
                    if let Some(reply) = shutdown_reply {
                        let _ = reply.send(());
                    }
                });
            })?;
        Ok(Self {
            tx,
            generations,
            initializing,
            startup_replies: Arc::new(Mutex::new(Vec::new())),
            tools: Arc::new(RwLock::new(tool_handlers)),
            versions: Arc::new(RwLock::new(versions)),
            dynamic,
            registered: Arc::new(Mutex::new(BTreeMap::new())),
            pending: Arc::new(Mutex::new(Vec::new())),
            active_interactions: Arc::new(Mutex::new(BTreeMap::new())),
            gate_target,
            gate_slot: Arc::new(Mutex::new(None)),
        })
    }

    pub fn load(&self, packages: Vec<ProgrammablePackage>) -> anyhow::Result<()> {
        let previous_generations = self
            .generations
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let previous_gate = self
            .gate_target
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let desired: std::collections::BTreeSet<_> = packages
            .iter()
            .map(|package| ExtensionId::new(&package.manifest.id))
            .collect();
        let removed: Vec<_> = self
            .generations
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .filter(|id| !desired.contains(*id))
            .cloned()
            .collect();
        if packages
            .iter()
            .filter(|package| package.manifest.tool_gate.is_some())
            .count()
            > 1
        {
            anyhow::bail!("only one tool gate extension may be active");
        }
        {
            let mut tools = self
                .tools
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for package in &packages {
                let extension = ExtensionId::new(&package.manifest.id);
                self.versions
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(extension.clone(), package.manifest.version.clone());
                for tool in &package.manifest.tools {
                    tools.insert((extension.clone(), tool.name.clone()), tool.handler.clone());
                }
            }
        }
        let (reply, receive) = mpsc::channel();
        self.tx.send(Request::Load { packages, reply })?;
        if let Err(error) = receive.recv()? {
            if previous_gate.is_some()
                && let Some(slot) = self
                    .gate_slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            {
                slot.fail(error.to_string());
            }
            return Err(error);
        }
        let current_generations = self
            .generations
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut lifecycle_changed: std::collections::BTreeSet<_> = removed.into_iter().collect();
        lifecycle_changed.extend(
            previous_generations
                .iter()
                .filter_map(|(owner, generation)| {
                    (current_generations
                        .get(owner)
                        .is_some_and(|current| current != generation))
                    .then_some(owner.clone())
                }),
        );
        for owner in lifecycle_changed {
            self.dynamic.disable(owner.as_str());
            let cancelled: Vec<_> = {
                let mut active = self
                    .active_interactions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let ids = active
                    .iter()
                    .filter_map(|(id, interaction_owner)| {
                        (interaction_owner == &owner).then_some(id.clone())
                    })
                    .collect::<Vec<_>>();
                for id in &ids {
                    active.remove(id);
                }
                ids
            };
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend(
                    cancelled
                        .into_iter()
                        .map(SessionHookEffect::InteractionCancelled),
                );
        }
        if let Some(slot) = self
            .gate_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            let current_gate = self
                .gate_target
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            match current_gate {
                Some(current)
                    if previous_gate
                        .as_ref()
                        .is_none_or(|previous| previous.generation != current.generation) =>
                {
                    slot.attach(Arc::new(ExtensionGate {
                        runtime: self.clone(),
                        slot: slot.clone(),
                    }));
                }
                Some(_) if matches!(slot.snapshot(), tokio_agent_core::ToolGateState::Absent) => {
                    slot.attach(Arc::new(ExtensionGate {
                        runtime: self.clone(),
                        slot: slot.clone(),
                    }));
                }
                Some(_) => {}
                None => slot.detach(),
            }
        }
        Ok(())
    }

    pub fn shutdown(&self) {
        let owners: Vec<_> = self
            .generations
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        for owner in owners {
            self.dynamic.disable(owner.as_str());
        }
        if let Some(slot) = self
            .gate_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            slot.detach();
        }
        let (reply, receive) = mpsc::channel();
        if self.tx.send(Request::Shutdown { reply }).is_ok() {
            let _ = receive.recv_timeout(std::time::Duration::from_secs(2));
        }
    }

    pub fn tool_gate(
        &self,
        slot: tokio_agent_core::ToolGateSlot,
    ) -> Option<Arc<dyn tokio_agent_core::ToolGate>> {
        *self
            .gate_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(slot.clone());
        self.gate_target
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()?;
        Some(Arc::new(ExtensionGate {
            runtime: self.clone(),
            slot,
        }))
    }

    pub fn respond_to_interaction(
        &self,
        response: tokio_agent_extension_api::InteractionResponse,
    ) -> anyhow::Result<()> {
        self.active_interactions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&response.id);
        let target = self
            .gate_target
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .filter(|target| {
                target.extension == response.owner && target.generation == response.generation
            })
            .ok_or_else(|| anyhow::anyhow!("stale or wrong-owner interaction response"))?;
        let (reply, receive) = mpsc::channel();
        self.tx.send(Request::GateRespond {
            invocation_id: response.id.to_string(),
            target,
            response,
            reply,
        })?;
        let (_, effects) = receive.recv()??;
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(self.process_effects(effects));
        Ok(())
    }

    pub fn route(
        &self,
        id: &CommandId,
        arguments: String,
    ) -> anyhow::Result<Option<(String, bool)>> {
        let (reply, receive) = mpsc::channel();
        self.tx.send(Request::Command {
            id: id.clone(),
            arguments,
            reply,
        })?;
        let effects = receive.recv()??;
        let mut prompt = None;
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for effect in self.process_effects(effects) {
            match effect {
                SessionHookEffect::SubmitPrompt {
                    text, automatic, ..
                } if prompt.is_none() => prompt = Some((text, automatic)),
                other => pending.push(other),
            }
        }
        Ok(prompt)
    }

    pub fn event(&self, event: SessionEvent) -> Vec<SessionHookEffect> {
        let is_session_start = matches!(event, SessionEvent::SessionStarted);
        let (reply, receive) = mpsc::channel();
        if self.tx.send(Request::Event { event, reply }).is_err() {
            return Vec::new();
        }
        if self.initializing.load(Ordering::Acquire) {
            self.startup_replies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(receive);
            return if is_session_start {
                vec![SessionHookEffect::StatusSegments(vec![
                    initializing_status_segment(),
                ])]
            } else {
                Vec::new()
            };
        }
        let mut effects = std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        effects.extend(self.process_effects(receive.recv().unwrap_or_default()));
        effects
    }

    pub fn poll(&self) -> Vec<SessionHookEffect> {
        let mut effects = std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let mut startup_replies = self
            .startup_replies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let had_startup_replies = !startup_replies.is_empty();
        let mut waiting = Vec::new();
        for receive in startup_replies.drain(..) {
            match receive.try_recv() {
                Ok(startup_effects) => effects.extend(self.process_effects(startup_effects)),
                Err(mpsc::TryRecvError::Empty) => waiting.push(receive),
                Err(mpsc::TryRecvError::Disconnected) => {}
            }
        }
        *startup_replies = waiting;
        let waiting_for_startup_events = !startup_replies.is_empty();
        drop(startup_replies);

        if self.initializing.load(Ordering::Acquire) || waiting_for_startup_events {
            return effects;
        }
        if had_startup_replies
            && !effects
                .iter()
                .any(|effect| matches!(effect, SessionHookEffect::StatusSegments(_)))
        {
            effects.push(SessionHookEffect::StatusSegments(Vec::new()));
        }

        let (reply, receive) = mpsc::channel();
        if self.tx.send(Request::Poll { reply }).is_err() {
            return effects;
        }
        effects.extend(self.process_effects(receive.recv().unwrap_or_default()));
        effects
    }

    fn process_effects(&self, effects: Vec<SupervisorEffect>) -> Vec<SessionHookEffect> {
        let mut output = Vec::new();
        for effect in effects {
            match effect {
                SupervisorEffect::SubmitPrompt {
                    text,
                    automatic,
                    owner,
                } => output.push(SessionHookEffect::SubmitPrompt {
                    text,
                    automatic,
                    source: Some(owner),
                }),
                SupervisorEffect::Notice { text, .. } => {
                    output.push(SessionHookEffect::Notice(text))
                }
                SupervisorEffect::Status(segments) => {
                    output.push(SessionHookEffect::StatusSegments(segments))
                }
                SupervisorEffect::RegisterTool(descriptor) => {
                    let key = (descriptor.owner.clone(), descriptor.name.clone());
                    if let Some(handler) = self
                        .tools
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(&key)
                        .cloned()
                    {
                        let generation = self
                            .generations
                            .read()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .get(&descriptor.owner)
                            .copied()
                            .unwrap_or(1);
                        let name = descriptor.name.clone();
                        let tool = Arc::new(ExtensionTool {
                            descriptor: descriptor.clone(),
                            handler,
                            generation,
                            version: self
                                .versions
                                .read()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .get(&descriptor.owner)
                                .cloned()
                                .unwrap_or_else(|| "unknown".into()),
                            runtime: self.clone(),
                        });
                        if self
                            .dynamic
                            .register(descriptor.owner.to_string(), tool)
                            .is_ok()
                        {
                            self.registered
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .insert(descriptor.id, name);
                        }
                    }
                }
                SupervisorEffect::UnregisterTool { owner, id } => {
                    if let Some(name) = self
                        .registered
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&id)
                    {
                        self.dynamic.unregister(owner.as_str(), &name);
                    }
                }
                SupervisorEffect::InteractionRequested(request) => {
                    self.active_interactions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(request.id.clone(), request.owner.clone());
                    output.push(SessionHookEffect::InteractionRequested(request));
                }
                SupervisorEffect::UserStateStored { owner, bytes } => {
                    if let Err(error) = tokio_agent_plugin::store_user_state(&owner, &bytes) {
                        output.push(SessionHookEffect::Notice(format!(
                            "failed to persist extension state: {error}"
                        )));
                    }
                }
                SupervisorEffect::SessionStateStored { .. }
                | SupervisorEffect::AutonomyReleased { .. } => {}
                SupervisorEffect::NetworkRequest { .. } => output.push(SessionHookEffect::Notice(
                    "extension network request was not processed".into(),
                )),
            }
        }
        output
    }
}

struct ExtensionGate {
    runtime: ExtensionRuntime,
    slot: tokio_agent_core::ToolGateSlot,
}

impl tokio_agent_core::ToolGate for ExtensionGate {
    fn authorize<'a>(
        &'a self,
        invocation: tokio_agent_core::ToolInvocation,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> tokio_agent_core::provider::BoxFuture<'a, tokio_agent_core::ToolGateResult> {
        Box::pin(async move {
            let Some(target) = self
                .runtime
                .gate_target
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
            else {
                self.slot.fail("installed tool gate is unavailable");
                return tokio_agent_core::ToolGateResult::Deny {
                    reason: "installed tool gate is unavailable".into(),
                };
            };
            let request = tokio_agent_extension_api::ToolGateInvocation {
                gate_owner: target.extension.clone(),
                gate_generation: target.generation,
                invocation_id: invocation.invocation_id,
                tool_name: invocation.tool_name,
                owner: invocation.owner,
                arguments: invocation.arguments,
                effect: invocation.effect,
                cwd: invocation.cwd.to_string_lossy().into_owned(),
                summary_hint: invocation.summary_hint,
                frontend: invocation.frontend,
            };
            let runtime = self.runtime.clone();
            let result = tokio::task::spawn_blocking(move || {
                let (reply, receive) = mpsc::channel();
                runtime.tx.send(Request::GateAuthorize {
                    target,
                    invocation: request,
                    reply,
                })?;
                let (response, effects) = receive.recv()??;
                runtime
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend(runtime.process_effects(effects));
                Ok::<_, anyhow::Error>(response)
            })
            .await;
            match result {
                Ok(Ok(response)) => gate_result(response),
                Ok(Err(error)) => {
                    self.slot.fail(error.to_string());
                    tokio_agent_core::ToolGateResult::Deny {
                        reason: error.to_string(),
                    }
                }
                Err(error) => {
                    self.slot.fail(error.to_string());
                    tokio_agent_core::ToolGateResult::Deny {
                        reason: error.to_string(),
                    }
                }
            }
        })
    }

    fn respond<'a>(
        &'a self,
        invocation: tokio_agent_core::ToolInvocation,
        response: tokio_agent_extension_api::InteractionResponse,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> tokio_agent_core::provider::BoxFuture<'a, tokio_agent_core::ToolGateResult> {
        Box::pin(async move {
            let Some(target) = self
                .runtime
                .gate_target
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
            else {
                self.slot.fail("installed tool gate is unavailable");
                return tokio_agent_core::ToolGateResult::Deny {
                    reason: "installed tool gate is unavailable".into(),
                };
            };
            let runtime = self.runtime.clone();
            let result = tokio::task::spawn_blocking(move || {
                let (reply, receive) = mpsc::channel();
                runtime.tx.send(Request::GateRespond {
                    target,
                    invocation_id: invocation.invocation_id,
                    response,
                    reply,
                })?;
                let (response, effects) = receive.recv()??;
                runtime
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend(runtime.process_effects(effects));
                Ok::<_, anyhow::Error>(response)
            })
            .await;
            match result {
                Ok(Ok(response)) => gate_result(response),
                Ok(Err(error)) => {
                    self.slot.fail(error.to_string());
                    tokio_agent_core::ToolGateResult::Deny {
                        reason: error.to_string(),
                    }
                }
                Err(error) => {
                    self.slot.fail(error.to_string());
                    tokio_agent_core::ToolGateResult::Deny {
                        reason: error.to_string(),
                    }
                }
            }
        })
    }
}

fn gate_result(
    response: tokio_agent_extension_api::ToolGateResponse,
) -> tokio_agent_core::ToolGateResult {
    match response {
        tokio_agent_extension_api::ToolGateResponse::Allow { .. } => {
            tokio_agent_core::ToolGateResult::Allow
        }
        tokio_agent_extension_api::ToolGateResponse::Deny { reason, .. } => {
            tokio_agent_core::ToolGateResult::Deny { reason }
        }
        tokio_agent_extension_api::ToolGateResponse::RequestInteraction { interaction, .. } => {
            tokio_agent_core::ToolGateResult::RequestInteraction(interaction)
        }
    }
}

fn initializing_status_segment() -> tokio_agent_extension_api::StatusSegment {
    tokio_agent_extension_api::StatusSegment {
        id: "tokio-agent:extensions-initializing".to_owned(),
        text: "Initializing extensions…".to_owned(),
        tone: tokio_agent_extension_api::StatusTone::Muted,
        side: tokio_agent_extension_api::StatusSide::Right,
        priority: i16::MAX,
        min_width: 24,
    }
}

struct ExtensionTool {
    descriptor: tokio_agent_extension_api::ToolDescriptor,
    handler: String,
    generation: u64,
    version: String,
    runtime: ExtensionRuntime,
}
impl Tool for ExtensionTool {
    fn schema(&self) -> ToolDef {
        ToolDef {
            name: self.descriptor.name.clone(),
            description: self.descriptor.description.clone(),
            input_schema: self.descriptor.input_schema.clone(),
        }
    }
    fn effect(&self) -> tokio_agent_core::ToolEffect {
        self.descriptor.effect
    }

    fn owner(&self) -> tokio_agent_core::ToolOwner {
        tokio_agent_core::ToolOwner::Extension {
            id: self.descriptor.owner.clone(),
            version: self.version.clone(),
        }
    }

    fn summary(&self, _input: &serde_json::Value) -> Option<String> {
        Some(format!(
            "run extension tool owned by {}",
            self.descriptor.owner
        ))
    }
    fn run<'a>(
        &'a self,
        input: serde_json::Value,
        _ctx: &'a ToolCtx,
    ) -> tokio_agent_core::provider::BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let runtime = self.runtime.clone();
            let extension = self.descriptor.owner.clone();
            let generation = self.generation;
            let handler = self.handler.clone();
            let arguments = input.to_string();
            match tokio::task::spawn_blocking(move || {
                let (reply, receive) = mpsc::channel();
                runtime.tx.send(Request::Tool {
                    extension,
                    generation,
                    handler,
                    arguments,
                    reply,
                })?;
                let result = receive.recv()??;
                let effects = runtime.process_effects(result.effects.clone());
                runtime
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend(effects);
                Ok::<_, anyhow::Error>(result)
            })
            .await
            {
                Ok(Ok(result)) if result.is_error => ToolResult::error(result.content),
                Ok(Ok(result)) => ToolResult::ok(result.content),
                Ok(Err(error)) => ToolResult::error(error.to_string()),
                Err(error) => ToolResult::error(error.to_string()),
            }
        })
    }
}

#[cfg(test)]
mod network_tests {
    use super::is_public_ip;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn public_destinations_are_allowed() {
        assert!(is_public_ip(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        assert!(is_public_ip(IpAddr::V6(
            "2606:4700:4700::1111".parse::<Ipv6Addr>().unwrap()
        )));
    }

    #[test]
    fn local_metadata_and_reserved_destinations_are_blocked() {
        for address in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.0.0.1",
            "192.168.0.1",
            "198.18.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "2001:db8::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
        ] {
            let address = address.parse::<IpAddr>().unwrap();
            assert!(!is_public_ip(address), "{address} must be blocked");
        }
    }
}

use std::fs;
use std::time::Duration;

use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use semver::Version;
use tokio_agent_extension_api::{
    Capability, CommandId, ExtensionAction, ExtensionId, NetworkRequest, Sequenced, SessionCommand,
    StatusSegment, StatusSide, StatusTone, ToolDescriptor, ToolEffect, ToolId,
};
use tokio_agent_plugin::{
    ActionError, ActionOutcome, CommandRouter, ExtensionLock, LockedExtension, LockedSource,
    PackageStore, RootMetadata, RoutedCommand, SessionQueues, SignatureEntry, SignedEnvelope,
    SupervisorPolicy, SupervisorState, discover_prompt_commands_in, root_fingerprint,
    validate_package, verify_initial_root,
};

#[test]
fn project_markdown_command_overrides_user_and_renders_safe_values() {
    let temp = tempfile::tempdir().unwrap();
    let user = temp.path().join("user");
    let project = temp.path().join("project");
    fs::create_dir_all(&user).unwrap();
    fs::create_dir_all(&project).unwrap();
    fs::write(user.join("review.md"), "user {{ arguments }}").unwrap();
    fs::write(project.join("review.md"), "---\ndescription: Project review\nargument-hint: '[focus]'\n---\nproject {{ arguments }} in {{ cwd }}").unwrap();

    let commands = discover_prompt_commands_in(Some(&user), &project).unwrap();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].descriptor.description, "Project review");
    assert_eq!(
        commands[0].descriptor.usage.as_deref(),
        Some("/review [focus]")
    );
    assert_eq!(
        commands[0].render("tests; $(bad)", temp.path()),
        format!("project tests; $(bad) in {}", temp.path().display())
    );
}

#[test]
fn markdown_front_matter_rejects_unknown_fields() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("bad.md"), "---\nexecute: true\n---\nnope").unwrap();
    assert!(discover_prompt_commands_in(None, temp.path()).is_err());
}

#[cfg(unix)]
#[test]
fn command_symlink_escape_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let commands = temp.path().join("commands");
    fs::create_dir(&commands).unwrap();
    let outside = temp.path().join("outside.md");
    fs::write(&outside, "secret").unwrap();
    std::os::unix::fs::symlink(&outside, commands.join("escape.md")).unwrap();
    assert!(discover_prompt_commands_in(None, &commands).is_err());
}

#[test]
fn strict_manifest_and_package_paths_are_validated() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("commands")).unwrap();
    fs::write(temp.path().join("commands/test.md"), "hello").unwrap();
    fs::write(
        temp.path().join("extension.toml"),
        r#"
manifest_version = 1
id = "example.test.command"
name = "Test"
version = "1.2.3"
description = "test"
license = "MIT"
host_api = ">=1.0, <2.0"
[[commands]]
name = "test"
description = "test"
prompt = "commands/test.md"
"#,
    )
    .unwrap();
    assert!(validate_package(temp.path(), &Version::new(1, 0, 0)).is_ok());
    assert!(validate_package(temp.path(), &Version::new(2, 0, 0)).is_err());
}

#[test]
fn supervisor_rejects_stale_and_undeclared_actions_and_coalesces_status() {
    let mut supervisor = SupervisorState::new(SupervisorPolicy::default());
    let id = ExtensionId::new("example.test.service");
    let generation = supervisor.enable(id.clone(), [Capability::StatusWrite]);
    let status = StatusSegment {
        id: "tests".into(),
        text: "passing".into(),
        tone: StatusTone::Success,
        side: StatusSide::Left,
        priority: 10,
        min_width: 4,
    };
    let outcome = supervisor
        .apply(Sequenced {
            sequence: 1,
            extension: id.clone(),
            generation,
            value: ExtensionAction::SetStatusSegment(status.clone()),
        })
        .unwrap();
    assert_eq!(outcome, ActionOutcome::StatusUpdated(status));
    let automatic = supervisor.apply(Sequenced {
        sequence: 2,
        extension: id.clone(),
        generation,
        value: ExtensionAction::SubmitPrompt {
            text: "again".into(),
            automatic: true,
        },
    });
    assert_eq!(
        automatic.unwrap_err(),
        ActionError::Capability(Capability::SessionSubmitAutomatic)
    );
    supervisor.disable(&id);
    let stale = supervisor.apply(Sequenced {
        sequence: 3,
        extension: id,
        generation,
        value: ExtensionAction::CancelTimer(tokio_agent_extension_api::TimerId::new("x")),
    });
    assert_eq!(stale.unwrap_err(), ActionError::Stale);
}

#[test]
fn extension_can_clear_its_status_segment() {
    let mut supervisor = SupervisorState::new(SupervisorPolicy::default());
    let id = ExtensionId::new("example.test.service");
    let generation = supervisor.enable(id.clone(), [Capability::StatusWrite]);
    supervisor
        .apply(Sequenced {
            sequence: 1,
            extension: id.clone(),
            generation,
            value: ExtensionAction::SetStatusSegment(StatusSegment {
                id: "task".into(),
                text: "active".into(),
                tone: StatusTone::Normal,
                side: StatusSide::Left,
                priority: 10,
                min_width: 4,
            }),
        })
        .unwrap();

    let outcome = supervisor
        .apply(Sequenced {
            sequence: 2,
            extension: id,
            generation,
            value: ExtensionAction::ClearStatusSegment("task".into()),
        })
        .unwrap();

    assert_eq!(outcome, ActionOutcome::StatusCleared("task".into()));
    assert!(supervisor.status_segments().is_empty());
}

#[test]
fn autonomy_queue_and_timer_limits_are_enforced() {
    let mut supervisor = SupervisorState::new(SupervisorPolicy {
        minimum_timer_interval: Duration::from_secs(1),
        ..SupervisorPolicy::default()
    });
    let id = ExtensionId::new("example.test.auto");
    let generation = supervisor.enable(
        id.clone(),
        [
            Capability::SessionSubmitAutomatic,
            Capability::SessionSchedule,
        ],
    );
    let submit = || ExtensionAction::SubmitPrompt {
        text: "continue".into(),
        automatic: true,
    };
    supervisor
        .apply(Sequenced {
            sequence: 1,
            extension: id.clone(),
            generation,
            value: submit(),
        })
        .unwrap();
    assert_eq!(
        supervisor
            .apply(Sequenced {
                sequence: 2,
                extension: id.clone(),
                generation,
                value: submit()
            })
            .unwrap_err(),
        ActionError::AlreadyQueued
    );
    let timer = ExtensionAction::ScheduleTimer {
        id: tokio_agent_extension_api::TimerId::new("fast"),
        after: Duration::from_millis(1).into(),
    };
    assert_eq!(
        supervisor
            .apply(Sequenced {
                sequence: 3,
                extension: id,
                generation,
                value: timer
            })
            .unwrap_err(),
        ActionError::TimerLimit
    );
}

#[test]
fn shared_router_handles_builtins_prompts_and_unknown_ids() {
    let temp = tempfile::tempdir().unwrap();
    let commands_dir = temp.path().join("commands");
    fs::create_dir(&commands_dir).unwrap();
    fs::write(
        commands_dir.join("review.md"),
        "Review {{ arguments }} in {{ cwd }}",
    )
    .unwrap();
    let commands = discover_prompt_commands_in(None, &commands_dir).unwrap();
    let router = CommandRouter::new(commands).unwrap();

    assert!(
        router
            .catalog()
            .iter()
            .any(|command| command.name == "/clear")
    );
    let review = router.find_name("/review").unwrap();
    let routed = router
        .route(
            SessionCommand::InvokeCommand {
                id: review.id.clone(),
                arguments: "security".into(),
            },
            temp.path(),
        )
        .unwrap();
    assert!(
        matches!(routed, RoutedCommand::SubmitPrompt(text) if text.contains("Review security") && text.contains(&temp.path().to_string_lossy().to_string()))
    );

    assert!(
        router
            .route(
                SessionCommand::InvokeCommand {
                    id: CommandId::new("missing:command"),
                    arguments: String::new(),
                },
                temp.path(),
            )
            .is_err()
    );
}

#[test]
fn package_store_is_atomic_immutable_and_lock_preserves_registry_identity() {
    let temp = tempfile::tempdir().unwrap();
    let package = temp.path().join("package");
    fs::create_dir_all(package.join("commands")).unwrap();
    fs::write(package.join("commands/test.md"), "hello").unwrap();
    fs::write(
        package.join("extension.toml"),
        r#"manifest_version = 1
id = "example.test.command"
name = "Test"
version = "1.0.0"
description = "test"
license = "MIT"
host_api = ">=1.0, <2.0"
[[commands]]
name = "test"
description = "test"
prompt = "commands/test.md"
"#,
    )
    .unwrap();
    let store = PackageStore::new(temp.path().join("store"), Version::new(1, 0, 0));
    let installed = store.install_directory(&package, None).unwrap();
    assert!(installed.path.join("extension.toml").is_file());
    assert_eq!(store.list().unwrap().len(), 1);

    fs::write(package.join("commands/test.md"), "changed").unwrap();
    assert!(store.install_directory(&package, None).is_err());

    let lock_path = temp.path().join("project/.tokio-agent/extensions.lock");
    let mut lock = ExtensionLock::default();
    lock.upsert(LockedExtension {
        id: "example.test.command".into(),
        version: "1.0.0".into(),
        source: LockedSource::Registry {
            root_identity: "sha256:registry-a".into(),
            url: "https://registry.example".into(),
        },
        digest: installed.digest,
        host_api: "1.0.0".into(),
        capabilities: Default::default(),
        publisher: Some("Example".into()),
    });
    lock.save(&lock_path).unwrap();
    assert_eq!(ExtensionLock::load(&lock_path).unwrap(), lock);
}

#[test]
fn user_submissions_have_priority_over_automatic_work() {
    let mut queues = SessionQueues::default();
    queues
        .submit_automatic(ExtensionId::new("example.test.auto"), "automatic".into())
        .unwrap();
    queues.submit_user("user".into());
    assert!(matches!(
        queues.dequeue(),
        Some(tokio_agent_plugin::QueuedSubmission::User(text)) if text == "user"
    ));
}

#[test]
fn tuf_root_requires_matching_fingerprint_threshold_and_expiry() {
    let signing = SigningKey::from_bytes(&[7_u8; 32]);
    let key_id = "fixture-key".to_owned();
    let mut keys = std::collections::BTreeMap::new();
    keys.insert(
        key_id.clone(),
        base64::engine::general_purpose::STANDARD.encode(signing.verifying_key().as_bytes()),
    );
    let root = RootMetadata {
        version: 1,
        expires_unix: u64::MAX,
        registry_name: "Fixture".into(),
        operator: "Tests".into(),
        index_keys: keys.clone(),
        keys,
        threshold: 1,
        index_threshold: 1,
    };
    let payload = serde_json::to_vec(&root).unwrap();
    let signature =
        base64::engine::general_purpose::STANDARD.encode(signing.sign(&payload).to_bytes());
    let fingerprint = root_fingerprint(&root);
    let envelope = SignedEnvelope {
        signed: root,
        signatures: vec![SignatureEntry { key_id, signature }],
    };
    verify_initial_root(&envelope, &fingerprint, std::time::SystemTime::now()).unwrap();
    assert!(verify_initial_root(&envelope, "sha256:wrong", std::time::SystemTime::now()).is_err());
}

#[test]
fn autonomy_has_one_owner_and_release_allows_the_next_service() {
    let mut supervisor = SupervisorState::new(SupervisorPolicy::default());
    let first = ExtensionId::new("example.first.service");
    let second = ExtensionId::new("example.second.service");
    let first_generation = supervisor.enable(first.clone(), [Capability::SessionSubmitAutomatic]);
    let second_generation = supervisor.enable(second.clone(), [Capability::SessionSubmitAutomatic]);
    supervisor
        .apply(Sequenced {
            sequence: 1,
            extension: first.clone(),
            generation: first_generation,
            value: ExtensionAction::SubmitPrompt {
                text: "first".into(),
                automatic: true,
            },
        })
        .unwrap();
    let conflict = supervisor.apply(Sequenced {
        sequence: 2,
        extension: second.clone(),
        generation: second_generation,
        value: ExtensionAction::SubmitPrompt {
            text: "second".into(),
            automatic: true,
        },
    });
    assert_eq!(conflict.unwrap_err(), ActionError::AutonomyConflict);
    supervisor
        .apply(Sequenced {
            sequence: 3,
            extension: first,
            generation: first_generation,
            value: ExtensionAction::ReleaseAutonomy,
        })
        .unwrap();
    assert!(
        supervisor
            .apply(Sequenced {
                sequence: 4,
                extension: second,
                generation: second_generation,
                value: ExtensionAction::SubmitPrompt {
                    text: "second".into(),
                    automatic: true
                },
            })
            .is_ok()
    );
}

#[test]
fn reenabling_always_advances_the_generation() {
    let mut supervisor = SupervisorState::default();
    let id = ExtensionId::new("example.test.runtime");
    let first = supervisor.enable(id.clone(), []);
    supervisor.disable(&id);
    let second = supervisor.enable(id.clone(), []);
    assert!(second > first);
    assert_eq!(
        supervisor
            .apply(Sequenced {
                sequence: 1,
                extension: id,
                generation: first,
                value: ExtensionAction::ShowNotice {
                    level: tokio_agent_extension_api::NoticeLevel::Info,
                    text: "stale".into(),
                },
            })
            .unwrap_err(),
        ActionError::Stale
    );
}

#[test]
fn manifest_skills_are_explicit_lazy_prompt_commands() {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir_all(temp.path().join("skills")).unwrap();
    fs::write(
        temp.path().join("skills/testing.md"),
        "Use the integration workflow for {{ arguments }}",
    )
    .unwrap();
    fs::write(
        temp.path().join("extension.toml"),
        r#"manifest_version = 1
id = "example.test.skills"
name = "Skills"
version = "1.0.0"
description = "skills"
license = "MIT"
host_api = ">=1.0, <2.0"
[[skills]]
name = "testing"
description = "Activate testing guidance"
instructions = "skills/testing.md"
"#,
    )
    .unwrap();
    let manifest = validate_package(temp.path(), &Version::new(1, 0, 0)).unwrap();
    let commands = tokio_agent_plugin::commands_from_package(temp.path(), &manifest).unwrap();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].descriptor.name, "/testing");
    assert!(
        commands[0]
            .render("api", temp.path())
            .contains("workflow for api")
    );
}

#[test]
fn action_batches_roll_back_dynamic_tool_collisions() {
    let mut supervisor = tokio_agent_plugin::SessionSupervisor::new(SupervisorPolicy::default());
    // Enable declaratively through the policy state is intentionally exercised
    // elsewhere; this test uses a runtime-free manifest and direct actions.
    let manifest = tokio_agent_plugin::ExtensionManifest {
        manifest_version: 1,
        id: "example.test.tools".into(),
        name: "Tools".into(),
        version: "1.0.0".into(),
        description: "tools".into(),
        license: "MIT".into(),
        host_api: ">=1.0, <2.0".into(),
        runtime: None,
        commands: Vec::new(),
        skills: Vec::new(),
        tools: Vec::new(),
        status: Vec::new(),
        tool_gate: None,
        cli_options: Vec::new(),
        capabilities: tokio_agent_plugin::Capabilities {
            tools_dynamic: true,
            ..Default::default()
        },
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let generation = runtime
        .block_on(supervisor.enable_programmable(
            &manifest,
            std::path::Path::new("."),
            Default::default(),
        ))
        .unwrap();
    let owner = ExtensionId::new(&manifest.id);
    let tool = |id: &str| ToolDescriptor {
        id: ToolId::new(id),
        name: "duplicate".into(),
        description: "test".into(),
        input_schema: serde_json::json!({"type":"object"}),
        owner: owner.clone(),
        effect: ToolEffect::Read,
    };
    let batch = vec![
        Sequenced {
            sequence: 1,
            extension: owner.clone(),
            generation,
            value: ExtensionAction::RegisterTool(tool("first")),
        },
        Sequenced {
            sequence: 2,
            extension: owner.clone(),
            generation,
            value: ExtensionAction::RegisterTool(tool("second")),
        },
    ];
    assert!(supervisor.apply_actions(batch).is_err());
    assert!(
        supervisor
            .apply_actions(vec![Sequenced {
                sequence: 3,
                extension: owner.clone(),
                generation,
                value: ExtensionAction::RegisterTool(tool("after-rollback")),
            }])
            .is_ok()
    );
}

#[test]
fn status_update_rate_is_bounded() {
    let mut supervisor = SupervisorState::default();
    let owner = ExtensionId::new("example.test.status");
    let generation = supervisor.enable(owner.clone(), [Capability::StatusWrite]);
    for sequence in 0..10 {
        supervisor
            .apply(Sequenced {
                sequence,
                extension: owner.clone(),
                generation,
                value: ExtensionAction::SetStatusSegment(StatusSegment {
                    id: "status".into(),
                    text: sequence.to_string(),
                    tone: StatusTone::Normal,
                    side: StatusSide::Left,
                    priority: 0,
                    min_width: 1,
                }),
            })
            .unwrap();
    }
    assert_eq!(
        supervisor
            .apply(Sequenced {
                sequence: 11,
                extension: owner,
                generation,
                value: ExtensionAction::SetStatusSegment(StatusSegment {
                    id: "status".into(),
                    text: "too-fast".into(),
                    tone: StatusTone::Normal,
                    side: StatusSide::Left,
                    priority: 0,
                    min_width: 1,
                }),
            })
            .unwrap_err(),
        ActionError::StatusRateLimit
    );
}

#[test]
fn network_requests_require_capability_and_are_rate_limited() {
    let owner = ExtensionId::new("example.test.network");
    let mut supervisor = SupervisorState::new(SupervisorPolicy {
        maximum_network_requests_per_minute: 1,
        ..SupervisorPolicy::default()
    });
    let generation = supervisor.enable(owner.clone(), []);
    let fetch = || {
        ExtensionAction::Fetch(NetworkRequest {
            id: "weather".into(),
            url: "https://example.com/weather".into(),
        })
    };
    assert_eq!(
        supervisor
            .apply(Sequenced {
                sequence: 1,
                extension: owner.clone(),
                generation,
                value: fetch(),
            })
            .unwrap_err(),
        ActionError::Capability(Capability::NetworkRequest)
    );

    let generation = supervisor.enable(owner.clone(), [Capability::NetworkRequest]);
    assert!(matches!(
        supervisor
            .apply(Sequenced {
                sequence: 2,
                extension: owner.clone(),
                generation,
                value: fetch(),
            })
            .unwrap(),
        ActionOutcome::NetworkRequested(_)
    ));
    assert_eq!(
        supervisor
            .apply(Sequenced {
                sequence: 3,
                extension: owner,
                generation,
                value: fetch(),
            })
            .unwrap_err(),
        ActionError::QueueLimit
    );
}

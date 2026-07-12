#![forbid(unsafe_code)]

mod catalog;
mod commands;
mod companion;
mod config;
mod lockfile;
mod manifest;
mod prompt;
mod registry;
mod reload;
mod runtime;
mod store;
mod supervisor;
mod trust;
mod tuf;

pub use catalog::{CatalogError, CommandCatalog};
pub use commands::{
    BuiltInCommand, CLEAR_COMMAND_ID, CommandRouter, EXTENSIONS_COMMAND_ID, MODEL_COMMAND_ID,
    PERMISSIONS_COMMAND_ID, PROVIDERS_COMMAND_ID, RouteError, RoutedCommand,
    builtin_command_catalog,
};
pub use companion::{CompanionError, CompanionManager, locate_companion};
pub use config::{CapabilityGrant, ConfigError, ExtensionConfig, RegistryReference};
pub use lockfile::{ExtensionLock, LockError, LockedExtension, LockedSource};
pub use manifest::{
    Capabilities, CommandContribution, ExtensionManifest, ManifestError, RuntimeContribution,
    SkillContribution, StatusContribution, ToolContribution, validate_package,
};
pub use prompt::{
    DiscoveryError, LocalCommandPaths, PromptCommand, commands_from_package,
    discover_prompt_commands, discover_prompt_commands_in, render_template,
};
pub use registry::{
    CachedRegistry, OFFICIAL_REGISTRY_URL, RegistryCatalog, RegistryError, RegistryIndex,
    RegistryPackage, RegistryTrust, SearchResult, builtin_official_root,
};
pub use reload::{DeclarativeReloader, PackageReloader};
pub use runtime::{RuntimeError, RuntimeToolResult, SessionSupervisor, SupervisorEffect};
pub use store::{InstalledPackage, PackageStore, StoreError, package_digest};
pub use supervisor::{
    ActionError, ActionOutcome, AutomaticSubmission, QueuedSubmission, SessionQueues,
    SupervisorPolicy, SupervisorState, TimerQueue,
};
pub use trust::{RegistryTrustStore, TrustError, TrustedRegistry};
pub use tuf::{
    RootMetadata, SignatureEntry, SignedEnvelope, TufError, root_fingerprint, verify_initial_root,
    verify_role, verify_root_rotation,
};

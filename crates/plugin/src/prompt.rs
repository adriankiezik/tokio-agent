use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use tokio_agent_extension_api::{CommandDescriptor, CommandId, CommandSource};

const MAX_COMMAND_BYTES: u64 = 256 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("could not read command directory {path}: {source}")]
    ReadDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not read command file {path}: {source}")]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("command file is too large: {0}")]
    TooLarge(PathBuf),
    #[error("invalid command name `{0}`")]
    InvalidName(String),
    #[error("invalid front matter in {path}: {message}")]
    FrontMatter { path: PathBuf, message: String },
    #[error("command file escapes its command directory: {0}")]
    PathEscape(PathBuf),
}

#[derive(Debug, Clone)]
pub struct LocalCommandPaths {
    pub user: Option<PathBuf>,
    pub project: PathBuf,
}

impl LocalCommandPaths {
    #[must_use]
    pub fn for_project(project: &Path) -> Self {
        let user = dirs::config_dir().map(|path| path.join("tokio-agent/commands"));
        Self {
            user,
            project: project.join(".tokio-agent/commands"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCommand {
    pub descriptor: CommandDescriptor,
    pub template: String,
    pub path: PathBuf,
}

impl PromptCommand {
    #[must_use]
    pub fn render(&self, arguments: &str, cwd: &Path) -> String {
        render_template(&self.template, arguments, cwd)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct FrontMatter {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    argument_hint: Option<String>,
}

pub fn commands_from_package(
    root: &Path,
    manifest: &crate::ExtensionManifest,
) -> Result<Vec<PromptCommand>, DiscoveryError> {
    let canonical_root =
        fs::canonicalize(root).map_err(|source| DiscoveryError::ReadDirectory {
            path: root.to_path_buf(),
            source,
        })?;
    let mut commands = Vec::new();
    for contribution in &manifest.commands {
        let Some(relative) = contribution.prompt.as_ref() else {
            continue;
        };
        let path = root.join(relative);
        let canonical = fs::canonicalize(&path).map_err(|source| DiscoveryError::ReadFile {
            path: path.clone(),
            source,
        })?;
        if !canonical.starts_with(&canonical_root) {
            return Err(DiscoveryError::PathEscape(path));
        }
        let template =
            fs::read_to_string(&canonical).map_err(|source| DiscoveryError::ReadFile {
                path: path.clone(),
                source,
            })?;
        commands.push(PromptCommand {
            descriptor: CommandDescriptor {
                id: CommandId::new(format!("{}:{}", manifest.id, contribution.name)),
                name: format!("/{}", contribution.name),
                description: contribution.description.clone(),
                usage: contribution.usage.clone(),
                source: CommandSource::Extension {
                    id: tokio_agent_extension_api::ExtensionId::new(&manifest.id),
                    version: manifest.version.clone(),
                },
                available_while_running: contribution.available_while_running,
            },
            template,
            path,
        });
    }
    for skill in &manifest.skills {
        let path = root.join(&skill.instructions);
        let canonical = fs::canonicalize(&path).map_err(|source| DiscoveryError::ReadFile {
            path: path.clone(),
            source,
        })?;
        if !canonical.starts_with(&canonical_root) {
            return Err(DiscoveryError::PathEscape(path));
        }
        let template =
            fs::read_to_string(&canonical).map_err(|source| DiscoveryError::ReadFile {
                path: path.clone(),
                source,
            })?;
        commands.push(PromptCommand {
            descriptor: CommandDescriptor {
                id: CommandId::new(format!("{}:skill-{}", manifest.id, skill.name)),
                name: format!("/{}", skill.name),
                description: skill.description.clone(),
                usage: skill.usage.clone(),
                source: CommandSource::Extension {
                    id: tokio_agent_extension_api::ExtensionId::new(&manifest.id),
                    version: manifest.version.clone(),
                },
                available_while_running: false,
            },
            template,
            path,
        });
    }
    Ok(commands)
}

pub fn discover_prompt_commands(project: &Path) -> Result<Vec<PromptCommand>, DiscoveryError> {
    let paths = LocalCommandPaths::for_project(project);
    discover_prompt_commands_in(paths.user.as_deref(), &paths.project)
}

/// Discovers user commands first and overlays project commands by command name.
pub fn discover_prompt_commands_in(
    user_dir: Option<&Path>,
    project_dir: &Path,
) -> Result<Vec<PromptCommand>, DiscoveryError> {
    let mut commands = BTreeMap::new();
    if let Some(user) = user_dir {
        discover_dir(user, false, &mut commands)?;
    }
    discover_dir(project_dir, true, &mut commands)?;
    Ok(commands.into_values().collect())
}

fn discover_dir(
    root: &Path,
    project: bool,
    output: &mut BTreeMap<String, PromptCommand>,
) -> Result<(), DiscoveryError> {
    if !root.exists() {
        return Ok(());
    }
    let canonical_root =
        fs::canonicalize(root).map_err(|source| DiscoveryError::ReadDirectory {
            path: root.to_path_buf(),
            source,
        })?;
    let entries = fs::read_dir(root).map_err(|source| DiscoveryError::ReadDirectory {
        path: root.to_path_buf(),
        source,
    })?;
    let mut paths = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| DiscoveryError::ReadDirectory {
            path: root.to_path_buf(),
            source,
        })?;
    paths.sort();
    for path in paths {
        if path.extension().and_then(|value| value.to_str()) != Some("md") {
            continue;
        }
        let canonical = fs::canonicalize(&path).map_err(|source| DiscoveryError::ReadFile {
            path: path.clone(),
            source,
        })?;
        if !canonical.starts_with(&canonical_root) {
            return Err(DiscoveryError::PathEscape(path));
        }
        let metadata = fs::metadata(&canonical).map_err(|source| DiscoveryError::ReadFile {
            path: path.clone(),
            source,
        })?;
        if !metadata.is_file() {
            continue;
        }
        if metadata.len() > MAX_COMMAND_BYTES {
            return Err(DiscoveryError::TooLarge(path));
        }
        let name = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        validate_command_name(name)?;
        let contents =
            fs::read_to_string(&canonical).map_err(|source| DiscoveryError::ReadFile {
                path: path.clone(),
                source,
            })?;
        let (front, template) = parse_document(&path, &contents)?;
        let scope = if project { "project" } else { "user" };
        let id = CommandId::new(format!("local.{scope}.{name}:{name}"));
        output.insert(
            name.to_owned(),
            PromptCommand {
                descriptor: CommandDescriptor {
                    id,
                    name: format!("/{name}"),
                    description: front
                        .description
                        .unwrap_or_else(|| format!("Local /{name} command")),
                    usage: front.argument_hint.map(|hint| format!("/{name} {hint}")),
                    source: CommandSource::Local {
                        path: path.to_string_lossy().into_owned(),
                    },
                    available_while_running: false,
                },
                template: template.trim().to_owned(),
                path,
            },
        );
    }
    Ok(())
}

fn parse_document<'a>(
    path: &Path,
    contents: &'a str,
) -> Result<(FrontMatter, &'a str), DiscoveryError> {
    let Some(after_open) = contents.strip_prefix("---\n") else {
        return Ok((FrontMatter::default(), contents));
    };
    let Some(end) = after_open.find("\n---\n") else {
        return Err(DiscoveryError::FrontMatter {
            path: path.to_path_buf(),
            message: "missing closing `---`".into(),
        });
    };
    let metadata = &after_open[..end];
    let front = serde_yaml::from_str::<FrontMatter>(metadata).map_err(|error| {
        DiscoveryError::FrontMatter {
            path: path.to_path_buf(),
            message: error.to_string(),
        }
    })?;
    Ok((front, &after_open[end + 5..]))
}

fn validate_command_name(name: &str) -> Result<(), DiscoveryError> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(DiscoveryError::InvalidName(name.to_owned()))
    }
}

#[must_use]
pub fn render_template(template: &str, arguments: &str, cwd: &Path) -> String {
    template
        .replace("{{ arguments }}", arguments)
        .replace("{{arguments}}", arguments)
        .replace("{{ cwd }}", &cwd.to_string_lossy())
        .replace("{{cwd}}", &cwd.to_string_lossy())
}

#[allow(dead_code)]
fn _assert_safe_relative(path: &Path) -> bool {
    path.components()
        .all(|part| matches!(part, Component::Normal(_)))
}

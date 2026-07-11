use std::path::Path;

use globset::GlobBuilder;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_agent_core::provider::BoxFuture;
use tokio_agent_core::tool::{Action, PermissionRequest, Tool, ToolCtx, ToolDef, ToolResult};
use tokio_util::sync::CancellationToken;

use crate::fs::{display_path, resolve};

const DEFAULT_LIMIT: usize = 1000;
const MAX_LIMIT: usize = 10_000;

pub struct Glob;

#[derive(Debug, Deserialize)]
struct Args {
    pattern: String,
    #[serde(default = "default_path")]
    path: String,
    limit: Option<usize>,
}

fn default_path() -> String {
    ".".to_owned()
}

impl Tool for Glob {
    fn schema(&self) -> ToolDef {
        ToolDef {
            name: "glob".to_owned(),
            description: "Find files recursively by glob pattern. Respects Git ignore files and returns paths sorted lexicographically."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern such as `**/*.rs`." },
                    "path": { "type": "string", "description": "Directory to search (default `.`)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "description": "Maximum results (default 1000)." }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    fn permission(&self, input: &Value) -> PermissionRequest {
        let pattern = input
            .get("pattern")
            .and_then(Value::as_str)
            .unwrap_or("<missing>");
        PermissionRequest {
            tool: "glob".to_owned(),
            summary: format!("glob {pattern}"),
            action: Action::Read,
        }
    }

    fn run<'a>(&'a self, input: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let args: Args = match serde_json::from_value(input) {
                Ok(args) => args,
                Err(error) => return ToolResult::error(format!("invalid arguments: {error}")),
            };
            let cwd = ctx.cwd.clone();
            let cancel = ctx.cancel.clone();
            match tokio::task::spawn_blocking(move || run_glob(args, &cwd, &cancel)).await {
                Ok(result) => result,
                Err(error) => ToolResult::error(format!("glob task failed: {error}")),
            }
        })
    }
}

fn run_glob(args: Args, cwd: &Path, cancel: &CancellationToken) -> ToolResult {
    if cancel.is_cancelled() {
        return ToolResult::error("cancelled by user");
    }
    let root = resolve(cwd, &args.path);
    if !root.is_dir() {
        return ToolResult::error(format!("cannot search {}: not a directory", root.display()));
    }
    let matcher = match GlobBuilder::new(&args.pattern).build() {
        Ok(glob) => glob.compile_matcher(),
        Err(error) => return ToolResult::error(format!("invalid glob pattern: {error}")),
    };
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let mut matches = Vec::new();
    let mut truncated = false;
    for entry in walker(&root) {
        if cancel.is_cancelled() {
            return ToolResult::error("cancelled by user");
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(&root).unwrap_or(entry.path());
        if matcher.is_match(relative) {
            if matches.len() == limit {
                truncated = true;
                break;
            }
            matches.push(display_path(cwd, entry.path()));
        }
    }
    matches.sort_unstable();
    if matches.is_empty() {
        return ToolResult::ok("(no matches)");
    }
    let mut output = matches.join("\n");
    if truncated {
        output.push_str(&format!("\n[results truncated at {limit} files]"));
    }
    ToolResult::ok(output)
}

pub(crate) fn walker(root: &Path) -> ignore::Walk {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .parents(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|entry| entry.file_name() != ".git");
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_nested_files_and_ignores_git_metadata() {
        let root = temp_dir();
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(root.join("src/nested/mod.rs"), "").unwrap();
        std::fs::write(root.join(".git/config.rs"), "").unwrap();
        let result = run_glob(
            Args {
                pattern: "**/*.rs".into(),
                path: ".".into(),
                limit: None,
            },
            &root,
            &CancellationToken::new(),
        );
        let tokio_agent_core::message::ToolOutput::Text(output) = result.output;
        assert_eq!(output, "src/lib.rs\nsrc/nested/mod.rs");
        let _ = std::fs::remove_dir_all(root);
    }

    fn temp_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "tokio-agent-glob-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}

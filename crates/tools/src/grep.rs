use std::fmt::Write as _;
use std::path::Path;

use globset::GlobBuilder;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_agent_core::provider::BoxFuture;
use tokio_agent_core::tool::{Action, PermissionRequest, Tool, ToolCtx, ToolDef, ToolResult};
use tokio_util::sync::CancellationToken;

use crate::fs::{display_path, resolve};
use crate::glob::walker;

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 2000;
const MAX_LINE_CHARS: usize = 500;

pub struct Grep;

#[derive(Debug, Deserialize)]
struct Args {
    pattern: String,
    #[serde(default = "default_path")]
    path: String,
    glob: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
    limit: Option<usize>,
}

fn default_path() -> String {
    ".".to_owned()
}

impl Tool for Grep {
    fn schema(&self) -> ToolDef {
        ToolDef {
            name: "grep".to_owned(),
            description: "Search UTF-8 file contents recursively with a Rust regular expression. Respects Git ignore files and returns `path:line:text` matches."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regular expression to search for." },
                    "path": { "type": "string", "description": "File or directory to search (default `.`)." },
                    "glob": { "type": "string", "description": "Optional file glob such as `*.rs`." },
                    "case_insensitive": { "type": "boolean", "description": "Use case-insensitive matching (default false)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "description": "Maximum matching lines (default 200)." }
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
            tool: "grep".to_owned(),
            summary: format!("grep {pattern}"),
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
            match tokio::task::spawn_blocking(move || run_grep(args, &cwd, &cancel)).await {
                Ok(result) => result,
                Err(error) => ToolResult::error(format!("grep task failed: {error}")),
            }
        })
    }
}

fn run_grep(args: Args, cwd: &Path, cancel: &CancellationToken) -> ToolResult {
    if cancel.is_cancelled() {
        return ToolResult::error("cancelled by user");
    }
    let regex = match RegexBuilder::new(&args.pattern)
        .case_insensitive(args.case_insensitive)
        .build()
    {
        Ok(regex) => regex,
        Err(error) => return ToolResult::error(format!("invalid regular expression: {error}")),
    };
    let file_matcher = match args.glob.as_deref() {
        Some(pattern) => match GlobBuilder::new(pattern).build() {
            Ok(glob) => Some(glob.compile_matcher()),
            Err(error) => return ToolResult::error(format!("invalid file glob: {error}")),
        },
        None => None,
    };
    let root = resolve(cwd, &args.path);
    if !root.exists() {
        return ToolResult::error(format!(
            "cannot search {}: path does not exist",
            root.display()
        ));
    }
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let mut output = String::new();
    let mut count = 0usize;
    let mut truncated = false;

    let mut visit = |path: &Path, relative: &Path| -> bool {
        if file_matcher
            .as_ref()
            .is_some_and(|matcher| !matcher.is_match(relative))
        {
            return false;
        }
        let Ok(contents) = std::fs::read_to_string(path) else {
            return false;
        };
        for (index, line) in contents.lines().enumerate() {
            if regex.is_match(line) {
                if count == limit {
                    return true;
                }
                let line = truncate_line(line);
                writeln!(output, "{}:{}:{line}", display_path(cwd, path), index + 1)
                    .expect("writing to a String cannot fail");
                count += 1;
            }
        }
        false
    };

    if root.is_file() {
        let relative = root.file_name().map(Path::new).unwrap_or(&root);
        truncated = visit(&root, relative);
    } else if root.is_dir() {
        for entry in walker(&root) {
            if cancel.is_cancelled() {
                return ToolResult::error("cancelled by user");
            }
            if truncated {
                break;
            }
            let Ok(entry) = entry else {
                continue;
            };
            if entry.file_type().is_some_and(|kind| kind.is_file()) {
                let relative = entry.path().strip_prefix(&root).unwrap_or(entry.path());
                truncated = visit(entry.path(), relative);
            }
        }
    } else {
        return ToolResult::error(format!(
            "cannot search {}: unsupported file type",
            root.display()
        ));
    }

    if count == 0 {
        return ToolResult::ok("(no matches)");
    }
    if truncated {
        writeln!(output, "[results truncated at {limit} matching lines]")
            .expect("writing to a String cannot fail");
    }
    ToolResult::ok(output.trim_end().to_owned())
}

fn truncate_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_owned();
    }
    format!(
        "{}…",
        line.chars().take(MAX_LINE_CHARS - 1).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn searches_recursively_with_file_filter_and_line_numbers() {
        let root = std::env::temp_dir().join(format!("tokio-agent-grep-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "first\nNeedle here\n").unwrap();
        std::fs::write(root.join("src/readme.md"), "Needle ignored\n").unwrap();
        let result = run_grep(
            Args {
                pattern: "needle".into(),
                path: ".".into(),
                glob: Some("*.rs".into()),
                case_insensitive: true,
                limit: None,
            },
            &root,
            &CancellationToken::new(),
        );
        let tokio_agent_core::message::ToolOutput::Text(output) = result.output;
        assert_eq!(output, "src/lib.rs:2:Needle here");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_invalid_regular_expressions() {
        let result = run_grep(
            Args {
                pattern: "[".into(),
                path: ".".into(),
                glob: None,
                case_insensitive: false,
                limit: None,
            },
            Path::new("."),
            &CancellationToken::new(),
        );
        assert!(result.is_error);
    }
}

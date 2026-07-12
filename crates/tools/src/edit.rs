use serde::Deserialize;
use serde_json::{Value, json};
use tokio_agent_core::provider::BoxFuture;
use tokio_agent_core::tool::{Tool, ToolCtx, ToolDef, ToolEffect, ToolResult};

use crate::fs::{display_path, resolve, write_file};

pub struct Edit;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Replacement {
    pub(crate) old_text: String,
    pub(crate) new_text: String,
    #[serde(default)]
    pub(crate) replace_all: bool,
}

#[derive(Debug, Deserialize)]
struct Args {
    path: String,
    #[serde(flatten)]
    replacement: Replacement,
}

impl Tool for Edit {
    fn schema(&self) -> ToolDef {
        ToolDef {
            name: "edit".to_owned(),
            description: "Replace an exact string in a UTF-8 text file. By default the old text must occur exactly once; set `replace_all` to replace every occurrence."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file." },
                    "old_text": { "type": "string", "description": "Exact text to replace." },
                    "new_text": { "type": "string", "description": "Replacement text." },
                    "replace_all": { "type": "boolean", "description": "Replace every occurrence (default false)." }
                },
                "required": ["path", "old_text", "new_text"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Edit
    }

    fn summary(&self, input: &Value) -> Option<String> {
        edit_summary(input)
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
            let path = resolve(&ctx.cwd, &args.path);
            let contents = match tokio::fs::read_to_string(&path).await {
                Ok(contents) => contents,
                Err(error) => {
                    return ToolResult::error(format!("cannot edit {}: {error}", path.display()));
                }
            };
            let (updated, count) = match apply_replacement(contents, &args.replacement) {
                Ok(result) => result,
                Err(error) => return ToolResult::error(error),
            };
            if let Err(error) = write_file(&path, &updated).await {
                return ToolResult::error(format!("cannot edit {}: {error}", path.display()));
            }
            ToolResult::ok(format!(
                "Replaced {count} {} in {}",
                if count == 1 {
                    "occurrence"
                } else {
                    "occurrences"
                },
                display_path(&ctx.cwd, &path)
            ))
        })
    }
}

pub(crate) fn edit_summary(input: &Value) -> Option<String> {
    input
        .get("path")
        .and_then(Value::as_str)
        .map(|path| format!("edit {path}"))
}

pub(crate) fn apply_replacement(
    contents: String,
    replacement: &Replacement,
) -> Result<(String, usize), String> {
    if replacement.old_text.is_empty() {
        return Err("`old_text` must not be empty".to_owned());
    }
    let count = contents.matches(&replacement.old_text).count();
    if count == 0 {
        return Err("old text was not found; the file was not changed".to_owned());
    }
    if !replacement.replace_all && count != 1 {
        return Err(format!(
            "old text occurs {count} times; provide more context to make it unique or set `replace_all`"
        ));
    }
    let updated = if replacement.replace_all {
        contents.replace(&replacement.old_text, &replacement.new_text)
    } else {
        contents.replacen(&replacement.old_text, &replacement.new_text, 1)
    };
    Ok((updated, count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_replacement_succeeds() {
        let replacement = Replacement {
            old_text: "one".into(),
            new_text: "two".into(),
            replace_all: false,
        };
        assert_eq!(
            apply_replacement("one fish".into(), &replacement).unwrap(),
            ("two fish".into(), 1)
        );
    }

    #[test]
    fn ambiguous_replacement_does_not_modify_content() {
        let replacement = Replacement {
            old_text: "fish".into(),
            new_text: "cat".into(),
            replace_all: false,
        };
        assert!(
            apply_replacement("fish fish".into(), &replacement)
                .unwrap_err()
                .contains("2 times")
        );
    }

    #[test]
    fn replace_all_reports_the_number_changed() {
        let replacement = Replacement {
            old_text: "fish".into(),
            new_text: "cat".into(),
            replace_all: true,
        };
        assert_eq!(
            apply_replacement("fish fish".into(), &replacement).unwrap(),
            ("cat cat".into(), 2)
        );
    }
}

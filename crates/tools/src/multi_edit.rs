use serde::Deserialize;
use serde_json::{Value, json};
use tokio_agent_core::provider::BoxFuture;
use tokio_agent_core::tool::{Tool, ToolCtx, ToolDef, ToolResult};

use crate::edit::{Replacement, apply_replacement, edit_summary};
use crate::fs::{display_path, resolve, write_file};

pub struct MultiEdit;

#[derive(Debug, Deserialize)]
struct Args {
    path: String,
    edits: Vec<Replacement>,
}

impl Tool for MultiEdit {
    fn schema(&self) -> ToolDef {
        ToolDef {
            name: "multi_edit".to_owned(),
            description: "Apply a sequence of exact-string replacements to one UTF-8 text file. Edits are evaluated in order and the file is written only if every edit is valid."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file." },
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_text": { "type": "string", "description": "Exact text to replace." },
                                "new_text": { "type": "string", "description": "Replacement text." },
                                "replace_all": { "type": "boolean", "description": "Replace every occurrence (default false)." }
                            },
                            "required": ["old_text", "new_text"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["path", "edits"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> tokio_agent_core::tool::ToolEffect {
        tokio_agent_core::tool::ToolEffect::Edit
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
            if args.edits.is_empty() {
                return ToolResult::error("`edits` must contain at least one edit");
            }
            let path = resolve(&ctx.cwd, &args.path);
            let mut contents = match tokio::fs::read_to_string(&path).await {
                Ok(contents) => contents,
                Err(error) => {
                    return ToolResult::error(format!("cannot edit {}: {error}", path.display()));
                }
            };
            let mut replacements = 0usize;
            for (index, edit) in args.edits.iter().enumerate() {
                match apply_replacement(contents, edit) {
                    Ok((updated, count)) => {
                        contents = updated;
                        replacements += count;
                    }
                    Err(error) => {
                        return ToolResult::error(format!(
                            "edit {} failed: {error}; the file was not changed",
                            index + 1
                        ));
                    }
                }
            }
            if let Err(error) = write_file(&path, &contents).await {
                return ToolResult::error(format!("cannot edit {}: {error}", path.display()));
            }
            ToolResult::ok(format!(
                "Applied {} edits ({replacements} replacements) to {}",
                args.edits.len(),
                display_path(&ctx.cwd, &path)
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn failure_leaves_the_original_file_unchanged() {
        let root =
            std::env::temp_dir().join(format!("tokio-agent-multi-edit-{}", std::process::id()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(root.join("file.txt"), "alpha beta")
            .await
            .unwrap();
        let ctx = ToolCtx::new(root.clone(), CancellationToken::new());
        let result = MultiEdit
            .run(
                json!({
                    "path": "file.txt",
                    "edits": [
                        {"old_text": "alpha", "new_text": "omega"},
                        {"old_text": "missing", "new_text": "value"}
                    ]
                }),
                &ctx,
            )
            .await;
        assert!(result.is_error);
        assert_eq!(
            tokio::fs::read_to_string(root.join("file.txt"))
                .await
                .unwrap(),
            "alpha beta"
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}

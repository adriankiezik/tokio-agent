use serde::Deserialize;
use serde_json::{Value, json};
use tokio_agent_core::provider::BoxFuture;
use tokio_agent_core::tool::{Action, PermissionRequest, Tool, ToolCtx, ToolDef, ToolResult};

use crate::fs::{display_path, resolve, write_file};

pub struct Write;

#[derive(Debug, Deserialize)]
struct Args {
    path: String,
    content: String,
}

impl Tool for Write {
    fn schema(&self) -> ToolDef {
        ToolDef {
            name: "write".to_owned(),
            description:
                "Create or overwrite a UTF-8 text file. Parent directories are created as needed."
                    .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file." },
                    "content": { "type": "string", "description": "Complete file contents." }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    fn permission(&self, input: &Value) -> PermissionRequest {
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("<missing>");
        PermissionRequest {
            tool: "write".to_owned(),
            summary: format!("write {path}"),
            action: Action::Edit,
        }
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
            match write_file(&path, &args.content).await {
                Ok(()) => ToolResult::ok(format!(
                    "Wrote {} bytes to {}",
                    args.content.len(),
                    display_path(&ctx.cwd, &path)
                )),
                Err(error) => {
                    ToolResult::error(format!("cannot write {}: {error}", path.display()))
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn creates_parent_directories_and_writes_contents() {
        let root = temp_dir("write");
        let ctx = ToolCtx::new(root.clone(), CancellationToken::new());
        let result = Write
            .run(json!({"path": "nested/file.txt", "content": "hello"}), &ctx)
            .await;
        assert!(!result.is_error);
        assert_eq!(
            tokio::fs::read_to_string(root.join("nested/file.txt"))
                .await
                .unwrap(),
            "hello"
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    fn temp_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "tokio-agent-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}

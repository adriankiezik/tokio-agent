use serde::Deserialize;
use serde_json::{Value, json};
use std::fmt::Write;
use tokio::io::AsyncBufReadExt;
use tokio_agent_core::provider::BoxFuture;
use tokio_agent_core::tool::{Action, PermissionRequest, Tool, ToolCtx, ToolDef, ToolResult};

use crate::fs::resolve;

const DEFAULT_LIMIT: usize = 2000;

pub struct Read;

#[derive(Debug, Deserialize)]
struct Args {
    path: String,
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
}

impl Tool for Read {
    fn schema(&self) -> ToolDef {
        ToolDef {
            name: "read".to_owned(),
            description: "Read a file from the filesystem. Returns lines numbered from `offset` \
                (0-based, default 0), up to `limit` lines (default 2000)."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file." },
                    "offset": { "type": "integer", "description": "0-based first line to read." },
                    "limit": { "type": "integer", "description": "Maximum lines to read." }
                },
                "required": ["path"]
            }),
        }
    }

    fn permission(&self, input: &Value) -> PermissionRequest {
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("<missing>");
        PermissionRequest {
            tool: "read".to_owned(),
            summary: format!("read {path}"),
            action: Action::Read,
        }
    }

    fn run<'a>(&'a self, input: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let args: Args = match serde_json::from_value(input) {
                Ok(a) => a,
                Err(e) => return ToolResult::error(format!("invalid arguments: {e}")),
            };

            let path = resolve(&ctx.cwd, &args.path);
            let file = match tokio::fs::File::open(&path).await {
                Ok(f) => f,
                Err(e) => return ToolResult::error(format!("cannot read {}: {e}", path.display())),
            };

            let limit = args.limit.unwrap_or(DEFAULT_LIMIT);
            let end = args.offset.saturating_add(limit);
            let mut lines = tokio::io::BufReader::new(file).lines();
            let mut out = String::new();
            let mut idx = 0;
            let mut selected_has_content = false;
            let mut reached_eof = false;
            loop {
                let line = match lines.next_line().await {
                    Ok(Some(l)) => l,
                    Ok(None) => {
                        reached_eof = true;
                        break;
                    }
                    Err(e) => {
                        return ToolResult::error(format!("cannot read {}: {e}", path.display()));
                    }
                };
                if idx >= end {
                    break;
                }
                if idx >= args.offset {
                    selected_has_content |= !line.is_empty();
                    writeln!(out, "{:>6}\t{line}", idx + 1)
                        .expect("writing to a String cannot fail");
                }
                idx += 1;
            }

            if args.offset == 0 && reached_eof && !selected_has_content {
                return ToolResult::ok("(empty file)");
            }
            if out.is_empty() {
                return ToolResult::ok("(no lines in range)");
            }
            ToolResult::ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_agent_core::message::ToolOutput;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn blank_file_is_reported_as_empty() {
        let root = temp_dir("empty-read");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(root.join("empty.txt"), "\n")
            .await
            .unwrap();
        let ctx = ToolCtx {
            cwd: root.clone(),
            cancel: CancellationToken::new(),
        };

        let result = Read.run(json!({"path": "empty.txt"}), &ctx).await;

        assert!(!result.is_error);
        let ToolOutput::Text(output) = result.output;
        assert_eq!(output, "(empty file)");
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

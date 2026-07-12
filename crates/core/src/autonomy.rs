use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use serde_json::Value;

use crate::provider::BoxFuture;
use crate::tool::{Tool, ToolCtx, ToolDef, ToolEffect, ToolResult};

pub(crate) const UPDATE_GOAL_TOOL: &str = "update_goal";

const ACTIVE: u8 = 0;
const COMPLETE: u8 = 1;
const BLOCKED: u8 = 2;

#[derive(Clone, Default)]
pub(crate) struct GoalSignal(Arc<AtomicU8>);

impl GoalSignal {
    pub(crate) fn reset(&self) {
        self.0.store(ACTIVE, Ordering::Release);
    }

    pub(crate) fn outcome(&self) -> Option<GoalOutcome> {
        match self.0.load(Ordering::Acquire) {
            COMPLETE => Some(GoalOutcome::Complete),
            BLOCKED => Some(GoalOutcome::Blocked),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GoalOutcome {
    Complete,
    Blocked,
}

pub(crate) struct UpdateGoalTool {
    signal: GoalSignal,
}

impl UpdateGoalTool {
    pub(crate) fn new(signal: GoalSignal) -> Self {
        Self { signal }
    }
}

impl Tool for UpdateGoalTool {
    fn schema(&self) -> ToolDef {
        ToolDef {
            name: UPDATE_GOAL_TOOL.to_owned(),
            description: "Mark the active autonomous goal complete or blocked. Use `complete` only after verifying every requirement. Use `blocked` only when progress requires user input or an external change."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["complete", "blocked"]
                    }
                },
                "required": ["status"],
                "additionalProperties": false
            }),
        }
    }

    fn effect(&self) -> ToolEffect { ToolEffect::Read }

    fn summary(&self, _input: &Value) -> Option<String> {
        Some("update autonomous goal status".to_owned())
    }

    fn run<'a>(&'a self, input: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            match input.get("status").and_then(Value::as_str) {
                Some("complete") => {
                    self.signal.0.store(COMPLETE, Ordering::Release);
                    ToolResult::ok("goal marked complete")
                }
                Some("blocked") => {
                    self.signal.0.store(BLOCKED, Ordering::Release);
                    ToolResult::ok("goal marked blocked")
                }
                _ => ToolResult::error("status must be `complete` or `blocked`"),
            }
        })
    }
}

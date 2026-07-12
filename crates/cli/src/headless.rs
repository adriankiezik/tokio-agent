use std::io::Write;
use std::path::PathBuf;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_agent_core::agent::AgentError;
use tokio_agent_core::agent::{AgentEvent, UiCommand};
use tokio_agent_core::message::ToolOutput;

pub struct Printer {
    text_open: bool,
}

impl Printer {
    pub fn new() -> Self {
        Self { text_open: false }
    }

    pub async fn consume(
        &mut self,
        mut events: UnboundedReceiver<AgentEvent>,
        commands: &UnboundedSender<UiCommand>,
    ) -> Result<(), AgentError> {
        while let Some(event) = events.recv().await {
            let result = self.handle(event, commands);
            if let Some(result) = result {
                let _ = commands.send(UiCommand::Shutdown);
                return result.map(|_| ());
            }
        }
        Ok(())
    }

    fn handle(
        &mut self,
        event: AgentEvent,
        commands: &UnboundedSender<UiCommand>,
    ) -> Option<Result<tokio_agent_core::event::StopReason, AgentError>> {
        match event {
            AgentEvent::AutomaticTurnStarted(_) => {
                self.end_text();
                eprintln!("\x1b[2m[automatic turn]\x1b[0m");
            }
            AgentEvent::TextDelta(text) => {
                print!("{text}");
                let _ = std::io::stdout().flush();
                self.text_open = true;
            }
            AgentEvent::ThinkingDelta(text) => {
                self.end_text();
                eprintln!("\x1b[2m[thinking] {text}\x1b[0m");
            }
            AgentEvent::ToolStarted { name, summary, .. } => {
                self.end_text();
                if name == "web_search" {
                    println!("\x1b[36m● Searching the web\x1b[0m");
                } else {
                    println!("\x1b[36m● {name}\x1b[0m — {summary}");
                }
            }
            AgentEvent::ToolOutputDelta { text, .. } => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            AgentEvent::ToolFinished { name, result, .. } => {
                let ToolOutput::Text(text) = &result.output;
                if name == "web_search" {
                    let query = text.strip_prefix("searched: ").unwrap_or(text).trim();
                    if result.is_error {
                        println!("  ✗ Web search failed");
                    } else if query.is_empty() || query == "search completed" {
                        println!("  ✓ Searched the web");
                    } else {
                        println!("  ✓ Searched the web for {query}");
                    }
                } else {
                    let marker = if result.is_error { "✗" } else { "✓" };
                    let preview: String = text.lines().take(20).collect::<Vec<_>>().join("\n");
                    println!("  {marker} {preview}");
                }
            }
            AgentEvent::TurnUsage(usage) => {
                eprintln!(
                    "\x1b[2m[usage] in={} out={} cache_read={} cache_write={}\x1b[0m",
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_read_tokens,
                    usage.cache_write_tokens
                );
            }
            AgentEvent::RequestUsage(_)
            | AgentEvent::StatusSegments(_)
            | AgentEvent::CommandCatalog(_)
            | AgentEvent::ExtensionCatalog(_) => {}
            AgentEvent::CommandHandled(result) => {
                self.end_text();
                return Some(match result {
                    Ok(_) => Ok(tokio_agent_core::event::StopReason::EndTurn),
                    Err(error) => Err(AgentError::Command(error)),
                });
            }
            AgentEvent::InteractionRequested(request) => {
                self.end_text();
                eprintln!(
                    "\x1b[33m[denied] this extension interaction requires an interactive frontend\x1b[0m"
                );
                let _ = commands.send(UiCommand::RespondToInteraction(
                    tokio_agent_extension_api::InteractionResponse {
                        id: request.id,
                        owner: request.owner,
                        generation: request.generation,
                        action_id: "cancel".into(),
                    },
                ));
            }
            AgentEvent::InteractionCancelled { .. } => {}
            AgentEvent::TurnDone(result) => {
                self.end_text();
                return Some(result);
            }
        }
        None
    }

    fn end_text(&mut self) {
        if self.text_open {
            println!();
            self.text_open = false;
        }
    }
}

pub fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

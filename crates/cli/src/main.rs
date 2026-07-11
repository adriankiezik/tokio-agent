mod headless;
mod session;

use anyhow::Context;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "tokio-agent",
    version,
    about = "A fast, provider-agnostic terminal coding agent"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(
        short = 'p',
        long,
        value_name = "PROMPT",
        help = "Run a single turn non-interactively and print the result to stdout"
    )]
    non_interactive: Option<String>,

    #[arg(long)]
    debug: bool,

    #[arg(
        long,
        help = "Allow all tool actions without permission prompts (dangerous)"
    )]
    yolo: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Sign in to OpenAI with your ChatGPT subscription")]
    Login,
    #[command(about = "Sign out and remove stored ChatGPT credentials")]
    Logout,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.debug {
        tracing_subscriber::fmt()
            .with_env_filter("tokio_agent=debug,tokio_agent_core=debug,tokio_agent_provider=debug,tokio_agent_tools=debug,tokio_agent_tui=debug,tokio_agent_config=debug,tokio_agent_mcp=debug,tokio_agent_plugin=debug,tokio_agent_auth=debug")
            .init();
    }

    match cli.command {
        Some(Command::Login) => run_login(),
        Some(Command::Logout) => run_logout(),
        None => match cli.non_interactive {
            Some(prompt) => run_headless(prompt, cli.yolo),
            None => run_tui(cli.yolo),
        },
    }
}

fn run_login() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting tokio runtime")?;

    let outcome = runtime
        .block_on(tokio_agent_auth::login())
        .context("signing in with ChatGPT")?;

    match outcome.email {
        Some(email) => println!("Signed in as {email}."),
        None => println!("Signed in."),
    }
    println!("Set `provider = \"openai\"` and `auth = \"chatgpt\"` in your config to use it.");
    Ok(())
}

fn run_logout() -> anyhow::Result<()> {
    match tokio_agent_auth::logout().context("signing out")? {
        Some(path) => println!("Removed stored credentials at {}.", path.display()),
        None => println!("No stored credentials to remove."),
    }
    Ok(())
}

fn run_headless(prompt: String, yolo: bool) -> anyhow::Result<()> {
    let cwd = headless::cwd();
    let agent = session::build_session(&cwd, yolo)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting tokio runtime")?;

    runtime.block_on(async move {
        let (commands_tx, commands_rx) = tokio::sync::mpsc::unbounded_channel();
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let turn = tokio::spawn(agent.run(commands_rx, events_tx));
        commands_tx
            .send(tokio_agent_core::agent::UiCommand::UserMessage(prompt))
            .context("starting the turn")?;
        let mut printer = headless::Printer::new();
        printer
            .consume(events_rx, &commands_tx)
            .await
            .context("running the turn")?;
        turn.await.context("agent task panicked")?;
        Ok::<_, anyhow::Error>(())
    })?;

    Ok(())
}

fn run_tui(yolo: bool) -> anyhow::Result<()> {
    let cwd = headless::cwd();
    loop {
        let agent = match session::build_session(&cwd, yolo) {
            Ok(agent) => agent,
            Err(error) => {
                if tokio_agent_tui::configure_provider(&cwd).context("configuring a provider")? {
                    continue;
                }
                return Err(error);
            }
        };
        match tokio_agent_tui::run(agent).context("running the terminal UI")? {
            tokio_agent_tui::RunOutcome::Quit => return Ok(()),
            tokio_agent_tui::RunOutcome::ConfigureProvider => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yolo_flag_is_available_for_interactive_and_non_interactive_modes() {
        let interactive = Cli::try_parse_from(["tokio-agent", "--yolo"]).unwrap();
        assert!(interactive.yolo);

        let long =
            Cli::try_parse_from(["tokio-agent", "--yolo", "--non-interactive", "hello"]).unwrap();
        assert!(long.yolo);
        assert_eq!(long.non_interactive.as_deref(), Some("hello"));

        let short = Cli::try_parse_from(["tokio-agent", "-p", "hello"]).unwrap();
        assert_eq!(short.non_interactive.as_deref(), Some("hello"));
        assert!(Cli::try_parse_from(["tokio-agent", "--print", "hello"]).is_err());
    }
}

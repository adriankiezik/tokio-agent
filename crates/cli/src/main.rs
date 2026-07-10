use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "tokio-agent",
    version,
    about = "A fast, provider-agnostic terminal coding agent"
)]
struct Cli {
    #[arg(long)]
    debug: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.debug {
        tracing_subscriber::fmt()
            .with_env_filter("tokio_agent=debug,tokio_agent_core=debug,tokio_agent_provider=debug,tokio_agent_tools=debug,tokio_agent_tui=debug,tokio_agent_config=debug,tokio_agent_mcp=debug,tokio_agent_plugin=debug")
            .init();
    }

    println!(
        "tokio-agent {} — nothing here yet (M0 in progress)",
        env!("CARGO_PKG_VERSION")
    );
    Ok(())
}

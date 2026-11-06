use clap::{Parser, Subcommand};
use ensaio::cli::{CoreCommand, run_core};

#[derive(Parser)]
#[command(
    name = "ensaio",
    about = "Typed architectural-trial rig — compile, inspect, and reconcile scenarios"
)]
struct Cli {
    /// Kube context to act against. **Pass this.**
    #[arg(long, global = true)]
    context: Option<String>,

    /// Proceed even when the target does not look like an ephemeral-environment
    /// cluster. Required to act against anything unrecognised.
    #[arg(long, global = true)]
    allow_foreign_cluster: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(flatten)]
    Core(CoreCommand),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Core(core) => {
            run_core(core, cli.context.as_deref(), cli.allow_foreign_cluster).await
        }
    }
}

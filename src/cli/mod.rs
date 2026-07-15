use clap::{Parser, Subcommand};

/// account-cooker: spawn believable, long-lived Solana agents to defeat wallet clustering.
#[derive(Parser)]
#[command(name = "cooker", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Start the agent fleet using the given config file.
    Run {
        #[arg(short, long, default_value = "cooker.toml")]
        config: String,

        /// Override number of agents to spawn (defaults to config value).
        #[arg(short, long)]
        agents: Option<usize>,
    },
    /// Print current fleet status (balances, last actions, uptime).
    Status {
        #[arg(short, long, default_value = "cooker.toml")]
        config: String,
    },
    /// Validate a config file without running anything.
    Validate {
        #[arg(short, long, default_value = "cooker.toml")]
        config: String,
    },
}

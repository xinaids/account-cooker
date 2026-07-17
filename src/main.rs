mod cli;

use account_cooker::{config, scheduler};
use clap::Parser;
use cli::{Cli, Commands};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("account_cooker=info".parse()?))
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Run { config: config_path, agents } => {
            let cfg = config::CookerConfig::load(&config_path)?;
            tracing::info!("Loaded config with {} protocol(s) enabled", cfg.protocols.len());
            scheduler::run_fleet(cfg, agents).await?;
        }
        Commands::Status { config: config_path } => {
            let cfg = config::CookerConfig::load(&config_path)?;
            scheduler::print_status(&cfg).await?;
        }
        Commands::Validate { config: config_path } => {
            let cfg = config::CookerConfig::load(&config_path)?;
            cfg.validate()?;
            println!("Config is valid. {} protocol(s), {} wallet(s) available.", cfg.protocols.len(), cfg.wallets.len());
        }
    }

    Ok(())
}

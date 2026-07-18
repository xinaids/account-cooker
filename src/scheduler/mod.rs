use solana_client::nonblocking::rpc_client::RpcClient;
use std::sync::Arc;

use crate::agent::Agent;
use crate::config::CookerConfig;
use crate::protocols::ProtocolRegistry;

/// Spawns one independent async task per wallet, each running its own agent
/// loop with its own randomized timing. This is what lets the fleet scale to
/// thousands of agents: they share nothing but the RPC client and registry.
pub async fn run_fleet(cfg: CookerConfig, agent_override: Option<usize>) -> anyhow::Result<()> {
    cfg.validate()?;

    let rpc = Arc::new(RpcClient::new(cfg.rpc_url.clone()));
    let registry = Arc::new(ProtocolRegistry::from_config(&cfg.protocols)?);

    let wallet_count = agent_override
        .unwrap_or(cfg.agent_count)
        .min(cfg.wallets.len());
    tracing::info!(
        "starting fleet: {} agent(s), {} protocol(s), rpc={}",
        wallet_count,
        registry.len(),
        cfg.rpc_url
    );

    let mut handles = Vec::with_capacity(wallet_count);

    for wallet_cfg in cfg.wallets.iter().take(wallet_count) {
        let agent = Agent::from_config(wallet_cfg, cfg.timing.clone())?;
        let rpc = Arc::clone(&rpc);
        let registry = Arc::clone(&registry);

        handles.push(tokio::spawn(async move {
            agent.run_forever(rpc, registry).await;
        }));
    }

    // Fleet runs until interrupted (Ctrl+C) or the process is stopped externally.
    futures_wait_all(handles).await;
    Ok(())
}

async fn futures_wait_all(handles: Vec<tokio::task::JoinHandle<()>>) {
    for h in handles {
        let _ = h.await;
    }
}

/// Prints balances and basic reachability for every configured wallet without
/// starting any agent loops. Useful before a long run to sanity-check funding.
pub async fn print_status(cfg: &CookerConfig) -> anyhow::Result<()> {
    let rpc = RpcClient::new(cfg.rpc_url.clone());
    for w in &cfg.wallets {
        let kp = solana_sdk::signature::read_keypair_file(&w.keypair_path)
            .map_err(|e| anyhow::anyhow!("bad keypair {}: {e}", w.keypair_path))?;
        let balance = rpc
            .get_balance(&solana_sdk::signer::Signer::pubkey(&kp))
            .await?;
        println!(
            "{:<20} {:<44} {:>12.6} SOL",
            w.label.clone().unwrap_or_default(),
            solana_sdk::signer::Signer::pubkey(&kp),
            balance as f64 / 1_000_000_000.0
        );
    }
    Ok(())
}

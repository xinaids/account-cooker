use chrono::{Local, Timelike};
use rand::Rng;
use rand_distr::{Distribution, LogNormal};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::{read_keypair_file, Keypair, Signer};
use std::sync::Arc;
use std::time::Duration;

use crate::config::{TimingConfig, WalletConfig};
use crate::protocols::ProtocolRegistry;

pub struct Agent {
    pub label: String,
    pub wallet: Keypair,
    pub timing: TimingConfig,
}

impl Agent {
    pub fn from_config(wallet_cfg: &WalletConfig, timing: TimingConfig) -> anyhow::Result<Self> {
        let wallet = read_keypair_file(&wallet_cfg.keypair_path)
            .map_err(|e| anyhow::anyhow!("failed to load keypair {}: {e}", wallet_cfg.keypair_path))?;
        let label = wallet_cfg
            .label
            .clone()
            .unwrap_or_else(|| wallet.pubkey().to_string()[..8].to_string());
        Ok(Self { label, wallet, timing })
    }

    /// Runs this agent forever: sleeps a human-like, non-deterministic interval,
    /// checks whether it's inside its active hours and hasn't decided to skip
    /// the day, then fires one weighted-random protocol interaction.
    pub async fn run_forever(
        self,
        rpc: Arc<RpcClient>,
        registry: Arc<ProtocolRegistry>,
    ) {
        let mut skip_today = rand::thread_rng().gen_bool(self.timing.skip_day_probability);
        let mut current_day = Local::now().date_naive();

        loop {
            let now = Local::now();
            if now.date_naive() != current_day {
                current_day = now.date_naive();
                skip_today = rand::thread_rng().gen_bool(self.timing.skip_day_probability);
                if skip_today {
                    tracing::info!("[{}] sitting out today (simulated absence)", self.label);
                }
            }

            let hour = now.hour() as u8;
            let in_active_window =
                hour >= self.timing.active_hours[0] && hour < self.timing.active_hours[1];

            if skip_today || !in_active_window {
                // Check back at a fraction of the mean interval rather than a
                // hardcoded constant, so faster-cadence configs (e.g. dust-mode
                // agents) don't wait an unrelated fixed period.
                let recheck_secs = (self.timing.mean_interval_minutes * 60.0 / 4.0)
                    .clamp(30.0, 600.0) as u64;
                tokio::time::sleep(Duration::from_secs(recheck_secs)).await;
                continue;
            }

            match registry_action(&self, &rpc, &registry).await {
                Ok(sig) => tracing::info!("[{}] action ok, sig={}", self.label, sig),
                Err(e) => tracing::warn!("[{}] action skipped/failed: {e}", self.label),
            }

            let sleep_secs = self.next_interval_secs();
            tracing::debug!("[{}] sleeping {}s until next action", self.label, sleep_secs);
            tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
        }
    }

    /// Draws the next wait time from a log-normal distribution: mostly clusters
    /// around the mean but occasionally produces long gaps or quick bursts —
    /// exactly the irregularity that separates humans from cron jobs.
    fn next_interval_secs(&self) -> u64 {
        let mean_secs = (self.timing.mean_interval_minutes * 60.0).max(1.0);
        let std_secs = (self.timing.stddev_interval_minutes * 60.0).max(1.0);

        // Convert desired arithmetic mean/std into log-normal mu/sigma params.
        let variance = std_secs.powi(2);
        let mu = (mean_secs.powi(2) / (mean_secs.powi(2) + variance).sqrt()).ln();
        let sigma = ((variance / mean_secs.powi(2)) + 1.0).ln().sqrt();

        let dist = LogNormal::new(mu, sigma).unwrap_or(LogNormal::new(mean_secs.ln(), 0.5).unwrap());
        let draw = dist.sample(&mut rand::thread_rng());
        draw.clamp(30.0, 60.0 * 60.0 * 12.0) as u64
    }
}

async fn registry_action(
    agent: &Agent,
    rpc: &RpcClient,
    registry: &ProtocolRegistry,
) -> anyhow::Result<solana_sdk::signature::Signature> {
    let protocol = registry.pick();
    tracing::debug!("[{}] chose protocol: {}", agent.label, protocol.name());
    protocol.execute(rpc, &agent.wallet).await
}

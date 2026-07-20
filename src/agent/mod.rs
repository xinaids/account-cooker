use chrono::{Local, Timelike};
use rand::Rng;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::{read_keypair_file, Keypair, Signer};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{TimingConfig, WalletConfig};
use crate::protocols::ProtocolRegistry;
use crate::state::Checkpoint;

/// Directory where per-agent crash-recovery checkpoints are written. Kept
/// out of git (see `.gitignore`) since it's local runtime state, not config.
const STATE_DIR: &str = ".cooker_state";

pub struct Agent {
    pub label: String,
    pub wallet: Keypair,
    pub timing: TimingConfig,
}

impl Agent {
    pub fn from_config(wallet_cfg: &WalletConfig, timing: TimingConfig) -> anyhow::Result<Self> {
        let wallet = read_keypair_file(&wallet_cfg.keypair_path).map_err(|e| {
            anyhow::anyhow!("failed to load keypair {}: {e}", wallet_cfg.keypair_path)
        })?;
        let label = wallet_cfg
            .label
            .clone()
            .unwrap_or_else(|| wallet.pubkey().to_string()[..8].to_string());
        Ok(Self {
            label,
            wallet,
            timing,
        })
    }

    /// Runs this agent forever: sleeps a human-like, non-deterministic interval,
    /// checks whether it's inside its active hours and hasn't decided to skip
    /// the day, then fires one weighted-random protocol interaction.
    pub async fn run_forever(self, rpc: Arc<RpcClient>, registry: Arc<ProtocolRegistry>) {
        let mut skip_today = rand::thread_rng().gen_bool(self.timing.skip_day_probability);
        let mut current_day = Local::now().date_naive();
        let state_dir = PathBuf::from(STATE_DIR);
        let mut action_count = 0u64;

        // Resume from a crash instead of acting immediately: if a checkpoint
        // exists and it isn't due yet, wait out the remainder rather than
        // firing an action right after restart (which would be a burst tell)
        // or restarting a full fresh interval (wasteful but not incorrect).
        if let Some(cp) = Checkpoint::load(&state_dir, &self.label) {
            action_count = cp.action_count;
            let now_unix = chrono::Utc::now().timestamp();
            let remaining = cp.next_action_due_unix - now_unix;
            if remaining > 0 {
                tracing::info!(
                    "[{}] resuming from checkpoint (action_count={}), waiting {}s remaining",
                    self.label,
                    action_count,
                    remaining
                );
                tokio::time::sleep(Duration::from_secs(remaining as u64)).await;
            } else {
                tracing::info!(
                    "[{}] resuming from checkpoint (action_count={}), overdue by {}s, acting now",
                    self.label,
                    action_count,
                    -remaining
                );
            }
        }

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
                let recheck_secs =
                    (self.timing.mean_interval_minutes * 60.0 / 4.0).clamp(30.0, 600.0) as u64;
                tokio::time::sleep(Duration::from_secs(recheck_secs)).await;
                continue;
            }

            let mut last_sig = None;
            match registry_action(&self, &rpc, &registry).await {
                Ok(sig) => {
                    tracing::info!("[{}] action ok, sig={}", self.label, sig);
                    last_sig = Some(sig.to_string());
                    action_count += 1;
                }
                Err(e) => tracing::warn!("[{}] action skipped/failed: {e}", self.label),
            }

            let sleep_secs = crate::timing::sample_interval_secs(
                self.timing.mean_interval_minutes,
                self.timing.stddev_interval_minutes,
                &mut rand::thread_rng(),
            );

            // Checkpoint before the long sleep, not after: this is the
            // window where a crash needs to be recoverable. On restart the
            // resume logic above reads this same file.
            let cp = Checkpoint {
                last_action_unix: chrono::Utc::now().timestamp(),
                last_sig,
                next_action_due_unix: chrono::Utc::now().timestamp() + sleep_secs as i64,
                action_count,
            };
            if let Err(e) = cp.save(&state_dir, &self.label) {
                tracing::warn!("[{}] failed to write recovery checkpoint: {e}", self.label);
            }

            tracing::debug!(
                "[{}] sleeping {}s until next action",
                self.label,
                sleep_secs
            );
            tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
        }
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

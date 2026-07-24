use chrono::{Local, Timelike};
use rand::Rng;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::{read_keypair_file, Keypair, Signer};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::config::{PersonaJitterConfig, ProtocolConfig, TimingConfig, WalletConfig};
use crate::persona;
use crate::protocols::ProtocolRegistry;
use crate::state::Checkpoint;

/// Directory where per-agent crash-recovery checkpoints are written. Kept
/// out of git (see `.gitignore`) since it's local runtime state, not config.
const STATE_DIR: &str = ".cooker_state";

pub struct Agent {
    pub label: String,
    pub wallet: Keypair,
    pub timing: TimingConfig,
    /// This agent's OWN active-hours window, in minutes-since-midnight —
    /// the operator's `timing.active_hours` plus a small deterministic
    /// per-agent offset (see `persona::jittered_active_hours_minutes` and
    /// `config::PersonaJitterConfig`). Not necessarily identical to
    /// `timing.active_hours` converted to minutes.
    pub active_start_minutes: u32,
    pub active_end_minutes: u32,
    /// This agent's OWN protocol registry, built from the operator's base
    /// `[[protocols]]` weights plus a small deterministic per-agent
    /// perturbation (see `persona::jittered_protocol_weights`). Every agent
    /// gets its own registry instance now rather than a fleet-shared one,
    /// since the point of the fix is that the weights themselves differ
    /// slightly per agent.
    pub registry: ProtocolRegistry,
    /// This agent's OWN daily skip probability — the operator's
    /// `timing.skip_day_probability` plus a small deterministic per-agent
    /// multiplicative perturbation (see
    /// `persona::jittered_skip_day_probability`). Closes the residual
    /// clustering signal named in README.md's "3d.": `actions_per_day`
    /// stayed separable across the active-hours/protocol-weight jitter
    /// sweep specifically because every agent in a fleet still shared this
    /// one value exactly.
    pub skip_day_probability: f64,
}

impl Agent {
    pub fn from_config(
        wallet_cfg: &WalletConfig,
        timing: TimingConfig,
        protocols: &[ProtocolConfig],
        jitter: &PersonaJitterConfig,
    ) -> anyhow::Result<Self> {
        let wallet = read_keypair_file(&wallet_cfg.keypair_path).map_err(|e| {
            anyhow::anyhow!("failed to load keypair {}: {e}", wallet_cfg.keypair_path)
        })?;
        let label = wallet_cfg
            .label
            .clone()
            .unwrap_or_else(|| wallet.pubkey().to_string()[..8].to_string());

        // Public key only (not the full keypair): there's no secrecy
        // property to protect here, only reproducible-but-uncorrelated
        // behavioral variation. See `src/persona.rs` module docs.
        let identity_bytes = wallet.pubkey().to_bytes();

        let (active_start_minutes, active_end_minutes) = persona::jittered_active_hours_minutes(
            timing.active_hours,
            jitter.active_hours_minutes,
            &identity_bytes,
        );

        let jittered_weights = persona::jittered_protocol_weights(
            protocols,
            jitter.protocol_weight_fraction,
            &identity_bytes,
        );
        let agent_protocols: Vec<ProtocolConfig> = protocols
            .iter()
            .zip(jittered_weights)
            .map(|(p, weight)| ProtocolConfig {
                name: p.name.clone(),
                weight,
                params: p.params.clone(),
            })
            .collect();
        let registry = ProtocolRegistry::from_config(&agent_protocols)?;

        let skip_day_probability = persona::jittered_skip_day_probability(
            timing.skip_day_probability,
            jitter.skip_day_probability_fraction,
            &identity_bytes,
        );

        Ok(Self {
            label,
            wallet,
            timing,
            active_start_minutes,
            active_end_minutes,
            registry,
            skip_day_probability,
        })
    }

    /// Runs this agent forever: sleeps a human-like, non-deterministic interval,
    /// checks whether it's inside its active hours and hasn't decided to skip
    /// the day, then fires one weighted-random protocol interaction.
    pub async fn run_forever(self, rpc: Arc<RpcClient>) {
        let mut skip_today = rand::thread_rng().gen_bool(self.skip_day_probability);
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
                skip_today = rand::thread_rng().gen_bool(self.skip_day_probability);
                if skip_today {
                    tracing::info!("[{}] sitting out today (simulated absence)", self.label);
                }
            }

            let minute_of_day = now.hour() * 60 + now.minute();
            let in_active_window = minute_of_day >= self.active_start_minutes
                && minute_of_day < self.active_end_minutes;

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
            match registry_action(&self, &rpc).await {
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
) -> anyhow::Result<solana_sdk::signature::Signature> {
    let protocol = agent.registry.pick();
    tracing::debug!("[{}] chose protocol: {}", agent.label, protocol.name());
    protocol.execute(rpc, &agent.wallet).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Writes a fresh in-memory keypair to `dir/name` in the same plain
    /// JSON-array-of-bytes format `solana-keygen new` / `read_keypair_file`
    /// use, and returns its path.
    fn write_keypair(dir: &std::path::Path, name: &str) -> PathBuf {
        let kp = Keypair::new();
        let path = dir.join(name);
        let bytes = kp.to_bytes().to_vec();
        std::fs::write(&path, serde_json::to_string(&bytes).unwrap()).unwrap();
        path
    }

    fn sample_timing() -> TimingConfig {
        TimingConfig {
            mean_interval_minutes: 45.0,
            stddev_interval_minutes: 30.0,
            active_hours: [8, 23],
            skip_day_probability: 0.15,
        }
    }

    fn sample_protocols() -> Vec<ProtocolConfig> {
        vec![
            ProtocolConfig {
                name: "jupiter_swap".to_string(),
                weight: 3.0,
                params: toml::Table::new(),
            },
            ProtocolConfig {
                name: "marinade_stake".to_string(),
                weight: 1.0,
                params: toml::Table::new(),
            },
        ]
    }

    fn wallet_config(path: &std::path::Path) -> WalletConfig {
        WalletConfig {
            keypair_path: path.to_str().unwrap().to_string(),
            label: None,
        }
    }

    #[test]
    fn two_agents_same_operator_get_different_jittered_active_hours() {
        let dir = tempdir().unwrap();
        let path_a = write_keypair(dir.path(), "a.json");
        let path_b = write_keypair(dir.path(), "b.json");

        let jitter = PersonaJitterConfig::default();
        let agent_a = Agent::from_config(
            &wallet_config(&path_a),
            sample_timing(),
            &sample_protocols(),
            &jitter,
        )
        .unwrap();
        let agent_b = Agent::from_config(
            &wallet_config(&path_b),
            sample_timing(),
            &sample_protocols(),
            &jitter,
        )
        .unwrap();

        assert_ne!(
            (agent_a.active_start_minutes, agent_a.active_end_minutes),
            (agent_b.active_start_minutes, agent_b.active_end_minutes),
            "two different wallets under the same operator config should get \
             slightly different persona jitter, not share the exact same window"
        );

        let bound = jitter.active_hours_minutes.round() as i64;
        for agent in [&agent_a, &agent_b] {
            assert!((agent.active_start_minutes as i64 - 8 * 60).abs() <= bound);
            assert!((agent.active_end_minutes as i64 - 23 * 60).abs() <= bound);
        }
    }

    #[test]
    fn two_agents_same_operator_get_different_jittered_skip_day_probability() {
        let dir = tempdir().unwrap();
        let path_a = write_keypair(dir.path(), "a.json");
        let path_b = write_keypair(dir.path(), "b.json");

        let jitter = PersonaJitterConfig::default();
        let agent_a = Agent::from_config(
            &wallet_config(&path_a),
            sample_timing(),
            &sample_protocols(),
            &jitter,
        )
        .unwrap();
        let agent_b = Agent::from_config(
            &wallet_config(&path_b),
            sample_timing(),
            &sample_protocols(),
            &jitter,
        )
        .unwrap();

        assert_ne!(
            agent_a.skip_day_probability, agent_b.skip_day_probability,
            "two different wallets under the same operator config should get \
             slightly different skip-day jitter, not share the exact same probability"
        );

        let base = sample_timing().skip_day_probability;
        let fraction = jitter.skip_day_probability_fraction;
        for agent in [&agent_a, &agent_b] {
            assert!((0.0..=1.0).contains(&agent.skip_day_probability));
            let lo = base * (1.0 - fraction) - 1e-9;
            let hi = base * (1.0 + fraction) + 1e-9;
            assert!((lo..=hi).contains(&agent.skip_day_probability));
        }
    }

    #[test]
    fn zero_jitter_config_reproduces_operator_active_hours_exactly() {
        let dir = tempdir().unwrap();
        let path_a = write_keypair(dir.path(), "a.json");

        let jitter = PersonaJitterConfig {
            active_hours_minutes: 0.0,
            protocol_weight_fraction: 0.0,
            skip_day_probability_fraction: 0.0,
        };
        let agent = Agent::from_config(
            &wallet_config(&path_a),
            sample_timing(),
            &sample_protocols(),
            &jitter,
        )
        .unwrap();

        assert_eq!(agent.active_start_minutes, 8 * 60);
        assert_eq!(agent.active_end_minutes, 23 * 60);
        assert_eq!(
            agent.skip_day_probability,
            sample_timing().skip_day_probability
        );
    }

    #[test]
    fn same_wallet_gives_identical_jitter_across_separate_constructions() {
        let dir = tempdir().unwrap();
        let path_a = write_keypair(dir.path(), "a.json");
        let jitter = PersonaJitterConfig::default();

        let agent_1 = Agent::from_config(
            &wallet_config(&path_a),
            sample_timing(),
            &sample_protocols(),
            &jitter,
        )
        .unwrap();
        let agent_2 = Agent::from_config(
            &wallet_config(&path_a),
            sample_timing(),
            &sample_protocols(),
            &jitter,
        )
        .unwrap();

        assert_eq!(agent_1.active_start_minutes, agent_2.active_start_minutes);
        assert_eq!(agent_1.active_end_minutes, agent_2.active_end_minutes);
        assert_eq!(agent_1.skip_day_probability, agent_2.skip_day_probability);
    }
}

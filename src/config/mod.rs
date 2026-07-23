use serde::Deserialize;
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct CookerConfig {
    pub rpc_url: String,
    #[serde(default = "default_agent_count")]
    pub agent_count: usize,
    pub wallets: Vec<WalletConfig>,
    pub timing: TimingConfig,
    pub protocols: Vec<ProtocolConfig>,
    #[serde(default)]
    pub consolidation: ConsolidationConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct WalletConfig {
    /// Path to a keypair JSON file (same format as `solana-keygen`).
    pub keypair_path: String,
    /// Optional human label, used only in logs.
    pub label: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TimingConfig {
    /// Mean minutes between actions for a single agent.
    pub mean_interval_minutes: f64,
    /// Standard deviation (log-normal), controls burstiness vs regularity.
    pub stddev_interval_minutes: f64,
    /// Active hours window in local-persona time, e.g. [8, 23].
    pub active_hours: [u8; 2],
    /// Probability [0,1] an agent skips an entire day (simulates real absence).
    #[serde(default = "default_skip_day_prob")]
    pub skip_day_probability: f64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ProtocolConfig {
    pub name: String,
    /// Relative weight for random protocol selection (higher = more frequent).
    #[serde(default = "default_weight")]
    pub weight: f64,
    #[serde(default)]
    pub params: toml::Table,
}

/// Periodic fund consolidation/redistribution across one operator's own
/// fleet (`src/consolidation.rs`). Opt-in: `enabled` defaults to `false`,
/// matching the existing precedent for new, less-battle-tested behavior
/// (`supersonic_cast`'s `weight = 0.0` default in `cooker.example.toml`) —
/// the conservative choice for a feature that moves real funds.
#[derive(Debug, Deserialize, Clone)]
pub struct ConsolidationConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Mean hours between consolidation transfers — deliberately a much
    /// longer cadence than noise-transaction timing, since "periodically
    /// consolidates" describes fleet-housekeeping, not a per-tick action.
    #[serde(default = "default_consolidation_mean_hours")]
    pub mean_interval_hours: f64,
    /// Standard deviation (log-normal, via the same `timing::sample_interval_secs`
    /// the noise scheduler uses) — controls how irregular the consolidation
    /// cadence itself is, so it isn't a predictable fixed-period tell.
    #[serde(default = "default_consolidation_stddev_hours")]
    pub stddev_interval_hours: f64,
    /// Minimum fraction of the source wallet's spendable balance moved per
    /// transfer.
    #[serde(default = "default_fraction_min")]
    pub fraction_min: f64,
    /// Maximum fraction of the source wallet's spendable balance moved per
    /// transfer.
    #[serde(default = "default_fraction_max")]
    pub fraction_max: f64,
    /// A wallet needs at least this many lamports to be eligible as a
    /// consolidation source this tick.
    #[serde(default = "default_min_balance_lamports")]
    pub min_balance_lamports: u64,
    /// Always left behind in the source wallet after a transfer, so it can
    /// keep paying fees/rent for ongoing noise-transaction activity.
    #[serde(default = "default_reserve_lamports")]
    pub reserve_lamports: u64,
}

impl Default for ConsolidationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mean_interval_hours: default_consolidation_mean_hours(),
            stddev_interval_hours: default_consolidation_stddev_hours(),
            fraction_min: default_fraction_min(),
            fraction_max: default_fraction_max(),
            min_balance_lamports: default_min_balance_lamports(),
            reserve_lamports: default_reserve_lamports(),
        }
    }
}

fn default_agent_count() -> usize {
    5
}
fn default_skip_day_prob() -> f64 {
    0.15
}
fn default_weight() -> f64 {
    1.0
}
fn default_consolidation_mean_hours() -> f64 {
    72.0
}
fn default_consolidation_stddev_hours() -> f64 {
    48.0
}
fn default_fraction_min() -> f64 {
    0.05
}
fn default_fraction_max() -> f64 {
    0.20
}
fn default_min_balance_lamports() -> u64 {
    10_000_000
}
fn default_reserve_lamports() -> u64 {
    5_000_000
}

impl CookerConfig {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let raw = fs::read_to_string(Path::new(path))
            .map_err(|e| anyhow::anyhow!("failed to read config at {path}: {e}"))?;
        let cfg: CookerConfig = toml::from_str(&raw)?;
        Ok(cfg)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.wallets.is_empty() {
            anyhow::bail!("config must define at least one wallet");
        }
        if self.protocols.is_empty() {
            anyhow::bail!("config must enable at least one protocol");
        }
        if self.timing.active_hours[0] >= self.timing.active_hours[1] {
            anyhow::bail!("active_hours must be [start, end) with start < end");
        }
        for w in &self.wallets {
            if !Path::new(&w.keypair_path).exists() {
                anyhow::bail!("keypair file not found: {}", w.keypair_path);
            }
        }
        if self.consolidation.enabled {
            if self.wallets.len() < 2 {
                anyhow::bail!(
                    "consolidation.enabled requires at least 2 wallets to move funds between"
                );
            }
            if !(0.0..=1.0).contains(&self.consolidation.fraction_min)
                || !(0.0..=1.0).contains(&self.consolidation.fraction_max)
            {
                anyhow::bail!("consolidation.fraction_min/fraction_max must be within [0.0, 1.0]");
            }
            if self.consolidation.fraction_min > self.consolidation.fraction_max {
                anyhow::bail!("consolidation.fraction_min must be <= fraction_max");
            }
        }
        Ok(())
    }
}

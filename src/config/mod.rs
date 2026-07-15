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

fn default_agent_count() -> usize { 5 }
fn default_skip_day_prob() -> f64 { 0.15 }
fn default_weight() -> f64 { 1.0 }

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
        Ok(())
    }
}

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
    #[serde(default)]
    pub persona_jitter: PersonaJitterConfig,
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

/// Per-agent persona jitter: nudges each agent's derived `active_hours`
/// window and protocol weights independently around the operator's base
/// `[timing]` / `[[protocols]]` persona, instead of every wallet in a fleet
/// sharing those values byte-for-byte. Implements the recommendation named
/// (but not implemented) in `THREAT_MODEL.md`'s "Multi-wallet clustering"
/// section: `clustering_harness` found persona-sharing, not timing spread,
/// is the dominant signal for grouping a fleet's wallets back to one
/// operator (ARI 0.42 vs 0.08 for a naive tight-timing baseline).
///
/// Each agent's own offset is derived deterministically from its own wallet
/// pubkey (`src/persona.rs`), never a shared/global RNG — see
/// THREAT_MODEL.md's "No shared entropy across agents" defense, which this
/// extends rather than weakens: reproducible across restarts, and no two
/// agents' jitter can be linked through a common random stream.
///
/// Unlike `ConsolidationConfig` (opt-in, `enabled = false`, because it moves
/// real funds) or `supersonic_cast` (opt-in, `weight = 0.0`, because it's a
/// new external dependency), this feature is pure config-derivation math —
/// no funds movement, no third-party code path — and the vulnerability it
/// fixes is otherwise present in `cooker.example.toml`'s defaults out of the
/// box. So it ships ACTIVE by default, at a small, conservative magnitude
/// (30 minutes / 15%). An operator who wants the pre-fix
/// byte-identical-fleet-persona behavior back sets both fields to `0.0`. A
/// `cooker.toml` predating this block parses fine and gets these same
/// defaults (see `Default` impl below) — never a hard parse error.
#[derive(Debug, Deserialize, Clone)]
pub struct PersonaJitterConfig {
    /// Max minutes each agent's active_hours window is shifted from the
    /// operator's configured `[timing].active_hours`. ONE random offset in
    /// `[-N, N]` is drawn per agent and applied to BOTH boundaries equally,
    /// so window WIDTH (how many hours/day the agent is "awake") is
    /// preserved — only where in the day it sits shifts, modeling
    /// early-bird/night-owl variation among an operator's agents. `0.0`
    /// disables active-hours jitter entirely.
    #[serde(default = "default_jitter_active_hours_minutes")]
    pub active_hours_minutes: f64,
    /// Max fractional perturbation applied independently to each
    /// protocol's own weight, e.g. `0.15` = each agent's weight for a given
    /// protocol is the operator's configured weight times a factor drawn
    /// independently from `[0.85, 1.15]`. A protocol disabled via
    /// `weight = 0.0` (e.g. `supersonic_cast`'s default) always stays
    /// exactly `0.0` — a multiplicative perturbation of zero is zero
    /// regardless of factor. Weights are NOT rescaled back to the
    /// operator's exact sum afterward: `ProtocolRegistry::pick_with_rng`
    /// already normalizes by the current total on every draw, so a global
    /// rescale would be purely cosmetic (and would silently cancel all
    /// jitter for a single-protocol registry) — see `src/persona.rs` for
    /// the full reasoning. `0.0` disables protocol-weight jitter entirely.
    #[serde(default = "default_jitter_protocol_weight_fraction")]
    pub protocol_weight_fraction: f64,
}

impl Default for PersonaJitterConfig {
    fn default() -> Self {
        Self {
            active_hours_minutes: default_jitter_active_hours_minutes(),
            protocol_weight_fraction: default_jitter_protocol_weight_fraction(),
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
fn default_jitter_active_hours_minutes() -> f64 {
    30.0
}
fn default_jitter_protocol_weight_fraction() -> f64 {
    0.15
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
        if self.persona_jitter.active_hours_minutes < 0.0 {
            anyhow::bail!("persona_jitter.active_hours_minutes must be >= 0.0");
        }
        if !(0.0..=1.0).contains(&self.persona_jitter.protocol_weight_fraction) {
            anyhow::bail!("persona_jitter.protocol_weight_fraction must be within [0.0, 1.0]");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Backward-compatibility regression test: a `cooker.toml` written
    /// before `[persona_jitter]` existed (no such block at all) must still
    /// parse successfully, and get the documented conservative defaults —
    /// never a hard parse error. This is the literal shape of the repo's
    /// own (gitignored) `cooker.toml`, which predates this field.
    #[test]
    fn config_without_persona_jitter_block_parses_with_defaults() {
        let toml_str = r#"
            rpc_url = "https://api.devnet.solana.com"
            agent_count = 3

            [timing]
            mean_interval_minutes = 45.0
            stddev_interval_minutes = 30.0
            active_hours = [8, 23]
            skip_day_probability = 0.15

            [[wallets]]
            keypair_path = "wallets/agent-01.json"
            label = "agent-01"

            [[protocols]]
            name = "jupiter_swap"
            weight = 3.0
        "#;

        let cfg: CookerConfig =
            toml::from_str(toml_str).expect("must parse without persona_jitter");
        assert_eq!(cfg.persona_jitter.active_hours_minutes, 30.0);
        assert_eq!(cfg.persona_jitter.protocol_weight_fraction, 0.15);
    }

    #[test]
    fn config_can_explicitly_zero_out_persona_jitter() {
        let toml_str = r#"
            rpc_url = "https://api.devnet.solana.com"

            [timing]
            mean_interval_minutes = 45.0
            stddev_interval_minutes = 30.0
            active_hours = [8, 23]

            [[wallets]]
            keypair_path = "wallets/agent-01.json"

            [[protocols]]
            name = "jupiter_swap"

            [persona_jitter]
            active_hours_minutes = 0.0
            protocol_weight_fraction = 0.0
        "#;

        let cfg: CookerConfig = toml::from_str(toml_str).expect("must parse explicit zero jitter");
        assert_eq!(cfg.persona_jitter.active_hours_minutes, 0.0);
        assert_eq!(cfg.persona_jitter.protocol_weight_fraction, 0.0);
    }

    #[test]
    fn validate_rejects_out_of_range_persona_jitter_fraction() {
        let mut cfg = CookerConfig {
            rpc_url: "https://api.devnet.solana.com".to_string(),
            agent_count: 1,
            wallets: vec![WalletConfig {
                keypair_path: "Cargo.toml".to_string(), // any file that exists
                label: None,
            }],
            timing: TimingConfig {
                mean_interval_minutes: 45.0,
                stddev_interval_minutes: 30.0,
                active_hours: [8, 23],
                skip_day_probability: 0.15,
            },
            protocols: vec![ProtocolConfig {
                name: "jupiter_swap".to_string(),
                weight: 1.0,
                params: toml::Table::new(),
            }],
            consolidation: ConsolidationConfig::default(),
            persona_jitter: PersonaJitterConfig::default(),
        };
        assert!(cfg.validate().is_ok());

        cfg.persona_jitter.protocol_weight_fraction = 1.5;
        assert!(cfg.validate().is_err());

        cfg.persona_jitter.protocol_weight_fraction = 0.15;
        cfg.persona_jitter.active_hours_minutes = -1.0;
        assert!(cfg.validate().is_err());
    }
}

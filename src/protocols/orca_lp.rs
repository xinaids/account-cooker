use async_trait::async_trait;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::{Keypair, Signature};

use super::Protocol;

/// Orca Whirlpools LP interaction (open/close small concentrated-liquidity positions).
///
/// TODO: extension-point skeleton. Real implementation should:
///   1. Use `orca_whirlpools` Rust SDK to open a narrow-range position with a
///      small, randomized amount of two noise-mint tokens.
///   2. Occasionally close positions after a random hold period (hours to days)
///      to mimic a real yield-seeking user rather than a bot with fixed cadence.
///   3. Reuse the mint list from `jupiter.rs` (`NOISE_MINTS`) for consistency —
///      an agent that provides LP on the same pairs it swaps looks organic.
pub struct OrcaLp {
    position_size_lamports_max: u64,
}

impl OrcaLp {
    pub fn from_params(params: &toml::Table) -> anyhow::Result<Self> {
        let position_size_lamports_max = params
            .get("position_size_lamports_max")
            .and_then(|v| v.as_integer())
            .unwrap_or(50_000_000) as u64;
        Ok(Self { position_size_lamports_max })
    }
}

#[async_trait]
impl Protocol for OrcaLp {
    fn name(&self) -> &str {
        "orca_lp"
    }

    async fn execute(
        &self,
        _rpc: &RpcClient,
        _wallet: &Keypair,
    ) -> anyhow::Result<Signature> {
        anyhow::bail!(
            "orca_lp not yet implemented (max_position={}) — see TODO in protocols/orca_lp.rs",
            self.position_size_lamports_max
        )
    }
}

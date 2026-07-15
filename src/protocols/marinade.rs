use async_trait::async_trait;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::{Keypair, Signature};

use super::Protocol;

/// Marinade liquid staking interaction (stake small SOL amounts periodically).
///
/// TODO: this is the extension-point skeleton. Real implementation should:
///   1. Build the `deposit` instruction against the Marinade State account
///      (program id: MarBmsSgKXdrN1egZf5sqe1TMai9K1rChYNDJgjq7aD).
///   2. Use the Marinade Rust SDK or raw Anchor IDL bindings.
///   3. Vary stake amounts + intervals the same way JupiterSwap varies swaps,
///      so the on-chain footprint doesn't look scripted.
pub struct MarinadeStake {
    max_balance_fraction: f64,
}

impl MarinadeStake {
    pub fn from_params(params: &toml::Table) -> anyhow::Result<Self> {
        let max_balance_fraction = params
            .get("max_balance_fraction")
            .and_then(|v| v.as_float())
            .unwrap_or(0.02);
        Ok(Self { max_balance_fraction })
    }
}

#[async_trait]
impl Protocol for MarinadeStake {
    fn name(&self) -> &str {
        "marinade_stake"
    }

    async fn execute(
        &self,
        _rpc: &RpcClient,
        _wallet: &Keypair,
    ) -> anyhow::Result<Signature> {
        anyhow::bail!(
            "marinade_stake not yet implemented (fraction={}) — see TODO in protocols/marinade.rs",
            self.max_balance_fraction
        )
    }
}

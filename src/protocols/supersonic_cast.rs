use async_trait::async_trait;
use rand::Rng;
use sha2::{Digest, Sha256};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer},
    transaction::Transaction,
};
use std::str::FromStr;
use supersonic_sdk::{build_instruction, plan_bundle, DecoyConfig};

use super::Protocol;

/// The `supersonic-tx` router program id deployed and live on devnet. Provenance:
/// `solanabr/supersonic-tx` PR #1 (Jmkoygg, branch `feat/intent-ambiguous-router`),
/// `programs/supersonic-tx/src/lib.rs` `declare_id!`. This protocol does not
/// reimplement the router — it composes with it via `supersonic-sdk`'s public API
/// (see ../../../COMPOSABILITY.md for the pinned dependency and a real proof tx).
const DEFAULT_ROUTER_PROGRAM_ID: &str = "BCrR3JKi5EWhC5DuKYzV4EX7ogawoWaoKkhSqZYeYabn";

const MASTER_TAG: &[u8] = b"account-cooker/supersonic-cast/master/v1";

/// Casts the agent's own noise transfers through the `supersonic-tx` router instead
/// of a plain `SystemProgram::transfer`, so a single self-transfer looks like an
/// intent-ambiguous K-leg bundle on-chain. This is a real third-party integration —
/// account-cooker never touches the router's source, only its public SDK surface
/// (`plan_bundle` + `build_instruction`).
///
/// The "real" leg of each bundle is a self-transfer back to the agent's own wallet
/// (a distinct destination is required — the router rejects self-destination legs,
/// see `SupersonicError::SelfDestination`), so this protocol adds bundle-shaped
/// noise without moving custody anywhere outside the agent.
pub struct SupersonicCast {
    /// Fraction of the wallet's SOL balance a single bundle's real leg is allowed
    /// to use.
    max_balance_fraction: f64,
    /// Minimum lamports required to attempt a cast; below this the agent skips
    /// the tick rather than sending a dust-sized, fee-losing tx.
    min_cast_lamports: u64,
    /// Anonymity set size (total legs including the real one), 2..=16 per the
    /// router's `MAX_LEGS`.
    k: usize,
    router_program_id: Pubkey,
}

impl SupersonicCast {
    pub fn from_params(params: &toml::Table) -> anyhow::Result<Self> {
        let max_balance_fraction = params
            .get("max_balance_fraction")
            .and_then(|v| v.as_float())
            .unwrap_or(0.01);
        let min_cast_lamports = params
            .get("min_cast_lamports")
            .and_then(|v| v.as_integer())
            .unwrap_or(1_000_000) as u64; // 0.001 SOL default floor
        let k = params.get("k").and_then(|v| v.as_integer()).unwrap_or(8) as usize;
        if !(2..=16).contains(&k) {
            anyhow::bail!("supersonic_cast.k must be in 2..=16 (router MAX_LEGS)");
        }
        let router_program_id = params
            .get("router_program_id")
            .and_then(|v| v.as_str())
            .map(Pubkey::from_str)
            .transpose()
            .map_err(|e| anyhow::anyhow!("supersonic_cast.router_program_id: {e}"))?
            .unwrap_or_else(|| {
                Pubkey::from_str(DEFAULT_ROUTER_PROGRAM_ID).expect("valid hardcoded program id")
            });
        Ok(Self {
            max_balance_fraction,
            min_cast_lamports,
            k,
            router_program_id,
        })
    }
}

/// The bundle's recovery secret, derived from the agent wallet's own keypair —
/// mirrors `supersonic`-CLI's `derive_master_seed`, so no extra secret needs to be
/// provisioned per agent. Never leaves this process.
fn derive_master_seed(wallet: &Keypair) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(MASTER_TAG);
    h.update(wallet.to_bytes());
    h.finalize().into()
}

#[async_trait]
impl Protocol for SupersonicCast {
    fn name(&self) -> &str {
        "supersonic_cast"
    }

    async fn execute(&self, rpc: &RpcClient, wallet: &Keypair) -> anyhow::Result<Signature> {
        let balance_lamports = rpc.get_balance(&wallet.pubkey()).await?;
        let usable = (balance_lamports as f64 * self.max_balance_fraction) as u64;
        if usable == 0 {
            anyhow::bail!(
                "wallet balance too low to compute a non-zero cast amount, skipping this tick"
            );
        }
        if usable < self.min_cast_lamports {
            anyhow::bail!("balance too low for a believable cast, skipping this tick");
        }

        let amount = rand::thread_rng().gen_range((usable / 4).max(1)..=usable);

        // The router rejects a leg whose destination equals the signer
        // (`SupersonicError::SelfDestination`), so the "real" leg targets a fresh
        // one-off keypair derived from the same master seed rather than the
        // wallet's own pubkey. Funds land in an address only this wallet's seed
        // can be traced back to — no value leaves the agent's custody, it just
        // moves to a sibling address the agent controls.
        let master_seed = derive_master_seed(wallet);
        let bundle_id = rand::thread_rng().gen::<u64>();
        let real_dest = supersonic_sdk::derive_decoy_keypair(&master_seed, bundle_id, u32::MAX);

        let plan = plan_bundle(
            &master_seed,
            bundle_id,
            real_dest.pubkey(),
            amount,
            self.k,
            DecoyConfig::default(),
        )
        .map_err(|e| anyhow::anyhow!("supersonic bundle planning failed: {e}"))?;

        let ix = build_instruction(self.router_program_id, wallet.pubkey(), &plan);

        let recent_blockhash = rpc.get_latest_blockhash().await?;
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&wallet.pubkey()),
            &[wallet],
            recent_blockhash,
        );

        let sim = rpc.simulate_transaction(&tx).await?;
        if let Some(err) = &sim.value.err {
            let logs = sim
                .value
                .logs
                .as_ref()
                .map(|l| l.join("\n"))
                .unwrap_or_default();
            anyhow::bail!("supersonic_cast bundle simulation failed: {err:?}\nlogs:\n{logs}");
        }

        match rpc.send_and_confirm_transaction(&tx).await {
            Ok(sig) => Ok(sig),
            Err(e) => anyhow::bail!("supersonic_cast bundle send/confirm failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = SupersonicCast::from_params(&toml::Table::new()).unwrap();
        assert_eq!(cfg.k, 8);
        assert_eq!(
            cfg.router_program_id,
            Pubkey::from_str(DEFAULT_ROUTER_PROGRAM_ID).unwrap()
        );
    }

    #[test]
    fn rejects_bad_k() {
        let mut params = toml::Table::new();
        params.insert("k".into(), toml::Value::Integer(1));
        assert!(SupersonicCast::from_params(&params).is_err());
    }

    #[test]
    fn master_seed_is_deterministic_per_wallet() {
        let kp = Keypair::new();
        assert_eq!(derive_master_seed(&kp), derive_master_seed(&kp));
    }

    #[test]
    fn accepts_custom_router_program_id() {
        let custom = Keypair::new().pubkey();
        let mut params = toml::Table::new();
        params.insert(
            "router_program_id".into(),
            toml::Value::String(custom.to_string()),
        );
        let cfg = SupersonicCast::from_params(&params).unwrap();
        assert_eq!(cfg.router_program_id, custom);
    }
}

pub mod jupiter;
pub mod marinade;
pub mod orca_lp;
pub mod supersonic_cast;

use async_trait::async_trait;
use rand::seq::SliceRandom;
use rand::Rng;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{signature::Keypair, signature::Signature};

use crate::config::ProtocolConfig;

/// Any interaction an agent can perform on-chain: swap, stake, LP, bridge, vote, etc.
/// New protocols are added by implementing this trait and registering them below —
/// this is the extension point that keeps the fleet "trivially customizable."
#[async_trait]
pub trait Protocol: Send + Sync {
    fn name(&self) -> &str;

    /// Execute one interaction for the given wallet. Returns the tx signature on success.
    async fn execute(&self, rpc: &RpcClient, wallet: &Keypair) -> anyhow::Result<Signature>;
}

pub struct ProtocolRegistry {
    entries: Vec<(f64, Box<dyn Protocol>)>,
}

impl ProtocolRegistry {
    pub fn from_config(configs: &[ProtocolConfig]) -> anyhow::Result<Self> {
        let mut entries: Vec<(f64, Box<dyn Protocol>)> = Vec::new();
        for c in configs {
            let proto: Box<dyn Protocol> = match c.name.as_str() {
                "jupiter_swap" => Box::new(jupiter::JupiterSwap::from_params(&c.params)?),
                "marinade_stake" => Box::new(marinade::MarinadeStake::from_params(&c.params)?),
                "orca_lp" => Box::new(orca_lp::OrcaLp::from_params(&c.params)?),
                "supersonic_cast" => {
                    Box::new(supersonic_cast::SupersonicCast::from_params(&c.params)?)
                }
                other => anyhow::bail!("unknown protocol in config: {other}"),
            };
            entries.push((c.weight.max(0.0001), proto));
        }
        Ok(Self { entries })
    }

    /// Pick one protocol at random, weighted by configured frequency.
    pub fn pick(&self) -> &dyn Protocol {
        let total: f64 = self.entries.iter().map(|(w, _)| w).sum();
        let mut roll = rand::thread_rng().gen_range(0.0..total);
        for (w, p) in &self.entries {
            if roll < *w {
                return p.as_ref();
            }
            roll -= w;
        }
        // Fallback (floating point edge case)
        self.entries
            .choose(&mut rand::thread_rng())
            .unwrap()
            .1
            .as_ref()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

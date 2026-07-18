use async_trait::async_trait;
use rand::seq::SliceRandom;
use rand::Rng;
use serde::Deserialize;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    signature::{Keypair, Signature, Signer},
    transaction::VersionedTransaction,
};

use super::Protocol;

const JUPITER_QUOTE_URL: &str = "https://lite-api.jup.ag/swap/v1/quote";
const JUPITER_SWAP_URL: &str = "https://lite-api.jup.ag/swap/v1/swap";

/// Performs randomized small-value swaps between major mints via Jupiter.
/// This is the "obvious human behavior" building block — most real wallets
/// interact with a DEX aggregator more than any other single primitive.
pub struct JupiterSwap {
    /// Fraction of the wallet's SOL balance a single swap is allowed to use, e.g. 0.01 = 1%.
    max_balance_fraction: f64,
    slippage_bps: u16,
    /// Mint pool to rotate through. Configurable so operators can add new
    /// tokens (or restrict to fewer) without recompiling — see cooker.toml.
    noise_mints: Vec<String>,
    /// Minimum lamports required to attempt a swap; below this the agent
    /// skips the tick rather than sending a dust-sized, fee-losing tx.
    min_swap_lamports: u64,
}

#[derive(Deserialize)]
struct SwapResponse {
    #[serde(rename = "swapTransaction")]
    swap_transaction: String,
}

/// Fallback used only if `noise_mints` is absent from cooker.toml — well-known,
/// high-liquidity mints. Operators are expected to override this list in config.
const DEFAULT_NOISE_MINTS: &[&str] = &[
    "So11111111111111111111111111111111111111112",  // wSOL
    "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", // USDC
    "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB", // USDT
    "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So",  // mSOL
];

impl JupiterSwap {
    pub fn from_params(params: &toml::Table) -> anyhow::Result<Self> {
        let max_balance_fraction = params
            .get("max_balance_fraction")
            .and_then(|v| v.as_float())
            .unwrap_or(0.01);
        let slippage_bps = params
            .get("slippage_bps")
            .and_then(|v| v.as_integer())
            .unwrap_or(50) as u16;
        let noise_mints: Vec<String> = params
            .get("noise_mints")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .filter(|v: &Vec<String>| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_NOISE_MINTS.iter().map(|s| s.to_string()).collect());
        if noise_mints.len() < 2 {
            anyhow::bail!("jupiter_swap.noise_mints must list at least 2 mints to swap between");
        }
        let min_swap_lamports = params
            .get("min_swap_lamports")
            .and_then(|v| v.as_integer())
            .unwrap_or(5_000) as u64;
        Ok(Self {
            max_balance_fraction,
            slippage_bps,
            noise_mints,
            min_swap_lamports,
        })
    }

    fn pick_pair(&self) -> (&str, &str) {
        let mut rng = rand::thread_rng();
        let input = self
            .noise_mints
            .choose(&mut rng)
            .expect("validated non-empty in from_params");
        let output = loop {
            let candidate = self
                .noise_mints
                .choose(&mut rng)
                .expect("validated non-empty in from_params");
            if candidate != input {
                break candidate;
            }
        };
        (input.as_str(), output.as_str())
    }
}

#[async_trait]
impl Protocol for JupiterSwap {
    fn name(&self) -> &str {
        "jupiter_swap"
    }

    async fn execute(&self, rpc: &RpcClient, wallet: &Keypair) -> anyhow::Result<Signature> {
        let balance_lamports = rpc.get_balance(&wallet.pubkey()).await?;
        // Keep a safety reserve for fees/rent so the agent never drains itself.
        let usable = (balance_lamports as f64 * self.max_balance_fraction) as u64;
        if usable < self.min_swap_lamports {
            anyhow::bail!("balance too low for a believable swap, skipping this tick");
        }

        let amount = rand::thread_rng().gen_range((usable / 4).max(1)..=usable);
        let (input_mint, output_mint) = self.pick_pair();

        let client = reqwest::Client::new();

        let quote: serde_json::Value = client
            .get(JUPITER_QUOTE_URL)
            .query(&[
                ("inputMint", input_mint),
                ("outputMint", output_mint),
                ("amount", &amount.to_string()),
                ("slippageBps", &self.slippage_bps.to_string()),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let out_amount = quote
            .get("outAmount")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        tracing::debug!(
            "quote {} -> {} amount_in={} amount_out={}",
            input_mint,
            output_mint,
            amount,
            out_amount
        );

        let swap_body = serde_json::json!({
            "quoteResponse": quote,
            "userPublicKey": wallet.pubkey().to_string(),
            "wrapAndUnwrapSol": true,
        });

        let swap_resp = client
            .post(JUPITER_SWAP_URL)
            .json(&swap_body)
            .send()
            .await?;

        if !swap_resp.status().is_success() {
            let status = swap_resp.status();
            let body = swap_resp.text().await.unwrap_or_default();
            anyhow::bail!("jupiter swap request failed ({status}): {body}");
        }

        let swap: SwapResponse = swap_resp.json().await?;

        let tx_bytes = base64_decode(&swap.swap_transaction)?;
        let mut tx: VersionedTransaction = bincode::deserialize(&tx_bytes)?;
        tx.signatures[0] = wallet.sign_message(&tx.message.serialize());

        let sig = rpc.send_and_confirm_transaction(&tx).await?;
        Ok(sig)
    }
}

fn base64_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    Ok(STANDARD.decode(s)?)
}

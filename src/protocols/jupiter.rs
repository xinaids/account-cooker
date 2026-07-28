use async_trait::async_trait;
use rand::seq::SliceRandom;
use rand::Rng;
use serde::Deserialize;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer},
    transaction::VersionedTransaction,
};
use solana_system_interface::program as system_program;

use super::Protocol;

const JUPITER_QUOTE_URL: &str = "https://lite-api.jup.ag/swap/v1/quote";
const JUPITER_SWAP_URL: &str = "https://lite-api.jup.ag/swap/v1/swap";

/// Program IDs a legitimate Jupiter swap transaction may invoke as a
/// top-level instruction — verified, not assumed: `solana account <id> --url
/// mainnet-beta` confirms each is a real, executable program, and two live
/// `lite-api.jup.ag` quote+swap round-trips (a 1-hop wSOL->USDC route that
/// used an address lookup table, and a 2-hop USDT->mSOL route that didn't)
/// were decoded with this project's own `solana-sdk`/`bincode` versions.
/// Both showed the identical top-level instruction sequence for a
/// `wrapAndUnwrapSol: true` swap: ComputeBudget (SetComputeUnitLimit,
/// SetComputeUnitPrice) -> ATA CreateIdempotent -> System Transfer (SOL wrap)
/// -> Token SyncNative -> ATA CreateIdempotent -> the aggregator's own route
/// instruction -> Token CloseAccount (SOL unwrap). Address lookup tables were
/// used only to compress the large, per-route *pool* account list passed to
/// the aggregator instruction (24 accounts in the observed case) — never a
/// top-level program id, and per `solana_program::message::v0`'s own doc
/// comment this isn't just an observed convention: "Program indexes must
/// index into the list of message `account_keys` because program id's cannot
/// be dynamically loaded from a lookup table" is a hard message-format rule.
/// `validate_swap_transaction` still checks this explicitly rather than
/// assume a third-party API response is well-formed — see its handling of an
/// out-of-range `program_id_index` — but a real ALT-loaded program id isn't a
/// bypass this allowlist needs to defend against; it isn't a valid Solana
/// transaction at all.
const JUPITER_AGGREGATOR_PROGRAM_ID: &str = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ASSOCIATED_TOKEN_PROGRAM_ID: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

/// SPL Token instruction discriminators (first data byte;
/// token/program/src/instruction.rs) that grant a THIRD PARTY standing
/// control over a token account without requiring this wallet's signature on
/// any later transaction — the "approve-then-drain" pattern: an attacker who
/// receives delegate authority here can call `Transfer`/`TransferChecked` as
/// the delegate in a completely separate transaction this wallet never sees
/// or signs. None of the real instruction sequences observed above (and no
/// legitimate swap route) ever needs to grant delegate authority, so both are
/// rejected outright rather than allow-listed by account shape.
const SPL_TOKEN_IX_APPROVE: u8 = 4;
const SPL_TOKEN_IX_APPROVE_CHECKED: u8 = 13;

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
        if usable == 0 {
            anyhow::bail!(
                "wallet balance too low to compute a non-zero swap amount, skipping this tick"
            );
        }
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

        // Don't blind-sign whatever the API handed back: check the minimal
        // shape a single-wallet swap must have before this wallet's key ever
        // touches it. This can't verify semantic intent (which pools it
        // routes through, etc.) but it does guarantee the only party that
        // can be asked to sign is this wallet, and nothing hijacked the fee
        // payer slot.
        validate_swap_transaction(&tx, &wallet.pubkey())?;

        tx.signatures[0] = wallet.sign_message(&tx.message.serialize());

        // Simulate before sending — same pattern as marinade.rs /
        // supersonic_cast.rs: surface detailed logs on failure instead of
        // committing an unverified transaction to the network first.
        let sim = rpc.simulate_transaction(&tx).await?;
        if let Some(err) = &sim.value.err {
            let logs = sim
                .value
                .logs
                .as_ref()
                .map(|l| l.join("\n"))
                .unwrap_or_default();
            anyhow::bail!("jupiter swap simulation failed: {err:?}\nlogs:\n{logs}");
        }

        let sig = rpc.send_and_confirm_transaction(&tx).await?;
        Ok(sig)
    }
}

/// Shape + content check on a swap transaction built by a third-party HTTP
/// API, before this wallet ever signs it: exactly one required signer and
/// that signer/fee-payer is this wallet (unchanged from the original check),
/// PLUS every top-level instruction's program is in a known-safe allowlist,
/// and no SPL Token `Approve`/`ApproveChecked` instruction is present on any
/// of them. Both signers and top-level program ids are protocol-guaranteed to
/// live in the message's static account keys, never an address-lookup-table
/// -loaded account (see the allowlist doc comment) — but this function still
/// treats an out-of-range `program_id_index` as a hard failure rather than
/// index-panicking or silently skipping the check, in case a malformed or
/// corrupted response ever violates that invariant.
fn validate_swap_transaction(tx: &VersionedTransaction, wallet: &Pubkey) -> anyhow::Result<()> {
    if tx.signatures.is_empty() {
        anyhow::bail!("jupiter swap transaction has no signature slots");
    }
    let header = tx.message.header();
    if header.num_required_signatures != 1 {
        anyhow::bail!(
            "jupiter swap transaction requires {} signer(s), expected exactly 1 (this wallet)",
            header.num_required_signatures
        );
    }
    let static_keys = tx.message.static_account_keys();
    let fee_payer = static_keys
        .first()
        .ok_or_else(|| anyhow::anyhow!("jupiter swap transaction has no account keys"))?;
    if fee_payer != wallet {
        anyhow::bail!(
            "jupiter swap transaction fee payer {fee_payer} does not match wallet {wallet}"
        );
    }

    let allowed_program_ids = allowed_jupiter_program_ids()?;
    let token_program_id = TOKEN_PROGRAM_ID.parse::<Pubkey>()?;

    for (i, ix) in tx.message.instructions().iter().enumerate() {
        let idx = ix.program_id_index as usize;
        let program_id = static_keys.get(idx).ok_or_else(|| {
            anyhow::anyhow!(
                "jupiter swap transaction instruction {i} targets program_id_index {idx}, which \
                 is outside the transaction's {} static account key(s) — i.e. loaded via an \
                 address lookup table. This wallet cannot verify an ALT-loaded program against \
                 the allowlist without an extra RPC round trip to resolve it, so it refuses to \
                 sign rather than trust an unverifiable program",
                static_keys.len()
            )
        })?;
        if !allowed_program_ids.contains(program_id) {
            anyhow::bail!(
                "jupiter swap transaction instruction {i} targets program {program_id}, which is \
                 not in the allowed program list for a Jupiter swap {allowed_program_ids:?} — \
                 refusing to sign"
            );
        }
        if *program_id == token_program_id {
            if let Some(&discriminator) = ix.data.first() {
                if discriminator == SPL_TOKEN_IX_APPROVE
                    || discriminator == SPL_TOKEN_IX_APPROVE_CHECKED
                {
                    anyhow::bail!(
                        "jupiter swap transaction instruction {i} is an SPL Token {} \
                         instruction (discriminator {discriminator}), which would grant \
                         delegate authority over this wallet's token account to a third party \
                         — refusing to sign",
                        if discriminator == SPL_TOKEN_IX_APPROVE {
                            "Approve"
                        } else {
                            "ApproveChecked"
                        }
                    );
                }
            }
        }
    }

    Ok(())
}

/// The known-safe program allowlist for `validate_swap_transaction` — see
/// the constants' own doc comment for how each entry was verified.
fn allowed_jupiter_program_ids() -> anyhow::Result<[Pubkey; 5]> {
    Ok([
        JUPITER_AGGREGATOR_PROGRAM_ID.parse()?,
        TOKEN_PROGRAM_ID.parse()?,
        ASSOCIATED_TOKEN_PROGRAM_ID.parse()?,
        system_program::id(),
        // Compute Budget is a native runtime program, not a BPF-deployed
        // one — its address is a fixed SDK constant
        // (`solana_sdk::compute_budget::ID` / `declare_id!` in
        // solana-sdk's own source), reproduced here as a string literal
        // for consistency with this file's other hardcoded program IDs
        // rather than importing the gated `compute_budget` module for a
        // single constant. Confirmed against solana-sdk 2.1.10's own
        // source (`declare_id!("ComputeBudget111111111111111111111111111111")`)
        // and cross-checked against the live-decoded transactions above,
        // where it appears twice (SetComputeUnitLimit, SetComputeUnitPrice)
        // in every swap.
        "ComputeBudget111111111111111111111111111111".parse()?,
    ])
}

fn base64_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    Ok(STANDARD.decode(s)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::{
        hash::Hash,
        instruction::{AccountMeta, CompiledInstruction, Instruction},
        message::{v0, Message, MessageHeader, VersionedMessage},
    };

    fn jupiter_program_id() -> Pubkey {
        JUPITER_AGGREGATOR_PROGRAM_ID.parse().unwrap()
    }

    fn token_program_id() -> Pubkey {
        TOKEN_PROGRAM_ID.parse().unwrap()
    }

    /// Compiles a legacy message the same way the real client would (via
    /// `Message::new`, not by hand), so `num_required_signatures` and the
    /// fee-payer-is-first-key invariant come from the real compiler rather
    /// than from an assumption baked into the test.
    fn legacy_tx(payer: &Pubkey, instructions: &[Instruction]) -> VersionedTransaction {
        let message = Message::new(instructions, Some(payer));
        let num_signatures = message.header.num_required_signatures as usize;
        VersionedTransaction {
            signatures: vec![Signature::default(); num_signatures],
            message: VersionedMessage::Legacy(message),
        }
    }

    /// A minimal but representative slice of a real `wrapAndUnwrapSol: true`
    /// swap's instruction sequence (see the allowlist doc comment for the
    /// full, empirically-observed sequence): the aggregator's own route
    /// instruction plus an SPL Token `SyncNative` (discriminator 17 — not
    /// one of the two discriminators this fix rejects).
    fn well_formed_swap_instructions(wallet: &Pubkey) -> Vec<Instruction> {
        vec![
            Instruction::new_with_bytes(
                jupiter_program_id(),
                &[0u8],
                vec![AccountMeta::new(*wallet, true)],
            ),
            Instruction::new_with_bytes(
                token_program_id(),
                &[17u8],
                vec![AccountMeta::new(*wallet, false)],
            ),
        ]
    }

    #[test]
    fn accepts_a_well_formed_swap_shaped_transaction() {
        let wallet = Pubkey::new_unique();
        let tx = legacy_tx(&wallet, &well_formed_swap_instructions(&wallet));
        assert!(validate_swap_transaction(&tx, &wallet).is_ok());
    }

    #[test]
    fn rejects_more_than_one_required_signer() {
        let wallet = Pubkey::new_unique();
        let other_signer = Pubkey::new_unique();
        let ix = Instruction::new_with_bytes(
            jupiter_program_id(),
            &[0u8],
            vec![
                AccountMeta::new(wallet, true),
                AccountMeta::new(other_signer, true),
            ],
        );
        let tx = legacy_tx(&wallet, &[ix]);
        let err = validate_swap_transaction(&tx, &wallet)
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires 2 signer"), "{err}");
    }

    #[test]
    fn rejects_wrong_fee_payer() {
        let wallet = Pubkey::new_unique();
        let attacker = Pubkey::new_unique();
        let tx = legacy_tx(&attacker, &well_formed_swap_instructions(&attacker));
        let err = validate_swap_transaction(&tx, &wallet)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not match wallet"), "{err}");
    }

    #[test]
    fn rejects_instruction_targeting_a_program_outside_the_allowlist() {
        let wallet = Pubkey::new_unique();
        let rogue_program = Pubkey::new_unique();
        let ix = Instruction::new_with_bytes(
            rogue_program,
            &[0u8],
            vec![AccountMeta::new(wallet, true)],
        );
        let tx = legacy_tx(&wallet, &[ix]);
        let err = validate_swap_transaction(&tx, &wallet)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not in the allowed program list"), "{err}");
    }

    /// The regression test for the finding itself: a swap transaction that
    /// is otherwise well-formed, plus a smuggled-in SPL Token `Approve`
    /// (discriminator 4) that would hand a third party standing delegate
    /// authority over this wallet's token account — without ever needing a
    /// second signature, since the wallet is already the fee-payer signer.
    #[test]
    fn rejects_embedded_approve_instruction() {
        let wallet = Pubkey::new_unique();
        let attacker_delegate = Pubkey::new_unique();
        let mut instructions = well_formed_swap_instructions(&wallet);
        // SPL Token `Approve` accounts: [source, delegate, owner (signer)].
        instructions.push(Instruction::new_with_bytes(
            token_program_id(),
            &[SPL_TOKEN_IX_APPROVE],
            vec![
                AccountMeta::new(wallet, false),
                AccountMeta::new_readonly(attacker_delegate, false),
                AccountMeta::new_readonly(wallet, true),
            ],
        ));
        let tx = legacy_tx(&wallet, &instructions);
        let err = validate_swap_transaction(&tx, &wallet)
            .unwrap_err()
            .to_string();
        assert!(err.contains("delegate authority"), "{err}");
    }

    #[test]
    fn rejects_embedded_approve_checked_instruction() {
        let wallet = Pubkey::new_unique();
        let attacker_delegate = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let mut instructions = well_formed_swap_instructions(&wallet);
        // SPL Token `ApproveChecked` accounts: [source, mint, delegate, owner (signer)].
        instructions.push(Instruction::new_with_bytes(
            token_program_id(),
            &[SPL_TOKEN_IX_APPROVE_CHECKED],
            vec![
                AccountMeta::new(wallet, false),
                AccountMeta::new_readonly(mint, false),
                AccountMeta::new_readonly(attacker_delegate, false),
                AccountMeta::new_readonly(wallet, true),
            ],
        ));
        let tx = legacy_tx(&wallet, &instructions);
        let err = validate_swap_transaction(&tx, &wallet)
            .unwrap_err()
            .to_string();
        assert!(err.contains("delegate authority"), "{err}");
    }

    /// Not a real Jupiter response shape — Solana's message format makes a
    /// top-level `program_id_index` outside the static account keys
    /// impossible to compile honestly (see the allowlist doc comment), so
    /// this hand-builds a v0 message to simulate a malformed/corrupted one
    /// and confirms the function fails closed instead of panicking on the
    /// out-of-bounds index or silently skipping the check.
    #[test]
    fn rejects_out_of_range_program_id_index_it_cannot_verify() {
        let wallet = Pubkey::new_unique();
        let message = v0::Message {
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 0,
            },
            account_keys: vec![wallet],
            recent_blockhash: Hash::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 5, // out of range: only 1 static key present
                accounts: vec![],
                data: vec![0u8],
            }],
            address_table_lookups: vec![],
        };
        let tx = VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::V0(message),
        };
        let err = validate_swap_transaction(&tx, &wallet)
            .unwrap_err()
            .to_string();
        assert!(err.contains("address lookup table"), "{err}");
    }
}

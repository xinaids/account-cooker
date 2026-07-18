use async_trait::async_trait;
use rand::Rng;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer},
    transaction::Transaction,
};
use solana_system_interface::program as system_program;

use super::Protocol;

/// Marinade liquid staking `State` account (mainnet-beta). This is a fixed,
/// structural address (same category as jupiter.rs's `JUPITER_QUOTE_URL`),
/// not a behavior-relevant config value — verified live against mainnet
/// (owner == Marinade program) before use.
const MARINADE_STATE: &str = "8szGkuLTAux9XMgZ2vtY39jVSowEcpBfFfD8hXSEqdGC";
const MARINADE_PROGRAM_ID: &str = "MarBmsSgKXdrN1egZf5sqe1TMai9K1rChYNDJgjq7aD";
const MSOL_MINT: &str = "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So";

// PDA seed constants, taken verbatim from marinade-finance/liquid-staking-program
// (programs/marinade-finance/src/state/mod.rs and state/liq_pool.rs).
const RESERVE_SEED: &[u8] = b"reserve";
const MSOL_MINT_AUTHORITY_SEED: &[u8] = b"st_mint";
const SOL_LEG_SEED: &[u8] = b"liq_sol";
const MSOL_LEG_AUTHORITY_SEED: &[u8] = b"liq_st_sol_authority";
const MSOL_LEG_SEED_STR: &str = "liq_st_sol";

// Anchor instruction discriminator: sha256("global:deposit")[0..8].
const DEPOSIT_DISCRIMINATOR: [u8; 8] = [242, 35, 198, 137, 82, 225, 242, 182];

const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ASSOCIATED_TOKEN_PROGRAM_ID: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

/// Performs randomized small-value SOL deposits into Marinade's liquid
/// staking pool, minting mSOL to the caller. This is the "liquid staking"
/// building block — depositing/holding mSOL is extremely common human
/// wallet behavior and diversifies an agent's on-chain footprint beyond
/// pure swapping.
pub struct MarinadeStake {
    /// Fraction of the wallet's SOL balance a single deposit is allowed to use.
    max_balance_fraction: f64,
    /// Minimum lamports required to attempt a deposit; below this the agent
    /// skips the tick rather than sending a dust-sized, fee-losing tx.
    min_stake_lamports: u64,
}

impl MarinadeStake {
    pub fn from_params(params: &toml::Table) -> anyhow::Result<Self> {
        let max_balance_fraction = params
            .get("max_balance_fraction")
            .and_then(|v| v.as_float())
            .unwrap_or(0.02);
        let min_stake_lamports = params
            .get("min_stake_lamports")
            .and_then(|v| v.as_integer())
            .unwrap_or(1_000_000) as u64; // 0.001 SOL default floor
        Ok(Self {
            max_balance_fraction,
            min_stake_lamports,
        })
    }
}

struct MarinadeAccounts {
    state: Pubkey,
    msol_mint: Pubkey,
    liq_pool_sol_leg_pda: Pubkey,
    liq_pool_msol_leg: Pubkey,
    liq_pool_msol_leg_authority: Pubkey,
    reserve_pda: Pubkey,
    msol_mint_authority: Pubkey,
    program_id: Pubkey,
}

impl MarinadeAccounts {
    fn derive() -> anyhow::Result<Self> {
        let program_id: Pubkey = MARINADE_PROGRAM_ID.parse()?;
        let state: Pubkey = MARINADE_STATE.parse()?;
        let msol_mint: Pubkey = MSOL_MINT.parse()?;
        let token_program: Pubkey = TOKEN_PROGRAM_ID.parse()?;

        let state_bytes = state.to_bytes();

        let (liq_pool_sol_leg_pda, _) =
            Pubkey::find_program_address(&[&state_bytes, SOL_LEG_SEED], &program_id);
        let (liq_pool_msol_leg_authority, _) =
            Pubkey::find_program_address(&[&state_bytes, MSOL_LEG_AUTHORITY_SEED], &program_id);
        let (reserve_pda, _) =
            Pubkey::find_program_address(&[&state_bytes, RESERVE_SEED], &program_id);
        let (msol_mint_authority, _) =
            Pubkey::find_program_address(&[&state_bytes, MSOL_MINT_AUTHORITY_SEED], &program_id);

        // liq_pool_msol_leg is NOT a PDA — it's a token account created with
        // `Pubkey::create_with_seed(state, "liq_st_sol", spl_token::ID)`.
        let liq_pool_msol_leg =
            Pubkey::create_with_seed(&state, MSOL_LEG_SEED_STR, &token_program)?;

        Ok(Self {
            state,
            msol_mint,
            liq_pool_sol_leg_pda,
            liq_pool_msol_leg,
            liq_pool_msol_leg_authority,
            reserve_pda,
            msol_mint_authority,
            program_id,
        })
    }
}

fn find_associated_token_address(owner: &Pubkey, mint: &Pubkey) -> anyhow::Result<Pubkey> {
    let token_program: Pubkey = TOKEN_PROGRAM_ID.parse()?;
    let ata_program: Pubkey = ASSOCIATED_TOKEN_PROGRAM_ID.parse()?;
    let (address, _) = Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ata_program,
    );
    Ok(address)
}

fn create_ata_instruction(
    payer: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    ata: &Pubkey,
) -> anyhow::Result<Instruction> {
    let token_program: Pubkey = TOKEN_PROGRAM_ID.parse()?;
    let ata_program: Pubkey = ASSOCIATED_TOKEN_PROGRAM_ID.parse()?;
    Ok(Instruction {
        program_id: ata_program,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(*ata, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(token_program, false),
        ],
        // idempotent create-if-needed variant (discriminator 1) — safe to
        // include even if the ATA already exists.
        data: vec![1],
    })
}

fn build_deposit_instruction(
    accounts: &MarinadeAccounts,
    wallet: &Pubkey,
    mint_to: &Pubkey,
    lamports: u64,
) -> Instruction {
    let token_program: Pubkey = TOKEN_PROGRAM_ID.parse().expect("valid static pubkey");

    let mut data = Vec::with_capacity(16);
    data.extend_from_slice(&DEPOSIT_DISCRIMINATOR);
    data.extend_from_slice(&lamports.to_le_bytes());

    Instruction {
        program_id: accounts.program_id,
        accounts: vec![
            AccountMeta::new(accounts.state, false),
            AccountMeta::new(accounts.msol_mint, false),
            AccountMeta::new(accounts.liq_pool_sol_leg_pda, false),
            AccountMeta::new(accounts.liq_pool_msol_leg, false),
            AccountMeta::new_readonly(accounts.liq_pool_msol_leg_authority, false),
            AccountMeta::new(accounts.reserve_pda, false),
            AccountMeta::new(*wallet, true),
            AccountMeta::new(*mint_to, false),
            AccountMeta::new_readonly(accounts.msol_mint_authority, false),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(token_program, false),
        ],
        data,
    }
}

#[async_trait]
impl Protocol for MarinadeStake {
    fn name(&self) -> &str {
        "marinade_stake"
    }

    async fn execute(&self, rpc: &RpcClient, wallet: &Keypair) -> anyhow::Result<Signature> {
        let balance_lamports = rpc.get_balance(&wallet.pubkey()).await?;
        let usable = (balance_lamports as f64 * self.max_balance_fraction) as u64;
        if usable < self.min_stake_lamports {
            anyhow::bail!("balance too low for a believable stake, skipping this tick");
        }

        let amount = rand::thread_rng().gen_range((usable / 4).max(1)..=usable);

        let accounts = MarinadeAccounts::derive()?;
        let mint_to = find_associated_token_address(&wallet.pubkey(), &accounts.msol_mint)?;

        let mut ixs = Vec::new();

        // Create the caller's mSOL ATA if it doesn't exist yet — required
        // before mSOL can be minted/transferred to them. Idempotent, so
        // it's safe to always include.
        let ata_info = rpc.get_account(&mint_to).await;
        if ata_info.is_err() {
            ixs.push(create_ata_instruction(
                &wallet.pubkey(),
                &wallet.pubkey(),
                &accounts.msol_mint,
                &mint_to,
            )?);
        }

        ixs.push(build_deposit_instruction(
            &accounts,
            &wallet.pubkey(),
            &mint_to,
            amount,
        ));

        let recent_blockhash = rpc.get_latest_blockhash().await?;
        let tx = Transaction::new_signed_with_payer(
            &ixs,
            Some(&wallet.pubkey()),
            &[wallet],
            recent_blockhash,
        );

        // Simulate first so a malformed instruction surfaces detailed logs
        // instead of a bare RPC error, mirroring how jupiter.rs surfaces
        // HTTP status + body on failure.
        let sim = rpc.simulate_transaction(&tx).await?;
        if let Some(err) = &sim.value.err {
            let logs = sim
                .value
                .logs
                .as_ref()
                .map(|l| l.join("\n"))
                .unwrap_or_default();
            anyhow::bail!("marinade deposit simulation failed: {err:?}\nlogs:\n{logs}");
        }

        match rpc.send_and_confirm_transaction(&tx).await {
            Ok(sig) => Ok(sig),
            Err(e) => {
                // solana-client's ClientError Display already includes RPC
                // simulation logs when available (via TransactionError /
                // RpcResponseError variants); surface it verbatim rather
                // than collapsing to a generic message.
                anyhow::bail!("marinade deposit send/confirm failed: {e}")
            }
        }
    }
}

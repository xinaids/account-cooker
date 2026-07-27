use async_trait::async_trait;
use mpl_token_metadata::{
    accounts::{MasterEdition, Metadata},
    instructions::{CreateV1Builder, MintV1Builder},
    types::{PrintSupply, TokenStandard},
    MAX_NAME_LENGTH, MAX_SYMBOL_LENGTH, MAX_URI_LENGTH,
};
use sha2::{Digest, Sha256};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{keypair_from_seed, Keypair, Signature, Signer},
    transaction::Transaction,
};
use solana_system_interface::program as system_program;

use super::Protocol;

const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ASSOCIATED_TOKEN_PROGRAM_ID: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

/// Domain-separation tag for the sibling-address derivation below. Deliberately
/// distinct from `supersonic_cast.rs`'s `MASTER_TAG` — even for the same wallet,
/// `Sha256(SIBLING_TAG || secret)` and `Sha256(MASTER_TAG || secret)` produce
/// unrelated seeds, so the two protocols can never derive the same sibling
/// keypair or be confused with one another.
const SIBLING_TAG: &[u8] = b"account-cooker/nft_flip/sibling/v1";

/// SPL Token `TransferChecked` instruction discriminant (token/program/src/instruction.rs).
const SPL_TOKEN_IX_TRANSFER_CHECKED: u8 = 12;

/// Mints a fresh, brand-new 1/1 NFT via the Metaplex Token Metadata program
/// (`CreateV1` + `MintV1`, official `mpl-token-metadata` crate — confirmed
/// compatible with this project's `solana-sdk = "2.1"`, unlike marinade.rs /
/// dao_vote.rs's targets, so those two instructions are NOT hand-built here)
/// to the agent's own wallet, then optionally "flips" it to a sibling address
/// deterministically derived from the same wallet. Minting and (optionally)
/// moving a freshly-minted NFT is common, unremarkable human wallet behavior —
/// a third protocol category alongside swapping (`jupiter.rs`) and staking
/// (`marinade.rs`).
///
/// # Why this is safe
///
/// Every asset this protocol touches is one it created itself, in the same
/// transaction that mints it:
///
/// - The mint is a brand-new `Keypair::new()` generated fresh on every call
///   (see `build_and_simulate`) and passed to `CreateV1` as a signer, so the
///   on-chain program initializes it as a new SPL Mint at that address. There
///   is no code path anywhere in this file that reads, resolves, or accepts
///   an already-existing mint from config or from chain — it is
///   architecturally impossible for this protocol to mint into, or be
///   confused with, someone else's real NFT. This is the key difference from
///   `dao_vote.rs`, which necessarily interacts with a real third party's
///   proposal; this protocol never touches anything that existed before the
///   call that creates it.
/// - `token_standard: NonFungible` + `decimals: Some(0)` +
///   `print_supply: Some(PrintSupply::Zero)` makes the Master Edition account
///   permanently record zero allowed additional prints (Metaplex Token
///   Metadata, `generated/types/print_supply.rs`) — a true 1/1, not a
///   fungible/semi-fungible asset and not an open edition.
/// - The optional "flip" leg (`flip_to_sibling`, default on) never sends to a
///   third party. Its destination is derived the same way as
///   `supersonic_cast.rs`'s `derive_master_seed` — `Sha256(TAG ||
///   wallet_secret_bytes)` fed to a keypair-from-seed function — but
///   reimplemented locally against `solana_sdk::signature::keypair_from_seed`
///   with its own domain-separation tag (`SIBLING_TAG` above) instead of
///   depending on `supersonic_sdk`, so this protocol has no coupling to that
///   one. Only the derived keypair's `.pubkey()` is ever used (see
///   `derive_sibling_pubkey`); its secret key is computed and immediately
///   dropped — never persisted, never a signer of anything, never leaves this
///   function.
/// - No marketplace, escrow, auction house, or any other third-party program
///   is invoked anywhere in this file. The only programs this protocol ever
///   calls are: Metaplex Token Metadata (`CreateV1`, `MintV1`), and, only if
///   `flip_to_sibling` is set, the SPL Token program (`TransferChecked`) and
///   SPL Associated Token Account program (idempotent create) — the latter
///   two hand-built with the exact same discriminants/account order already
///   proven in `marinade.rs`'s `create_ata_instruction`.
///
/// Unlike `dao_vote.rs` and `supersonic_cast.rs`, there is no config field
/// here that names an external address to interact with (no
/// `proposal_pubkey`, no `router_program_id` pointing at a specific
/// deployment) — the sibling destination is always internally derived, never
/// operator-supplied, so there is no way to configure this protocol into
/// touching a real third party even by mistake.
///
/// # Scope limitation
///
/// Only the classic `TokenStandard::NonFungible` asset type is supported —
/// no Programmable NFT (`token_record`, `authorization_rules` are always
/// omitted/`None`) and no off-chain metadata JSON is uploaded or validated
/// (an empty or unreachable `uri` mints successfully; it just means
/// wallets/indexers show no image/attributes for it). Both are deliberate:
/// pNFT's token-delegate/authorization-rules machinery and real off-chain
/// hosting are complexity this protocol doesn't need to prove the "mint +
/// transfer" behavior category.
pub struct NftFlip {
    name: String,
    symbol: String,
    uri: String,
    seller_fee_basis_points: u16,
    /// If true (default), also transfers the freshly-minted NFT to a sibling
    /// address derived from the wallet itself (see module docs) — simulates a
    /// "mint then flip" pattern without ever naming a third party.
    flip_to_sibling: bool,
    /// Minimum wallet balance required to attempt a mint — covers rent for
    /// the mint, metadata, and master edition accounts, plus up to two
    /// associated-token accounts (wallet's own + sibling's, if flipping) and
    /// fees. Default is set above a REAL measured cost (a real
    /// `flip_to_sibling = true` devnet transaction cost exactly 0.02169584
    /// SOL all-in: see README.md's "NFT mint + transfer" proof section for
    /// the tx), not a guess — same "skip this tick rather than send a doomed
    /// tx" guard as `min_swap_lamports` / `min_stake_lamports` /
    /// `min_balance_lamports` in the other protocols.
    min_balance_lamports: u64,
}

impl NftFlip {
    pub fn from_params(params: &toml::Table) -> anyhow::Result<Self> {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("account-cooker noise NFT")
            .to_string();
        if name.len() > MAX_NAME_LENGTH {
            anyhow::bail!(
                "nft_flip.name exceeds Metaplex's {MAX_NAME_LENGTH}-byte limit ({} bytes)",
                name.len()
            );
        }

        let symbol = params
            .get("symbol")
            .and_then(|v| v.as_str())
            .unwrap_or("NOISE")
            .to_string();
        if symbol.len() > MAX_SYMBOL_LENGTH {
            anyhow::bail!(
                "nft_flip.symbol exceeds Metaplex's {MAX_SYMBOL_LENGTH}-byte limit ({} bytes)",
                symbol.len()
            );
        }

        // No default beyond empty — a real off-chain metadata JSON is
        // deployment-specific and out of scope for this protocol (see
        // "Scope limitation" below); an empty URI is valid on-chain, it just
        // means wallets/indexers show no off-chain image/attributes.
        let uri = params
            .get("uri")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if uri.len() > MAX_URI_LENGTH {
            anyhow::bail!(
                "nft_flip.uri exceeds Metaplex's {MAX_URI_LENGTH}-byte limit ({} bytes)",
                uri.len()
            );
        }

        let seller_fee_basis_points = params
            .get("seller_fee_basis_points")
            .and_then(|v| v.as_integer())
            .unwrap_or(0);
        if !(0..=10_000).contains(&seller_fee_basis_points) {
            anyhow::bail!(
                "nft_flip.seller_fee_basis_points must be in 0..=10000 (basis points of 100%), got {seller_fee_basis_points}"
            );
        }

        let flip_to_sibling = params
            .get("flip_to_sibling")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let min_balance_lamports = params
            .get("min_balance_lamports")
            .and_then(|v| v.as_integer())
            .unwrap_or(25_000_000) as u64; // 0.025 SOL — ~15% headroom over a real measured 0.02169584 SOL cost

        Ok(Self {
            name,
            symbol,
            uri,
            seller_fee_basis_points: seller_fee_basis_points as u16,
            flip_to_sibling,
            min_balance_lamports,
        })
    }

    /// Builds a fresh mint + metadata + master edition + mint-to-self
    /// transaction (and, if `flip_to_sibling`, the transfer-to-sibling leg),
    /// and simulates it — everything `execute()` needs before it's safe to
    /// send. Returns the transaction alongside the mint address it would
    /// create, since (unlike every other protocol here) that address doesn't
    /// exist until this call generates it, and is otherwise unrecoverable
    /// from the transaction object alone without re-parsing instructions.
    /// Shared by `execute()` (which sends afterward) and `simulate()` (which
    /// doesn't), so the two can never drift apart.
    async fn build_and_simulate(
        &self,
        rpc: &RpcClient,
        wallet: &Keypair,
    ) -> anyhow::Result<(Transaction, Pubkey)> {
        let balance = rpc.get_balance(&wallet.pubkey()).await?;
        if balance < self.min_balance_lamports {
            anyhow::bail!(
                "wallet balance too low ({balance} lamports) to safely cover mint + metadata + master edition rent, skipping this tick"
            );
        }

        let mint = Keypair::new();
        let mint_pubkey = mint.pubkey();
        let (metadata_pda, _) = Metadata::find_pda(&mint_pubkey);
        let (master_edition_pda, _) = MasterEdition::find_pda(&mint_pubkey);
        let token_program: Pubkey = TOKEN_PROGRAM_ID.parse()?;

        // CreateV1: creates + initializes the mint (0 decimals), the metadata
        // account, and the master edition account (print_supply = Zero) in a
        // single instruction. `mint(mint_pubkey, true)` marks the fresh mint
        // keypair as a signer, so the on-chain program is authorized to
        // create an account at that address — see module docs for why this
        // mint can never be an existing/third-party asset. `spl_token_program`
        // must be set explicitly: leaving it `None` doesn't fall back to a
        // sane default the way `system_program`/`sysvar_instructions` do —
        // the builder instead pushes the Metadata program's own ID as a
        // placeholder, which the on-chain processor rejects with "Missing
        // SPL token program" (confirmed via a real devnet simulation).
        let create_ix = CreateV1Builder::new()
            .metadata(metadata_pda)
            .master_edition(Some(master_edition_pda))
            .mint(mint_pubkey, true)
            .authority(wallet.pubkey())
            .payer(wallet.pubkey())
            .update_authority(wallet.pubkey(), true)
            .spl_token_program(Some(token_program))
            .name(self.name.clone())
            .symbol(self.symbol.clone())
            .uri(self.uri.clone())
            .seller_fee_basis_points(self.seller_fee_basis_points)
            .token_standard(TokenStandard::NonFungible)
            .decimals(0)
            .print_supply(PrintSupply::Zero)
            .instruction();

        let wallet_ata = find_associated_token_address(&wallet.pubkey(), &mint_pubkey)?;

        // MintV1 creates the destination associated token account itself
        // (that's why `spl_ata_program`/`system_program` are among its
        // accounts, defaulted by the builder below) and mints the single
        // unit into it — no separate CreateIdempotent instruction is needed
        // for the wallet's own ATA, unlike marinade.rs's deposit flow.
        let mint_ix = MintV1Builder::new()
            .token(wallet_ata)
            .token_owner(Some(wallet.pubkey()))
            .metadata(metadata_pda)
            .master_edition(Some(master_edition_pda))
            .mint(mint_pubkey)
            .authority(wallet.pubkey())
            .payer(wallet.pubkey())
            .amount(1)
            .instruction();

        let mut ixs = vec![create_ix, mint_ix];

        if self.flip_to_sibling {
            let sibling_pubkey = derive_sibling_pubkey(wallet)?;
            let sibling_ata = find_associated_token_address(&sibling_pubkey, &mint_pubkey)?;

            // Sibling's ATA never existed before this call (the mint itself
            // is brand new), so this always fires in practice; the existence
            // check is kept for symmetry with marinade.rs's ATA-creation
            // pattern and to stay correct if that assumption ever changes.
            if rpc.get_account(&sibling_ata).await.is_err() {
                ixs.push(create_ata_instruction(
                    &wallet.pubkey(),
                    &sibling_pubkey,
                    &mint_pubkey,
                    &sibling_ata,
                )?);
            }

            ixs.push(build_transfer_checked_instruction(
                &wallet_ata,
                &mint_pubkey,
                &sibling_ata,
                &wallet.pubkey(),
                1,
                0,
            )?);
        }

        let recent_blockhash = rpc.get_latest_blockhash().await?;
        let tx = Transaction::new_signed_with_payer(
            &ixs,
            Some(&wallet.pubkey()),
            &[wallet, &mint],
            recent_blockhash,
        );

        // Simulate first — same pattern as every other protocol in this
        // crate: surface detailed logs on failure instead of committing an
        // unverified transaction to the network first.
        let sim = rpc.simulate_transaction(&tx).await?;
        if let Some(err) = &sim.value.err {
            let logs = sim
                .value
                .logs
                .as_ref()
                .map(|l| l.join("\n"))
                .unwrap_or_default();
            anyhow::bail!("nft_flip mint simulation failed: {err:?}\nlogs:\n{logs}");
        }

        Ok((tx, mint_pubkey))
    }

    /// Builds and simulates the exact transaction `execute()` would send,
    /// without ever sending it — a non-destructive dry run for operators
    /// (and this crate's own proof-gathering) to verify the configured
    /// name/symbol/uri/fee resolve cleanly before opting in for real. Returns
    /// the mint address the transaction *would* create on a clean
    /// simulation — note this mint is never actually created by this call
    /// (nothing is sent), so a subsequent real `execute()` call generates a
    /// different (but structurally identical) mint of its own.
    pub async fn simulate(&self, rpc: &RpcClient, wallet: &Keypair) -> anyhow::Result<Pubkey> {
        let (_tx, mint) = self.build_and_simulate(rpc, wallet).await?;
        Ok(mint)
    }

    /// Same as `Protocol::execute`, but also returns the mint address that
    /// was just created — useful for proof-gathering (the trait's fixed
    /// `execute(...) -> Result<Signature>` signature can't surface this,
    /// since every other protocol here only ever touches addresses already
    /// known ahead of time). `Protocol::execute` below is a thin wrapper over
    /// this, so the two can never drift apart.
    pub async fn execute_returning_mint(
        &self,
        rpc: &RpcClient,
        wallet: &Keypair,
    ) -> anyhow::Result<(Signature, Pubkey)> {
        let (tx, mint) = self.build_and_simulate(rpc, wallet).await?;
        match rpc.send_and_confirm_transaction(&tx).await {
            Ok(sig) => Ok((sig, mint)),
            Err(e) => anyhow::bail!("nft_flip mint send/confirm failed: {e}"),
        }
    }
}

/// Deterministically derives a sibling wallet's public key from the agent's
/// own keypair — same pattern as `supersonic_cast.rs`'s `derive_master_seed`,
/// reimplemented locally (see module docs for why) with its own
/// domain-separation tag. The secret half of the derived keypair is computed
/// and dropped in the same expression; only `.pubkey()` ever escapes this
/// function.
fn derive_sibling_pubkey(wallet: &Keypair) -> anyhow::Result<Pubkey> {
    let mut h = Sha256::new();
    h.update(SIBLING_TAG);
    h.update(wallet.to_bytes());
    let seed: [u8; 32] = h.finalize().into();
    let sibling = keypair_from_seed(&seed)
        .map_err(|e| anyhow::anyhow!("nft_flip sibling keypair derivation failed: {e}"))?;
    Ok(sibling.pubkey())
}

/// Duplicated from `marinade.rs` rather than shared, matching this crate's
/// existing convention of each protocol file being self-contained.
fn find_associated_token_address(owner: &Pubkey, mint: &Pubkey) -> anyhow::Result<Pubkey> {
    let token_program: Pubkey = TOKEN_PROGRAM_ID.parse()?;
    let ata_program: Pubkey = ASSOCIATED_TOKEN_PROGRAM_ID.parse()?;
    let (address, _) = Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ata_program,
    );
    Ok(address)
}

/// Duplicated from `marinade.rs`'s `create_ata_instruction` (same
/// discriminator, same account order) rather than shared — see
/// `find_associated_token_address` above.
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

/// SPL Token `TransferChecked` (token/program/src/instruction.rs): accounts
/// `[source (writable), mint (readonly), destination (writable), authority
/// (signer)]`, data `[12, amount: u64 LE, decimals: u8]`. Used instead of
/// legacy `Transfer` per this project's Anchor/Rust rules (checked variants
/// preferred) and to match `.claude/rules/anchor.md`'s guidance even though
/// this is hand-built client code, not an Anchor CPI.
fn build_transfer_checked_instruction(
    source: &Pubkey,
    mint: &Pubkey,
    destination: &Pubkey,
    authority: &Pubkey,
    amount: u64,
    decimals: u8,
) -> anyhow::Result<Instruction> {
    let token_program: Pubkey = TOKEN_PROGRAM_ID.parse()?;
    let mut data = Vec::with_capacity(10);
    data.push(SPL_TOKEN_IX_TRANSFER_CHECKED);
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);
    Ok(Instruction {
        program_id: token_program,
        accounts: vec![
            AccountMeta::new(*source, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    })
}

#[async_trait]
impl Protocol for NftFlip {
    fn name(&self) -> &str {
        "nft_flip"
    }

    async fn execute(&self, rpc: &RpcClient, wallet: &Keypair) -> anyhow::Result<Signature> {
        self.execute_returning_mint(rpc, wallet)
            .await
            .map(|(sig, _mint)| sig)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = NftFlip::from_params(&toml::Table::new()).unwrap();
        assert_eq!(cfg.name, "account-cooker noise NFT");
        assert_eq!(cfg.symbol, "NOISE");
        assert_eq!(cfg.uri, "");
        assert_eq!(cfg.seller_fee_basis_points, 0);
        assert!(cfg.flip_to_sibling);
        assert_eq!(cfg.min_balance_lamports, 25_000_000);
    }

    #[test]
    fn accepts_custom_params() {
        let mut params = toml::Table::new();
        params.insert("name".into(), toml::Value::String("Custom NFT".into()));
        params.insert("symbol".into(), toml::Value::String("CUST".into()));
        params.insert(
            "uri".into(),
            toml::Value::String("https://example.com/meta.json".into()),
        );
        params.insert("seller_fee_basis_points".into(), toml::Value::Integer(250));
        params.insert("flip_to_sibling".into(), toml::Value::Boolean(false));
        let cfg = NftFlip::from_params(&params).unwrap();
        assert_eq!(cfg.name, "Custom NFT");
        assert_eq!(cfg.symbol, "CUST");
        assert_eq!(cfg.uri, "https://example.com/meta.json");
        assert_eq!(cfg.seller_fee_basis_points, 250);
        assert!(!cfg.flip_to_sibling);
    }

    #[test]
    fn rejects_name_too_long() {
        let mut params = toml::Table::new();
        params.insert("name".into(), toml::Value::String("x".repeat(33)));
        assert!(NftFlip::from_params(&params).is_err());
    }

    #[test]
    fn rejects_symbol_too_long() {
        let mut params = toml::Table::new();
        params.insert("symbol".into(), toml::Value::String("x".repeat(11)));
        assert!(NftFlip::from_params(&params).is_err());
    }

    #[test]
    fn rejects_uri_too_long() {
        let mut params = toml::Table::new();
        params.insert("uri".into(), toml::Value::String("x".repeat(201)));
        assert!(NftFlip::from_params(&params).is_err());
    }

    #[test]
    fn rejects_out_of_range_seller_fee_basis_points() {
        let mut params = toml::Table::new();
        params.insert(
            "seller_fee_basis_points".into(),
            toml::Value::Integer(10_001),
        );
        assert!(NftFlip::from_params(&params).is_err());
    }

    #[test]
    fn sibling_pubkey_is_deterministic_per_wallet() {
        let kp = Keypair::new();
        assert_eq!(
            derive_sibling_pubkey(&kp).unwrap(),
            derive_sibling_pubkey(&kp).unwrap()
        );
    }

    #[test]
    fn sibling_pubkey_differs_from_wallet_and_between_wallets() {
        let a = Keypair::new();
        let b = Keypair::new();
        let sibling_a = derive_sibling_pubkey(&a).unwrap();
        let sibling_b = derive_sibling_pubkey(&b).unwrap();
        assert_ne!(sibling_a, a.pubkey());
        assert_ne!(sibling_a, sibling_b);
    }

    /// Confirms this protocol's sibling tag is independent from
    /// `supersonic_cast.rs`'s — same wallet, different tag, must produce a
    /// different sibling address (see module docs).
    #[test]
    fn sibling_tag_is_domain_separated_from_supersonic_cast() {
        let wallet = Keypair::new();
        let mut h = Sha256::new();
        h.update(b"account-cooker/supersonic-cast/master/v1");
        h.update(wallet.to_bytes());
        let supersonic_seed: [u8; 32] = h.finalize().into();
        let supersonic_pubkey = keypair_from_seed(&supersonic_seed).unwrap().pubkey();

        assert_ne!(derive_sibling_pubkey(&wallet).unwrap(), supersonic_pubkey);
    }

    #[test]
    fn find_associated_token_address_is_deterministic() {
        let owner = Keypair::new().pubkey();
        let mint = Keypair::new().pubkey();
        assert_eq!(
            find_associated_token_address(&owner, &mint).unwrap(),
            find_associated_token_address(&owner, &mint).unwrap()
        );
    }

    #[test]
    fn metaplex_pda_derivations_are_deterministic_and_distinct() {
        let mint_a = Keypair::new().pubkey();
        let mint_b = Keypair::new().pubkey();

        let (metadata_a, _) = Metadata::find_pda(&mint_a);
        let (metadata_a_again, _) = Metadata::find_pda(&mint_a);
        assert_eq!(metadata_a, metadata_a_again);

        let (metadata_b, _) = Metadata::find_pda(&mint_b);
        assert_ne!(metadata_a, metadata_b);

        let (master_edition_a, _) = MasterEdition::find_pda(&mint_a);
        assert_ne!(metadata_a, master_edition_a);
    }

    #[test]
    fn transfer_checked_instruction_matches_spl_token_layout() {
        let source = Keypair::new().pubkey();
        let mint = Keypair::new().pubkey();
        let destination = Keypair::new().pubkey();
        let authority = Keypair::new().pubkey();

        let ix = build_transfer_checked_instruction(&source, &mint, &destination, &authority, 1, 0)
            .unwrap();

        assert_eq!(ix.program_id, TOKEN_PROGRAM_ID.parse::<Pubkey>().unwrap());
        assert_eq!(ix.data, vec![12u8, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(ix.accounts.len(), 4);
        assert_eq!(ix.accounts[0].pubkey, source);
        assert!(ix.accounts[0].is_writable && !ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[1].pubkey, mint);
        assert!(!ix.accounts[1].is_writable && !ix.accounts[1].is_signer);
        assert_eq!(ix.accounts[2].pubkey, destination);
        assert!(ix.accounts[2].is_writable && !ix.accounts[2].is_signer);
        assert_eq!(ix.accounts[3].pubkey, authority);
        assert!(!ix.accounts[3].is_writable && ix.accounts[3].is_signer);
    }
}

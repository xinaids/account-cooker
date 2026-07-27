use async_trait::async_trait;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer},
    transaction::Transaction,
};
use solana_system_interface::program as system_program;
use std::str::FromStr;

use super::Protocol;

/// SPL Governance (the program Realms / app.realms.today runs on) — shared
/// instance, same address on mainnet-beta AND devnet. Verified live by this
/// project directly (not trusted from memory or docs): `solana account
/// GovER5Lthms3bLBqWub97yVrMmEogzX7xNjdXpPPCVZw --url <mainnet-beta|devnet>`
/// returns `executable: true`, owner `BPFLoaderUpgradeab1e...`, and identical
/// `ProgramData` account contents on both clusters. Structural, not
/// behavior-relevant (same category as marinade.rs's `MARINADE_PROGRAM_ID`).
const DEFAULT_GOVERNANCE_PROGRAM_ID: &str = "GovER5Lthms3bLBqWub97yVrMmEogzX7xNjdXpPPCVZw";

// PDA seed + instruction/account-layout constants below are taken verbatim
// from solana-labs/solana-program-library, `governance/program/src/`:
//   - `PROGRAM_AUTHORITY_SEED` — lib.rs
//   - `GovernanceInstruction` variant indices (borsh enum tag = first byte,
//     NOT an Anchor-style 8-byte sha256 discriminator) — instruction.rs
//   - `GovernanceAccountType` discriminants, `ProposalState` variants,
//     `ProposalV2` / `GovernanceV2` field order — state/enums.rs,
//     state/proposal.rs, state/governance.rs
//   - `get_realm_config_address_seeds` — state/realm_config.rs
// Field offsets below were independently confirmed against REAL live mainnet
// accounts during development (a `ProposalV2` and a `GovernanceV2` fetched
// via RPC decoded with exactly the expected discriminant bytes at every
// offset used here) — see README.md's dao_vote proof section.
const PROGRAM_AUTHORITY_SEED: &[u8] = b"governance";
const REALM_CONFIG_SEED: &[u8] = b"realm-config";

const IX_CREATE_TOKEN_OWNER_RECORD: u8 = 23;
const IX_CAST_VOTE: u8 = 13;

const ACCOUNT_TYPE_PROPOSAL_V2: u8 = 14;
const ACCOUNT_TYPE_TOKEN_OWNER_RECORD_V2: u8 = 17;
const ACCOUNT_TYPE_GOVERNANCE_V2: u8 = 18;

/// `ProposalState::Voting` — the only state `CastVote` accepts
/// (`assert_is_voting_state` in state/proposal.rs). Declaration order:
/// Draft=0, SigningOff=1, Voting=2, Succeeded=3, Executing=4, Completed=5,
/// Cancelled=6, Defeated=7, ExecutingWithErrors=8, Vetoed=9.
const PROPOSAL_STATE_VOTING: u8 = 2;

/// Casts a vote on an SPL Governance (Realms) proposal.
///
/// # Why this is safe to run against a REAL, third-party DAO's live proposal
///
/// `CreateTokenOwnerRecord` (instruction 23) is permissionless — verified
/// directly in `process_create_token_owner_record.rs`: it only checks the
/// mint is valid for the realm, never that the caller holds any of it, and
/// always sets `governing_token_deposit_amount: 0`. `CastVote`
/// (`process_cast_vote.rs`) never checks that the resolved voter weight is
/// nonzero before recording a full on-chain `VoteRecordV2` and adding the
/// weight to the tally via `checked_add`. So a wallet that has never held or
/// deposited a single unit of a DAO's governing token can create its own
/// zero-balance `TokenOwnerRecord` in that DAO's realm and cast a fully real,
/// on-chain vote that is *mathematically* incapable of changing the
/// outcome — not "low impact", exactly zero impact, by construction of the
/// program itself. This protocol NEVER deposits governing tokens on the
/// wallet's behalf (no `DepositGoverningTokens` instruction anywhere here).
/// If the wallet already holds a `TokenOwnerRecord` with real deposited
/// weight (because the operator independently deposited tokens themselves,
/// outside this protocol), a cast vote would otherwise carry that real
/// weight — `build_and_simulate` checks this explicitly
/// (`parse_token_owner_record_deposit_amount`) and refuses to vote rather
/// than silently casting a weighted one; see THREAT_MODEL.md.
///
/// # Scope limitation
///
/// Only realms using the default vote-weight source (deposited governing SPL
/// tokens, no plugin) are supported — `voter_weight_record` /
/// `max_voter_weight_record` are always omitted. A realm gated by a
/// voter-weight addin (NFT voting, Civic gateway, etc.) will fail simulation
/// with a clear on-chain error rather than silently misbehave.
pub struct DaoVote {
    governance_program_id: Pubkey,
    proposal: Pubkey,
    vote_choice: VoteChoice,
    /// Minimum wallet balance required to attempt a vote — covers rent for
    /// up to two new accounts (`TokenOwnerRecord`, `VoteRecord`) plus fees.
    /// Same "skip this tick rather than send a doomed tx" guard as
    /// `min_swap_lamports` / `min_stake_lamports` in the other protocols.
    min_balance_lamports: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoteChoice {
    Approve,
    Deny,
    Abstain,
    Veto,
}

impl VoteChoice {
    fn parse(s: &str) -> anyhow::Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "approve" => Ok(Self::Approve),
            "deny" => Ok(Self::Deny),
            "abstain" => Ok(Self::Abstain),
            "veto" => Ok(Self::Veto),
            other => anyhow::bail!(
                "dao_vote.vote_choice must be one of approve|deny|abstain|veto, got {other:?}"
            ),
        }
    }

    /// Borsh encoding of spl-governance's `Vote` enum (state/vote_record.rs):
    /// `Approve(Vec<VoteChoice>) | Deny | Abstain | Veto`, tags 0..=3 in that
    /// declaration order. `Approve` always casts a single full-weight choice
    /// (`VoteChoice { rank: 0, weight_percentage: 100 }`), the standard shape
    /// for a simple single-choice yes/no proposal.
    fn encode(self) -> Vec<u8> {
        match self {
            Self::Approve => {
                let mut v = vec![0u8]; // Vote::Approve tag
                v.extend_from_slice(&1u32.to_le_bytes()); // Vec<VoteChoice> len = 1
                v.push(0); // VoteChoice.rank
                v.push(100); // VoteChoice.weight_percentage
                v
            }
            Self::Deny => vec![1u8],
            Self::Abstain => vec![2u8],
            Self::Veto => vec![3u8],
        }
    }
}

impl DaoVote {
    pub fn from_params(params: &toml::Table) -> anyhow::Result<Self> {
        let governance_program_id = params
            .get("governance_program_id")
            .and_then(|v| v.as_str())
            .map(Pubkey::from_str)
            .transpose()
            .map_err(|e| anyhow::anyhow!("dao_vote.governance_program_id: {e}"))?
            .unwrap_or_else(|| {
                Pubkey::from_str(DEFAULT_GOVERNANCE_PROGRAM_ID).expect("valid hardcoded program id")
            });

        // No sensible default exists — unlike a mint list or slippage
        // tolerance, a specific proposal is inherently a one-off choice the
        // operator must name explicitly.
        let proposal = params
            .get("proposal_pubkey")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "dao_vote.proposal_pubkey is required (no default — must name a specific proposal)"
                )
            })
            .and_then(|s| {
                Pubkey::from_str(s).map_err(|e| anyhow::anyhow!("dao_vote.proposal_pubkey: {e}"))
            })?;

        let vote_choice = match params.get("vote_choice").and_then(|v| v.as_str()) {
            Some(s) => VoteChoice::parse(s)?,
            // Conservative default: an agent given no explicit instruction
            // expresses no opinion. Weight is 0 regardless (see module docs),
            // but Abstain is the honest choice absent operator intent.
            None => VoteChoice::Abstain,
        };

        let min_balance_lamports = params
            .get("min_balance_lamports")
            .and_then(|v| v.as_integer())
            .unwrap_or(5_000_000) as u64; // 0.005 SOL default floor

        Ok(Self {
            governance_program_id,
            proposal,
            vote_choice,
            min_balance_lamports,
        })
    }

    /// Fetches the proposal + governance accounts, derives every PDA, builds
    /// the (up to two) instructions, and simulates the resulting transaction —
    /// everything `execute()` needs before it's safe to send. Shared by
    /// `execute()` (which sends afterward) and `simulate()` (which doesn't),
    /// so the two can never drift apart.
    async fn build_and_simulate(
        &self,
        rpc: &RpcClient,
        wallet: &Keypair,
    ) -> anyhow::Result<Transaction> {
        let balance = rpc.get_balance(&wallet.pubkey()).await?;
        if balance < self.min_balance_lamports {
            anyhow::bail!(
                "wallet balance too low ({balance} lamports) to safely cover TokenOwnerRecord/VoteRecord rent + fees, skipping this tick"
            );
        }

        let proposal_account = rpc.get_account(&self.proposal).await.map_err(|e| {
            anyhow::anyhow!("failed to fetch proposal account {}: {e}", self.proposal)
        })?;
        if proposal_account.owner != self.governance_program_id {
            anyhow::bail!(
                "proposal account {} is not owned by the configured governance program {} (owned by {})",
                self.proposal,
                self.governance_program_id,
                proposal_account.owner
            );
        }
        let proposal_fields = parse_proposal_account(&proposal_account.data)?;
        if proposal_fields.state != PROPOSAL_STATE_VOTING {
            anyhow::bail!(
                "proposal {} is not in Voting state (state={}), cannot cast vote",
                self.proposal,
                proposal_fields.state
            );
        }

        let governance_account =
            rpc.get_account(&proposal_fields.governance)
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "failed to fetch governance account {}: {e}",
                        proposal_fields.governance
                    )
                })?;
        if governance_account.owner != self.governance_program_id {
            anyhow::bail!(
                "governance account {} is not owned by the configured governance program {} (owned by {})",
                proposal_fields.governance,
                self.governance_program_id,
                governance_account.owner
            );
        }
        let realm = parse_governance_account(&governance_account.data)?;

        let our_token_owner_record = token_owner_record_address(
            &self.governance_program_id,
            &realm,
            &proposal_fields.governing_token_mint,
            &wallet.pubkey(),
        );
        let vote_record = vote_record_address(
            &self.governance_program_id,
            &self.proposal,
            &our_token_owner_record,
        );
        let realm_config = realm_config_address(&self.governance_program_id, &realm);

        // A second CastVote onto an existing VoteRecord fails at account
        // creation on-chain (the PDA is already in use) — checking first
        // gives a clear, specific error instead of a generic simulation
        // failure, same rationale as jupiter.rs's validate_swap_transaction.
        if rpc.get_account(&vote_record).await.is_ok() {
            anyhow::bail!(
                "wallet {} already cast a vote on proposal {} (vote_record {vote_record} exists) — \
                 relinquish first to change it, which this protocol does not implement",
                wallet.pubkey(),
                self.proposal
            );
        }

        let mut ixs = Vec::new();

        // Create the wallet's own TokenOwnerRecord if it doesn't exist yet —
        // permissionless, always zero-balance (see module docs). Mirrors
        // marinade.rs's create-ATA-if-needed pattern. If one already exists,
        // the "mathematically zero weight" guarantee only holds if it's
        // still zero-balance — verify that precondition here instead of
        // silently trusting it (a pre-existing nonzero-balance record could
        // only come from the operator depositing tokens outside this
        // protocol, but this code shouldn't vote through it without saying
        // so first).
        match rpc.get_account(&our_token_owner_record).await {
            Err(_) => {
                ixs.push(build_create_token_owner_record_instruction(
                    &self.governance_program_id,
                    &realm,
                    &wallet.pubkey(),
                    &proposal_fields.governing_token_mint,
                    &wallet.pubkey(),
                    &our_token_owner_record,
                ));
            }
            Ok(existing) => {
                let deposit_amount = parse_token_owner_record_deposit_amount(&existing.data)?;
                if deposit_amount != 0 {
                    anyhow::bail!(
                        "wallet {} already holds a TokenOwnerRecord ({our_token_owner_record}) in \
                         this realm with a nonzero deposit ({deposit_amount} governing tokens) — \
                         voting would carry real weight, not the zero-weight guarantee this \
                         protocol is built on. This protocol never deposits tokens itself, so \
                         this balance came from outside it (see THREAT_MODEL.md's \"DAO governance \
                         voting\" section) — refusing to vote rather than silently casting a \
                         weighted vote",
                        wallet.pubkey()
                    );
                }
            }
        }

        ixs.push(build_cast_vote_instruction(
            &self.governance_program_id,
            &realm,
            &proposal_fields.governance,
            &self.proposal,
            &proposal_fields.proposal_owner_record,
            &our_token_owner_record,
            &wallet.pubkey(),
            &vote_record,
            &proposal_fields.governing_token_mint,
            &wallet.pubkey(),
            &realm_config,
            self.vote_choice,
        ));

        let recent_blockhash = rpc.get_latest_blockhash().await?;
        let tx = Transaction::new_signed_with_payer(
            &ixs,
            Some(&wallet.pubkey()),
            &[wallet],
            recent_blockhash,
        );

        // Simulate first — same pattern as jupiter.rs / marinade.rs /
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
            anyhow::bail!("dao_vote cast_vote simulation failed: {err:?}\nlogs:\n{logs}");
        }

        Ok(tx)
    }

    /// Builds and simulates the exact transaction `execute()` would send,
    /// without ever sending it — a non-destructive dry run for operators
    /// (and this crate's own proof-gathering) to verify a `proposal_pubkey`
    /// resolves cleanly before opting in for real. Returns `Ok(())` on a
    /// clean simulation, `Err` with full logs otherwise — never sends,
    /// regardless of outcome.
    pub async fn simulate(&self, rpc: &RpcClient, wallet: &Keypair) -> anyhow::Result<()> {
        self.build_and_simulate(rpc, wallet).await.map(|_tx| ())
    }
}

/// Fields read out of a live `ProposalV2` account, at fixed byte offsets
/// (see module doc for provenance). Only the fields this protocol actually
/// needs are parsed — `signatories_count` onward (vote_type, options,
/// timestamps, ...) are never read.
struct ProposalFields {
    governance: Pubkey,
    governing_token_mint: Pubkey,
    state: u8,
    /// The TokenOwnerRecord of whoever CREATED the proposal (a required
    /// `CastVote` account) — NOT the voter's own record.
    proposal_owner_record: Pubkey,
}

/// Layout (Borsh, in order): account_type(1) + governance(32) +
/// governing_token_mint(32) + state(1) + token_owner_record(32) + ... —
/// see state/proposal.rs's `ProposalV2` struct. All fields read here precede
/// every variable-length field (`vote_type`, `options`, `name`, ...) in
/// declaration order, so fixed offsets are safe without parsing those.
fn parse_proposal_account(data: &[u8]) -> anyhow::Result<ProposalFields> {
    let field = data
        .get(0..98)
        .ok_or_else(|| anyhow::anyhow!("proposal account data too short ({} bytes)", data.len()))?;
    if field[0] != ACCOUNT_TYPE_PROPOSAL_V2 {
        anyhow::bail!(
            "account is not a ProposalV2 (account_type={}, expected {ACCOUNT_TYPE_PROPOSAL_V2}) — \
             legacy ProposalV1 accounts are not supported",
            field[0]
        );
    }
    let governance = pubkey_from_slice(&field[1..33])?;
    let governing_token_mint = pubkey_from_slice(&field[33..65])?;
    let state = field[65];
    let proposal_owner_record = pubkey_from_slice(&field[66..98])?;
    Ok(ProposalFields {
        governance,
        governing_token_mint,
        state,
        proposal_owner_record,
    })
}

/// Layout: account_type(1) + realm(32) + ... — see state/governance.rs's
/// `GovernanceV2` struct. Only `realm` (needed to derive PDAs and to pass as
/// the `realm` account) is read.
fn parse_governance_account(data: &[u8]) -> anyhow::Result<Pubkey> {
    let field = data.get(0..33).ok_or_else(|| {
        anyhow::anyhow!("governance account data too short ({} bytes)", data.len())
    })?;
    if field[0] != ACCOUNT_TYPE_GOVERNANCE_V2 {
        anyhow::bail!(
            "account is not a GovernanceV2 (account_type={}, expected {ACCOUNT_TYPE_GOVERNANCE_V2}) — \
             legacy GovernanceV1 accounts are not supported",
            field[0]
        );
    }
    pubkey_from_slice(&field[1..33])
}

/// Layout: account_type(1) + realm(32) + governing_token_mint(32) +
/// governing_token_owner(32) + governing_token_deposit_amount(8) + ... —
/// see state/token_owner_record.rs's `TokenOwnerRecordV2` struct. Only
/// `governing_token_deposit_amount` is read, to verify a pre-existing record
/// is still zero-balance before voting through it (see call site).
fn parse_token_owner_record_deposit_amount(data: &[u8]) -> anyhow::Result<u64> {
    let field = data.get(0..105).ok_or_else(|| {
        anyhow::anyhow!("token owner record data too short ({} bytes)", data.len())
    })?;
    if field[0] != ACCOUNT_TYPE_TOKEN_OWNER_RECORD_V2 {
        anyhow::bail!(
            "account is not a TokenOwnerRecordV2 (account_type={}, expected {ACCOUNT_TYPE_TOKEN_OWNER_RECORD_V2}) — \
             legacy TokenOwnerRecordV1 accounts are not supported",
            field[0]
        );
    }
    let amount_bytes: [u8; 8] = field[97..105]
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected exactly 8 bytes for deposit_amount"))?;
    Ok(u64::from_le_bytes(amount_bytes))
}

fn pubkey_from_slice(bytes: &[u8]) -> anyhow::Result<Pubkey> {
    let arr: [u8; 32] = bytes.try_into().map_err(|_| {
        anyhow::anyhow!(
            "expected exactly 32 bytes for a pubkey, got {}",
            bytes.len()
        )
    })?;
    Ok(Pubkey::new_from_array(arr))
}

/// `TokenOwnerRecord` PDA — seeds from `get_token_owner_record_address_seeds`
/// (state/token_owner_record.rs): `[PROGRAM_AUTHORITY_SEED, realm,
/// governing_token_mint, governing_token_owner]`.
fn token_owner_record_address(
    program_id: &Pubkey,
    realm: &Pubkey,
    governing_token_mint: &Pubkey,
    governing_token_owner: &Pubkey,
) -> Pubkey {
    Pubkey::find_program_address(
        &[
            PROGRAM_AUTHORITY_SEED,
            realm.as_ref(),
            governing_token_mint.as_ref(),
            governing_token_owner.as_ref(),
        ],
        program_id,
    )
    .0
}

/// `VoteRecord` PDA — seeds from `get_vote_record_address_seeds`
/// (state/vote_record.rs): `[PROGRAM_AUTHORITY_SEED, proposal,
/// token_owner_record]`.
fn vote_record_address(
    program_id: &Pubkey,
    proposal: &Pubkey,
    token_owner_record: &Pubkey,
) -> Pubkey {
    Pubkey::find_program_address(
        &[
            PROGRAM_AUTHORITY_SEED,
            proposal.as_ref(),
            token_owner_record.as_ref(),
        ],
        program_id,
    )
    .0
}

/// `RealmConfigAccount` PDA — seeds from `get_realm_config_address_seeds`
/// (state/realm_config.rs): `[b"realm-config", realm]`. Required on every
/// `CastVote` call (`with_realm_config_accounts`), not just plugin-gated
/// realms.
fn realm_config_address(program_id: &Pubkey, realm: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[REALM_CONFIG_SEED, realm.as_ref()], program_id).0
}

/// `GovernanceInstruction::CreateTokenOwnerRecord {}` — instruction.rs's
/// `create_token_owner_record()` builder, account order verbatim.
/// Permissionless: `governing_token_owner` is NOT a signer (see module docs).
fn build_create_token_owner_record_instruction(
    program_id: &Pubkey,
    realm: &Pubkey,
    governing_token_owner: &Pubkey,
    governing_token_mint: &Pubkey,
    payer: &Pubkey,
    token_owner_record: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new_readonly(*realm, false),
            AccountMeta::new_readonly(*governing_token_owner, false),
            AccountMeta::new(*token_owner_record, false),
            AccountMeta::new_readonly(*governing_token_mint, false),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        data: vec![IX_CREATE_TOKEN_OWNER_RECORD],
    }
}

/// `GovernanceInstruction::CastVote { vote }` — instruction.rs's
/// `cast_vote()` builder, account order verbatim (including the always-
/// present `realm_config` account from `with_realm_config_accounts`;
/// `voter_weight_record` / `max_voter_weight_record` omitted — see module
/// doc "Scope limitation").
#[allow(clippy::too_many_arguments)]
fn build_cast_vote_instruction(
    program_id: &Pubkey,
    realm: &Pubkey,
    governance: &Pubkey,
    proposal: &Pubkey,
    proposal_owner_record: &Pubkey,
    voter_token_owner_record: &Pubkey,
    governance_authority: &Pubkey,
    vote_record: &Pubkey,
    vote_governing_token_mint: &Pubkey,
    payer: &Pubkey,
    realm_config: &Pubkey,
    vote_choice: VoteChoice,
) -> Instruction {
    let mut data = vec![IX_CAST_VOTE];
    data.extend(vote_choice.encode());

    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new_readonly(*realm, false),
            AccountMeta::new(*governance, false),
            AccountMeta::new(*proposal, false),
            AccountMeta::new(*proposal_owner_record, false),
            AccountMeta::new(*voter_token_owner_record, false),
            AccountMeta::new_readonly(*governance_authority, true),
            AccountMeta::new(*vote_record, false),
            AccountMeta::new_readonly(*vote_governing_token_mint, false),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(*realm_config, false),
        ],
        data,
    }
}

#[async_trait]
impl Protocol for DaoVote {
    fn name(&self) -> &str {
        "dao_vote"
    }

    async fn execute(&self, rpc: &RpcClient, wallet: &Keypair) -> anyhow::Result<Signature> {
        let tx = self.build_and_simulate(rpc, wallet).await?;
        match rpc.send_and_confirm_transaction(&tx).await {
            Ok(sig) => Ok(sig),
            Err(e) => anyhow::bail!("dao_vote cast_vote send/confirm failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let mut params = toml::Table::new();
        params.insert(
            "proposal_pubkey".into(),
            toml::Value::String(Keypair::new().pubkey().to_string()),
        );
        let cfg = DaoVote::from_params(&params).unwrap();
        assert_eq!(
            cfg.governance_program_id,
            Pubkey::from_str(DEFAULT_GOVERNANCE_PROGRAM_ID).unwrap()
        );
        assert_eq!(cfg.vote_choice, VoteChoice::Abstain);
        assert_eq!(cfg.min_balance_lamports, 5_000_000);
    }

    #[test]
    fn rejects_missing_proposal_pubkey() {
        let params = toml::Table::new();
        assert!(DaoVote::from_params(&params).is_err());
    }

    #[test]
    fn rejects_invalid_vote_choice() {
        let mut params = toml::Table::new();
        params.insert(
            "proposal_pubkey".into(),
            toml::Value::String(Keypair::new().pubkey().to_string()),
        );
        params.insert("vote_choice".into(), toml::Value::String("yolo".into()));
        assert!(DaoVote::from_params(&params).is_err());
    }

    #[test]
    fn accepts_custom_governance_program_id_and_vote_choice() {
        let custom = Keypair::new().pubkey();
        let mut params = toml::Table::new();
        params.insert(
            "proposal_pubkey".into(),
            toml::Value::String(Keypair::new().pubkey().to_string()),
        );
        params.insert(
            "governance_program_id".into(),
            toml::Value::String(custom.to_string()),
        );
        params.insert("vote_choice".into(), toml::Value::String("Approve".into()));
        let cfg = DaoVote::from_params(&params).unwrap();
        assert_eq!(cfg.governance_program_id, custom);
        assert_eq!(cfg.vote_choice, VoteChoice::Approve);
    }

    /// Builds a synthetic 98-byte buffer matching `ProposalV2`'s real Borsh
    /// layout (see `parse_proposal_account`'s doc comment) so the offset
    /// math is regression-tested without needing a live RPC call.
    fn synthetic_proposal_bytes(
        account_type: u8,
        governance: Pubkey,
        governing_token_mint: Pubkey,
        state: u8,
        proposal_owner_record: Pubkey,
    ) -> Vec<u8> {
        let mut buf = Vec::with_capacity(98);
        buf.push(account_type);
        buf.extend_from_slice(governance.as_ref());
        buf.extend_from_slice(governing_token_mint.as_ref());
        buf.push(state);
        buf.extend_from_slice(proposal_owner_record.as_ref());
        buf
    }

    #[test]
    fn parse_proposal_account_extracts_fields_correctly() {
        let governance = Keypair::new().pubkey();
        let mint = Keypair::new().pubkey();
        let owner_record = Keypair::new().pubkey();
        let buf = synthetic_proposal_bytes(
            ACCOUNT_TYPE_PROPOSAL_V2,
            governance,
            mint,
            PROPOSAL_STATE_VOTING,
            owner_record,
        );
        let fields = parse_proposal_account(&buf).unwrap();
        assert_eq!(fields.governance, governance);
        assert_eq!(fields.governing_token_mint, mint);
        assert_eq!(fields.state, PROPOSAL_STATE_VOTING);
        assert_eq!(fields.proposal_owner_record, owner_record);
    }

    #[test]
    fn parse_proposal_account_rejects_wrong_discriminant() {
        let buf = synthetic_proposal_bytes(
            5, // ProposalV1
            Keypair::new().pubkey(),
            Keypair::new().pubkey(),
            PROPOSAL_STATE_VOTING,
            Keypair::new().pubkey(),
        );
        assert!(parse_proposal_account(&buf).is_err());
    }

    #[test]
    fn parse_proposal_account_rejects_short_buffer() {
        assert!(parse_proposal_account(&[0u8; 50]).is_err());
    }

    #[test]
    fn parse_governance_account_extracts_realm() {
        let realm = Keypair::new().pubkey();
        let mut buf = vec![ACCOUNT_TYPE_GOVERNANCE_V2];
        buf.extend_from_slice(realm.as_ref());
        assert_eq!(parse_governance_account(&buf).unwrap(), realm);
    }

    #[test]
    fn parse_governance_account_rejects_wrong_discriminant() {
        let realm = Keypair::new().pubkey();
        let mut buf = vec![3u8]; // GovernanceV1
        buf.extend_from_slice(realm.as_ref());
        assert!(parse_governance_account(&buf).is_err());
    }

    /// Builds a synthetic 105-byte buffer matching `TokenOwnerRecordV2`'s
    /// real Borsh layout up through `governing_token_deposit_amount` (see
    /// `parse_token_owner_record_deposit_amount`'s doc comment).
    fn synthetic_token_owner_record_bytes(account_type: u8, deposit_amount: u64) -> Vec<u8> {
        let mut buf = Vec::with_capacity(105);
        buf.push(account_type);
        buf.extend_from_slice(Keypair::new().pubkey().as_ref()); // realm
        buf.extend_from_slice(Keypair::new().pubkey().as_ref()); // governing_token_mint
        buf.extend_from_slice(Keypair::new().pubkey().as_ref()); // governing_token_owner
        buf.extend_from_slice(&deposit_amount.to_le_bytes());
        buf
    }

    #[test]
    fn parse_token_owner_record_deposit_amount_extracts_zero() {
        let buf = synthetic_token_owner_record_bytes(ACCOUNT_TYPE_TOKEN_OWNER_RECORD_V2, 0);
        assert_eq!(parse_token_owner_record_deposit_amount(&buf).unwrap(), 0);
    }

    #[test]
    fn parse_token_owner_record_deposit_amount_extracts_nonzero() {
        let buf = synthetic_token_owner_record_bytes(ACCOUNT_TYPE_TOKEN_OWNER_RECORD_V2, 42_000);
        assert_eq!(
            parse_token_owner_record_deposit_amount(&buf).unwrap(),
            42_000
        );
    }

    #[test]
    fn parse_token_owner_record_deposit_amount_rejects_wrong_discriminant() {
        let buf = synthetic_token_owner_record_bytes(2, 0); // TokenOwnerRecordV1
        assert!(parse_token_owner_record_deposit_amount(&buf).is_err());
    }

    #[test]
    fn parse_token_owner_record_deposit_amount_rejects_short_buffer() {
        assert!(parse_token_owner_record_deposit_amount(&[0u8; 50]).is_err());
    }

    #[test]
    fn vote_choice_encode_matches_expected_bytes() {
        assert_eq!(
            VoteChoice::Approve.encode(),
            vec![0u8, 1, 0, 0, 0, /* rank */ 0, /* weight_percentage */ 100]
        );
        assert_eq!(VoteChoice::Deny.encode(), vec![1u8]);
        assert_eq!(VoteChoice::Abstain.encode(), vec![2u8]);
        assert_eq!(VoteChoice::Veto.encode(), vec![3u8]);
    }

    #[test]
    fn pda_derivations_are_deterministic() {
        let program_id = Pubkey::from_str(DEFAULT_GOVERNANCE_PROGRAM_ID).unwrap();
        let realm = Keypair::new().pubkey();
        let mint = Keypair::new().pubkey();
        let owner = Keypair::new().pubkey();

        let tor_a = token_owner_record_address(&program_id, &realm, &mint, &owner);
        let tor_b = token_owner_record_address(&program_id, &realm, &mint, &owner);
        assert_eq!(tor_a, tor_b);

        let proposal = Keypair::new().pubkey();
        let vr_a = vote_record_address(&program_id, &proposal, &tor_a);
        let vr_b = vote_record_address(&program_id, &proposal, &tor_a);
        assert_eq!(vr_a, vr_b);
        assert_ne!(vr_a, tor_a);

        let rc_a = realm_config_address(&program_id, &realm);
        let rc_b = realm_config_address(&program_id, &realm);
        assert_eq!(rc_a, rc_b);
    }
}

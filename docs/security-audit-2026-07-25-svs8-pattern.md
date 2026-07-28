# account-cooker Security Audit — Targeted Pattern Hunt (account substitution)

**Date**: 2026-07-25
**Scope**: A single vulnerability *class*, hunted across every protocol that sends
real transactions: **an account is accepted and used in a CPI/instruction, or in
a balance/value calculation, without validating it is exactly the expected
account (owner, PDA, discriminator)** — allowing account substitution (someone
else's account, an API-supplied account, an unaudited-SDK-constructed account)
to inflate a computed value, redirect funds, or bypass a check. This is the
pattern named in the audit request: the P0 class most consistently found by
`@kauenet` in Superteam BR bounty reviews (cf. Solana Vault Standard PR #44/#38,
the `redeem_single` finding referenced below).
Files in scope: `src/protocols/jupiter.rs`, `marinade.rs`, `dao_vote.rs`,
`nft_flip.rs`, `supersonic_cast.rs`, `src/consolidation.rs`.
**Not** re-auditing findings already reported in
[`security-audit-2026-07-20.md`](./security-audit-2026-07-20.md) (jupiter,
marinade, supersonic_cast, state.rs) or
[`security-audit-2026-07-23.md`](./security-audit-2026-07-23.md) (consolidation,
clustering) except where this specific pattern re-opens or re-contextualizes a
previously-known gap — those cases are explicitly cross-referenced, not
duplicated.
**Auditor**: claude (read-only pass, no fixes applied per instruction)
**Commit**: `1d6af5f` (branch `feat/jupiter-swap-scaffold`), working tree clean
at audit start (only an untracked, unrelated comparison doc present)

## Summary

| Severity | Count |
|---|---|
| Critical | 0 |
| High | 1 |
| Medium | 1 |
| Low | 1 |

One real, actionable gap (High) on the highest-weighted, default-enabled
protocol; one defense-in-depth gap (Medium) on a protocol that already carries
an accepted, separately-tracked supply-chain caveat; one low-severity
error-handling nit (Low) that already fails closed today. **No Critical
findings** — nothing in this pass lets an external party redirect funds to an
address of their own choosing without the wallet owner's cooperation. The
codebase's general discipline on this exact pattern is notably strong in the
two files with the largest external-account surface (`dao_vote.rs` fetches and
parses a real third party's on-chain accounts; `consolidation.rs` moves funds
between wallets) — see "What's already right" below.

## How each file was checked

For every file, the same four questions from the audit request were applied
line-by-line to every account that ends up in an `Instruction`'s `accounts`
list or in a lamport/balance calculation:

1. Is this account's identity derived locally (PDA/`find_program_address`,
   `create_with_seed`, or a hardcoded verified constant) or independently
   validated (owner check, discriminator check) before use — or is it accepted
   at face value from a parameter, an external HTTP response, or another
   crate's output?
2. Does every balance/value computation read the account it claims to read?
3. Does every fund-moving instruction's destination resolve to what the
   protocol actually intends (the wallet's own derived sibling/ATA, or an
   explicit config value), with no path where an unvalidated value becomes the
   destination or signer?
4. Is any `Result`/`Option` collapsed into an assumed-safe default (`unwrap_or`,
   blanket `Err(_) => `) in a way that could mask a real error rather than
   fail explicitly?

---

## What's already right (verified, not assumed)

- **`marinade.rs` has no external-account attack surface at all.** Every
  account in `build_deposit_instruction` — `state`, `msol_mint`,
  `liq_pool_sol_leg_pda`, `liq_pool_msol_leg`, `liq_pool_msol_leg_authority`,
  `reserve_pda`, `msol_mint_authority` — is either a hardcoded, live-verified
  constant or a PDA/`create_with_seed` address derived locally from that
  constant (`MarinadeAccounts::derive()`). `mint_to` is the wallet's own ATA,
  derived from the wallet's own pubkey. Nothing here is a parameter, an API
  response, or `remaining_accounts`-equivalent input. There is structurally no
  account for an attacker to substitute.
- **`nft_flip.rs`'s mint can never be an existing/third-party asset**, verified
  by reading the full file, not just its own doc-comment claim: the mint is
  `Keypair::new()`, freshly generated inside `build_and_simulate` on every
  call and passed to `CreateV1` as a signer — there is no code path anywhere
  in the file that reads or accepts an existing mint from config or chain.
  The `flip_to_sibling` destination (`derive_sibling_pubkey`) is deterministically
  derived from `Sha256(SIBLING_TAG || wallet_secret_bytes)`, domain-separated
  from `supersonic_cast.rs`'s tag (confirmed by the file's own
  `sibling_tag_is_domain_separated_from_supersonic_cast` test, and re-checked
  by hand). No config field in this file names an external address at all —
  unlike `dao_vote.rs`/`supersonic_cast.rs`, there is no way to even
  *misconfigure* this protocol into touching a third party.
- **`consolidation.rs`'s destination pool is closed and traced end-to-end.**
  Followed `wallets: Vec<FleetWallet>` back to its only construction site
  (`scheduler/mod.rs:46-56`): built exclusively from `cfg.wallets` — the
  operator's own `cooker.toml` wallet list — via `read_keypair_file` on local
  paths. No network input, no remaining-accounts-style surface, feeds into
  `pick_source_destination`. Indexing was re-checked for consistency: `balances`
  is built in the same order as `wallets`; `eligible` and `pick_source_destination`
  both operate on indices into that same order; `compute_transfer_lamports`
  reads `balances[source_idx]` — no mismatched-index read of one wallet's
  balance feeding a transfer built against a different wallet's keys. This
  reconfirms (rather than duplicates) the 07-23 audit's source≠destination
  finding, now specifically checked against the "is the candidate pool itself
  poisonable" question.
- **`dao_vote.rs` applies the correct owner-check-before-parse discipline
  everywhere it touches a real third party's account**, which is what makes it
  safe to read embedded pubkeys out of untrusted-shaped-but-owner-verified
  data. Traced the full trust chain by hand:
  - `proposal_account.owner != self.governance_program_id` is checked
    **before** `parse_proposal_account` ever reads `governance`,
    `governing_token_mint`, or `proposal_owner_record` out of its bytes
    (`dao_vote.rs:201-212`). Since only the owning program can ever write an
    account's data on Solana, once ownership is confirmed, those embedded
    pubkeys are exactly what the real governance program wrote at proposal
    creation — not attacker-substitutable — the discriminator check
    (`ACCOUNT_TYPE_PROPOSAL_V2`) additionally guards against reading a
    same-program-owned-but-differently-shaped account.
  - Same pattern for the governance account before `realm` is trusted
    (`:221-238`), with its own discriminator check
    (`ACCOUNT_TYPE_GOVERNANCE_V2`).
  - The two PDAs that matter most for authority — `our_token_owner_record`
    and `vote_record` — are **not** accepted as input at all; they're derived
    locally via `find_program_address` seeded with `wallet.pubkey()`
    (`token_owner_record_address`) so the "voter" record used is
    cryptographically tied to this wallet, never a parameter.
  - Cross-checked what happens if `parse_proposal_account`'s byte offsets were
    ever wrong (a correctness bug, not an attacker action): a shifted read
    could only ever produce another governance-program-owned pubkey from
    elsewhere in the same account (still passes the owner check) or something
    that fails the owner check outright — either way, the real on-chain
    `CastVote` processor independently re-validates the realm/governance/
    proposal/mint relationship itself, so this class of bug fails closed at
    `simulate_transaction`, before send. No live exploit path found.
  - This is a meaningfully higher bar than the minimum: the file interacts
    with a real third party's live governance program specifically *because*
    it does this correctly.

---

## High

### H-1 (reopened aspect of `security-audit-2026-07-20.md` H-1): `jupiter_swap`'s post-fix validation still lets a malicious API response grant token delegate authority to an attacker address

**File**: `src/protocols/jupiter.rs:180-196` (`validate_swap_transaction` at
`:208-230`)

The 07-20 audit's H-1 was fixed by adding `validate_swap_transaction` (checks
`num_required_signatures == 1` and that the fee payer is this wallet) plus a
pre-send `simulate_transaction` call. That fix was explicitly scoped — the
07-20 report itself records the decision: *"a scoped version of recommendation
2 (fee-payer/signer-count check rather than a full program-id instruction
allowlist, which the user judged sufficient)."* Re-examining that same
residual gap specifically through the account-substitution/drain lens this
audit was asked to hunt for — because it is a live, reachable instance of
exactly that class, not a new discovery:

```rust
let tx_bytes = base64_decode(&swap.swap_transaction)?;
let mut tx: VersionedTransaction = bincode::deserialize(&tx_bytes)?;
validate_swap_transaction(&tx, &wallet.pubkey())?;   // signer-count + fee-payer only
tx.signatures[0] = wallet.sign_message(&tx.message.serialize());
let sim = rpc.simulate_transaction(&tx).await?;       // only checks for an *error*
```

`validate_swap_transaction` confirms the wallet is the sole required signer
and fee payer. It does **not** check what the transaction's *other*
instructions or accounts are. Because the wallet is already in the
transaction's signer set (as fee payer), any instruction that only needs the
wallet's own authority — most concretely SPL Token's `Approve` — adds **no**
new required signer, so it passes `num_required_signatures == 1` unchanged.
`simulate_transaction` only reports whether the transaction *errors*; a
well-formed `Approve` granting delegate authority over one of the wallet's own
token accounts to an arbitrary address *succeeds*, so `sim.value.err` is
`None` and the transaction is sent.

**Failure scenario**: `lite-api.jup.ag` is compromised, DNS-hijacked, or
fronted by a malicious mirror (same threat model the original H-1 already
named). The returned `swapTransaction` contains the legitimate swap route
**plus** an unrelated `Approve` instruction on the wallet's own source-token
ATA, delegating a large or unlimited (`u64::MAX`) amount to an
attacker-controlled address — the delegated amount is independent of the
swap's own size, so even the smallest noise-sized swap this tool ever sends
can carry a delegation with no cap. `account-cooker` signs and sends it: both
current checks pass, and simulation reports success. In a **second,
attacker-initiated** transaction that never touches this wallet again, the
attacker calls `Transfer` as the now-approved delegate and drains the ATA up
to the delegated amount. This is the standard "malicious approve" drain
pattern (the same shape as ERC-20 approve-phishing on EVM), reachable on
`jupiter_swap` — the highest-weighted (`weight = 3.0`), only-active-by-default
protocol in `cooker.toml` — on every tick.

**Recommendation**: implement the second half of the original H-1
recommendation that was explicitly deferred: assert every instruction's
`program_id` is in an allowlist (Jupiter aggregator program, ATA program,
token program, compute-budget program) before signing. This is the cheapest
version of the fix and closes the gap completely — an allowlist of 3-4 known
program IDs, checked once per swap, costs nothing on the happy path. A weaker
but still valuable partial mitigation, if a full allowlist is out of scope
right now: reject any transaction containing an SPL Token `Approve` /
`ApproveChecked` instruction specifically, since no legitimate Jupiter swap
route needs to grant standing delegate authority.

---

## Medium

### M-1: `supersonic_cast` signs an unaudited third-party SDK's constructed instruction with zero local verification of its accounts

**File**: `src/protocols/supersonic_cast.rs:119-137`

```rust
let plan = plan_bundle(&master_seed, bundle_id, real_dest.pubkey(), amount, self.k, DecoyConfig::default())
    .map_err(|e| anyhow::anyhow!("supersonic bundle planning failed: {e}"))?;
let ix = build_instruction(self.router_program_id, wallet.pubkey(), &plan);
let tx = Transaction::new_signed_with_payer(&[ix], Some(&wallet.pubkey()), &[wallet], recent_blockhash);
let sim = rpc.simulate_transaction(&tx).await?;   // only checks for an *error*, same limit as H-1
```

`real_dest.pubkey()` (the intended recipient of the bundle's real leg) is
correctly derived locally from the wallet's own seed — that part is sound and
matches `nft_flip.rs`'s pattern. But `plan` and `ix` themselves are produced
entirely by `supersonic_sdk` (`git = "https://github.com/Jmkoygg/supersonic-tx"`,
already flagged in `security-audit-2026-07-20.md` M-5 as *"a personal fork of
an open, unmerged PR, not a published/audited crate"*). Unlike `jupiter.rs`
(which at least checks fee-payer identity and signer count post-H-1), there is
**no** equivalent check here at all: `account-cooker` never inspects `ix.accounts`
to confirm the leg carrying `amount` actually targets `real_dest.pubkey()`
before this wallet signs. This audit could not verify `build_instruction`'s
actual account layout (out of scope — the SDK's source lives in a separate
repo, same limitation the original M-5 already noted), which is itself the
point: the code trusts it blindly.

This is lower-likelihood than H-1 — `supersonic_sdk` is a git-pinned exact
commit (immune to a live force-push rug-pull, per M-5's own note), not a
runtime HTTP response, so the realistic trigger is an SDK bug or a
supply-chain compromise of the pinned commit (the latter already tracked
under M-5, not duplicated here) rather than a remote attacker acting at
request time. It's rated Medium rather than Low because the mitigating cost is
so low relative to the fact that the dependency is explicitly *known and
documented* to be unaudited: a single post-construction assertion closes the
gap regardless of whether the SDK is ever compromised or simply buggy.

**Recommendation**: after building `ix`, assert (a) `wallet.pubkey()` is the
only signer required, mirroring `jupiter.rs`'s `validate_swap_transaction`,
and (b) — if `supersonic_sdk`'s `Plan`/`Instruction` types expose enough
structure to identify the real leg — that its destination account is exactly
`real_dest.pubkey()`. If the SDK's plan type doesn't expose that today, that's
itself worth raising upstream (`Jmkoygg/supersonic-tx`), since it's the
minimum a caller needs to verify the one property that actually matters here.

---

## Low

### L-1: `dao_vote.rs` collapses "account not found" and "RPC call failed for another reason" into the same branch at two call sites

**File**: `src/protocols/dao_vote.rs:257-264` and `:277-303`

```rust
if rpc.get_account(&vote_record).await.is_ok() {
    anyhow::bail!("wallet {} already cast a vote ...", wallet.pubkey());
}
// ...
match rpc.get_account(&our_token_owner_record).await {
    Err(_) => { ixs.push(build_create_token_owner_record_instruction(...)); }
    Ok(existing) => { /* check deposit_amount */ }
}
```

Both sites use a blanket `Err`/`is_ok()` on `rpc.get_account` to distinguish
"the account doesn't exist yet" from "the account exists." A transient RPC
error (timeout, rate limit, momentary network blip) is indistinguishable from
"not found" in this code, so it gets treated as the same case: the
`vote_record` check assumes "not yet voted" and proceeds; the
`our_token_owner_record` check assumes "doesn't exist" and queues a `Create`
instruction. This is the same *shape* as the class of bug named in the audit
request (an ambiguous/failed lookup silently resolved to an assumed-safe
default rather than surfaced) — worth flagging on that basis even though,
concretely, both sites fail closed today: SPL Governance's own account-creation
instructions reject re-initializing an already-allocated PDA, so a wrongly-assumed
"doesn't exist" is caught by `simulate_transaction` before send in
either case, not silently accepted. This is a fragile safety argument, though
— it depends entirely on a downstream program's behavior that account-cooker
doesn't control, rather than on account-cooker explicitly distinguishing the
two cases itself.

**Recommendation**: match on the specific "account not found" signal (e.g.
`ClientErrorKind`/`RpcError` variant or message content) rather than a blanket
`Err(_)`/`is_ok()`, so a transient RPC failure surfaces as a retryable error
instead of silently being treated as a specific on-chain state.

---

## Not applicable to this pattern

"Bypass a fee" (item 3 of the audit request) doesn't apply to any of these six
files: none of them implement their own fee-collection logic with a
protocol-owned vault/fee account that could be spoofed — fees, where they
exist, are enforced entirely inside the third-party on-chain programs being
composed with (Jupiter, Marinade, SPL Governance, Metaplex), outside this
repo's control or attack surface.

`src/protocols/orca_lp.rs` was checked for completeness since the audit
request said "every protocol that sends real transactions," but it isn't one
yet: `execute()` unconditionally `bail!`s without touching `rpc` or `wallet` —
a documented skeleton (see its own `TODO` doc comment), correctly excluded
from the user's file list.

## Recommendations, in priority order

1. **H-1** — add a program-ID allowlist (or, at minimum, an `Approve`/
   `ApproveChecked` instruction rejection) to `jupiter.rs`'s
   `validate_swap_transaction`, completing the recommendation the original
   H-1 explicitly deferred.
2. **M-1** — add a post-construction check in `supersonic_cast.rs` verifying
   the SDK-built instruction's signer and (if feasible) real-leg destination
   before signing, matching the discipline `jupiter.rs` already has.
3. **L-1** — distinguish "not found" from "RPC failed" at `dao_vote.rs`'s two
   `get_account` call sites.

## Sign-off

- [x] Manual line-by-line review of all 6 named files against the 4-question
      checklist in the audit request — complete
- [x] Trust chain for every externally-sourced account traced to either a
      local derivation, a verified owner+discriminator check, or flagged as a
      gap — complete
- [x] `consolidation.rs`'s wallet pool traced to its construction site
      (`scheduler/mod.rs`) to confirm it cannot be externally influenced
- [x] Checked for scope gaps against the user's own framing ("every protocol
      that sends real transactions") — found and noted `orca_lp.rs` is an
      unimplemented skeleton, correctly out of scope
- [x] No fixes applied — read-only pass per instruction, no commits made
- [ ] Findings above discussed with the user and a fix decision made — pending

# account-cooker Security Audit

**Date**: 2026-07-20
**Scope**: `account-cooker` — off-chain Rust agent that signs and sends real Solana
transactions (Jupiter swaps, Marinade stake, `supersonic-tx` composability) from
real wallet keypairs. This is **not** an on-chain Anchor/Pinocchio program; there
is no `programs/` directory, no `declare_id!`, no on-chain account validation
surface. The audit is scoped accordingly: fund-custody/signing correctness,
secret handling, external-input trust boundaries, and Rust safety, rather than
PDA/CPI/discriminator review.
**Auditor**: claude (via `/audit-solana`)
**Commit**: `64e0bbd` (branch `feat/jupiter-swap-scaffold`)

## Summary

| Severity | Count | Fixed |
|---|---|---|
| Critical | 0 | — |
| High | 1 | 1 |
| Medium | 6 | 2 |
| Low | 5 | 0 |

No leaked key material, no direct remote fund-drain path, and no critical
findings. The main issues are (1) one protocol blind-signs an externally
supplied transaction with no simulation or content check, (2) a reachable panic
in the funded-protocol amount calculation, and (3) upstream `ed25519-dalek`
advisories inherited from the Solana SDK that this repo cannot fix unilaterally.

**Update 2026-07-20 (same-day follow-up pass)**: H-1, M-1, and M-6 — the three
findings actionable without an upstream dependency bump or a product/scope
decision — are now fixed in the working tree (uncommitted, pending review).
`cargo build --release`, `cargo test --release`,
`cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check` were run
after each individual fix and stayed clean throughout (15/15 tests passing, 0
clippy warnings, no format diff, at every step). See "Status: FIXED" under
each finding below.

## What's already right (verified, not assumed)

- **No secrets in git, ever.** `git ls-files` and a full-history blob scan for
  `wallets/`, `wallets-mainnet/`, `.env`, `cooker.toml`, `cooker.mainnet-test.toml`
  found zero hits across all commits. `.gitignore` correctly excludes all of them.
- Wallet keypair files (`wallets/*.json`, `wallets-mainnet/bounty-wallet.json`)
  are mode `0600` (owner-only).
- `.cooker_state/` checkpoint writes are properly atomic (tmp file + `fsync` +
  `rename`), with a dedicated test proving no `.tmp` file survives a save.
- `sample_interval_secs` clamps every sleep to `[30s, 12h]`, so a pathological
  log-normal draw can't produce a near-zero or multi-day wait.
- `marinade_stake` and `supersonic_cast` both call `simulate_transaction` and
  surface simulation logs before sending — the right pattern (see High-1 for
  where it's missing).
- `supersonic-sdk` (the one git dependency) is pinned to an exact commit `rev`,
  not a movable branch — immune to a remote force-push rug-pull.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` (full
  `clippy::all`) are both **clean, zero warnings**.
- All 15 unit tests pass (`cargo test`).
- CI (`.github/workflows/ci.yml`) already runs build+test+clippy+fmt on every
  push/PR.
- Dust-floor guards (`min_swap_lamports` / `min_stake_lamports` /
  `min_cast_lamports`) and the `max_balance_fraction` reserve mean most bugs
  found below fail closed into "wastes a 5000-lamport fee," not fund loss.

---

## High

### H-1: `jupiter_swap` signs and broadcasts an externally-supplied transaction with no simulation and no content validation

**File**: `src/protocols/jupiter.rs:151-169`

`marinade_stake` and `supersonic_cast` both build their own instructions locally
and call `rpc.simulate_transaction()` before sending. `jupiter_swap` — the
highest-weighted, default-enabled protocol (`weight = 3.0` in
`cooker.example.toml`, the only one active in the current `cooker.toml`) —
does neither:

```rust
let swap: SwapResponse = swap_resp.json().await?;
let tx_bytes = base64_decode(&swap.swap_transaction)?;
let mut tx: VersionedTransaction = bincode::deserialize(&tx_bytes)?;
tx.signatures[0] = wallet.sign_message(&tx.message.serialize());
let sig = rpc.send_and_confirm_transaction(&tx).await?;
```

The wallet signs and sends whatever `bincode`-decodes from `lite-api.jup.ag`'s
response, with no check that the instructions inside actually match "swap
`amount` of `input_mint` for `output_mint`." Nothing prevents the transaction
from containing an additional instruction (e.g., an SPL `Approve` granting
token delegate authority to a third-party address) alongside the expected swap.

**Failure scenario**: `lite-api.jup.ag` is compromised, DNS-hijacked, fronted by
a malicious mirror (if `JUPITER_SWAP_URL` is ever made configurable), or simply
has a server-side bug/compromise that injects an extra instruction into the
returned `swapTransaction`. `account-cooker` deserializes it, signs it with the
real agent keypair, and — because there is no local simulation and no
instruction allowlist — broadcasts it with no check catching the anomaly first.
Every agent running this protocol (the default/primary one) is exposed on every
tick.

Note also `tx.signatures[0] = ...` is an unchecked index into a `Vec` from
externally-deserialized data — a structurally malformed `swapTransaction` with
zero signature slots would panic here rather than error out, though this is a
narrow illustration of the same underlying "external data trusted without
structural validation" issue, not a separate finding.

**Recommendation**:
1. Call `rpc.simulate_transaction()` before sending, matching `marinade.rs` /
   `supersonic_cast.rs`, and surface logs on failure.
2. At minimum, assert the deserialized transaction's fee payer is the wallet
   and that every instruction's `program_id` is in an expected allowlist
   (Jupiter aggregator program, associated-token-program, token program,
   compute-budget program) before signing. Rejecting anything else costs
   nothing on the happy path and closes the blind-signing gap.

**Status: FIXED** (working tree, uncommitted). `src/protocols/jupiter.rs`
`execute()` now calls `validate_swap_transaction(&tx, &wallet.pubkey())?`
immediately after deserializing the API response and before signing — it
rejects anything that isn't a single-required-signer transaction whose sole
signer/fee-payer is this wallet (also closes the `tx.signatures[0]` index-panic
noted above, by checking `!tx.signatures.is_empty()` first). After signing, it
now calls `rpc.simulate_transaction(&tx)` and bails with the simulation error
plus full logs on failure, before ever calling
`send_and_confirm_transaction`, matching `marinade.rs` / `supersonic_cast.rs`.
Implemented as recommendation 1 in full, and a scoped version of
recommendation 2 (fee-payer/signer-count check rather than a full
program-id instruction allowlist, which the user judged sufficient for
"minimally check the shape" without hardcoding Jupiter's route-dependent AMM
program IDs).

---

## Medium

### M-1: `gen_range` panics when `usable == 0`, silently killing that agent's task forever

**Files**: `src/protocols/jupiter.rs:114`, `src/protocols/marinade.rs:191`,
`src/protocols/supersonic_cast.rs:102` — identical pattern in all three funded
protocols:

```rust
let amount = rand::thread_rng().gen_range((usable / 4).max(1)..=usable);
```

`rand::Rng::gen_range` panics on an empty range. If `usable == 0` while the
preceding guard (`if usable < min_X_lamports { bail }`) doesn't catch it, the
range becomes `1..=0`, which is empty → panic. That guard only protects this
when `min_X_lamports > 0`; if an operator sets a `min_*_lamports` floor to `0`
— which is exactly what the README's own roadmap item **"Dust-level interaction
mode (sub-cent amounts, higher frequency)"** would require — and the wallet's
balance/fraction computes to exactly `0`, this panics.

The blast radius is contained but silent: each agent runs in its own
`tokio::spawn` task, so the panic doesn't crash the process, but
`scheduler::run_fleet`'s `futures_wait_all` discards the result —

```rust
async fn futures_wait_all(handles: Vec<tokio::task::JoinHandle<()>>) {
    for h in handles {
        let _ = h.await;   // JoinError (including panics) silently dropped
    }
}
```

— so that one agent stops forever with **no log line**. For a tool whose stated
goal is scaling to "thousands of concurrent agents," a silently-dead agent is a
real operational gap, and it's on a code path this project's own roadmap will
walk into.

**Recommendation**: guard `usable == 0` explicitly before the `gen_range` call
(bail out, same as the existing dust-floor check), and log
`JoinError`/panics in `futures_wait_all` instead of discarding them.

**Status: FIXED** (working tree, uncommitted) — the panic itself. All three
protocols (`jupiter.rs`, `marinade.rs`, `supersonic_cast.rs`) now bail with a
clear error (`"wallet balance too low to compute a non-zero
{swap,stake,cast} amount, skipping this tick"`) immediately if `usable == 0`,
before the existing `usable < min_*_lamports` check and before `gen_range` is
ever reached — closing the panic regardless of how `min_*_lamports` is
configured. The `futures_wait_all`/`JoinError`-logging half of the
recommendation above (visibility into a dead agent task) was **not** part of
this pass and remains open.

### M-2: Upstream `ed25519-dalek 1.0.1` is in the live signing path (RUSTSEC-2024-0344, RUSTSEC-2022-0093)

`cargo audit` flags two advisories against crates in the dependency tree that
`solana-keypair v2.2.3` (via `solana-sdk`/`solana-client` 2.1→2.3.x) pulls in
directly — **not a dependency this project chose**:

- **RUSTSEC-2024-0344** — timing variability in `curve25519-dalek 3.2.0`'s
  scalar subtraction, a dependency of `ed25519-dalek 1.0.1`. This is scalar
  arithmetic used during signing itself.
- **RUSTSEC-2022-0093** — the "double public key signing function oracle
  attack" in `ed25519-dalek 1.0.1`. This is primarily a *verifier*-side
  exploit (an attacker crafts related keys to fool strict/batch signature
  *verification*); `account-cooker` never verifies untrusted signatures
  against attacker-supplied keys, so the direct exploit scenario doesn't
  clearly apply to this project's own code paths — but the vulnerable crate
  version is what every `Keypair::sign_message` call in this codebase
  resolves to.

Traced with `cargo tree -i ed25519-dalek@1.0.1`: there is exactly one
`ed25519-dalek` in the whole tree (no coexisting patched version), reached via
`solana-keypair`, `solana-signature`, and `solana-ed25519-program` — i.e., this
is the actual signing implementation behind every `wallet.sign_message(...)`
call in `jupiter.rs`, `marinade.rs`, and `supersonic_cast.rs`. Every real
mainnet transaction this tool has produced was signed through this code.

**This is not fixable from this repo's `Cargo.toml`** — it's determined
entirely by what `solana-keypair` (as currently published) depends on.
Practical exploitability of the timing side-channel requires precise local
timing measurement (co-located/same-host access), not a remote network
attacker against a normal operator setup.

**Recommendation**: track `solana-keypair`/`solana-sdk` upstream for a bump
past `ed25519-dalek 2.x`; no local action is currently available beyond that.

### M-3: `rustls-webpki 0.101.7` — reachable panic + two certificate-validation bugs

Three advisories (RUSTSEC-2026-0104 reachable panic in CRL parsing,
RUSTSEC-2026-0098/0099 incorrect name-constraint acceptance), pulled in via
`solana-pubsub-client → tungstenite/tokio-rustls → rustls 0.21.12`. DoS-class
and certificate-trust-weakening; same "upstream SDK pin, not this repo's
choice" caveat as M-2. Recommend tracking for a bump when the Solana SDK moves
past this `rustls` line.

### M-4: Balance-fraction math is unchecked `f64`, with no bounds validation on the config value

**Files**: `jupiter.rs:109`, `marinade.rs:186`, `supersonic_cast.rs:97` —
identical in all three:

```rust
let usable = (balance_lamports as f64 * self.max_balance_fraction) as u64;
```

`max_balance_fraction` comes straight from `cooker.toml`'s `params` table and
is never validated to be within `[0.0, 1.0]` anywhere (`CookerConfig::validate`
doesn't touch protocol `params`; each protocol's `from_params` only checks
presence/type, not range). This is also exactly the pattern `rust.md` calls
out as forbidden ("`as u32` truncates! use `try_into()`"). In practice this is
self-limiting on Solana — a wildly-oversized `amount` just makes the
transaction fail on-chain for insufficient funds, costing one base fee, not a
drain — but it's a real gap between the project's own mandatory Rust rules and
the code, on the exact line that computes how many lamports of a real wallet
get committed to a signature.

**Recommendation**: validate `(0.0..=1.0).contains(&max_balance_fraction)` in
each protocol's `from_params` (cheap, matches the existing validation style
already used for `k` in `supersonic_cast.rs` and `noise_mints.len()` in
`jupiter.rs`), and prefer `checked` integer math over the `f64` round-trip
where feasible.

### M-5: Wallet-secret-derived seed is handed to an unaudited, personal-fork git dependency

**File**: `src/protocols/supersonic_cast.rs:82-87`

```rust
fn derive_master_seed(wallet: &Keypair) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(MASTER_TAG);
    h.update(wallet.to_bytes());   // full 64-byte keypair, incl. secret key
    h.finalize().into()
}
```

The `SHA256(tag || secret_bytes)` derivation itself is sound — a one-way hash
doesn't expose the private key. But the resulting `master_seed` (a value
directly derived from live wallet secret material) is then passed into
`supersonic_sdk::plan_bundle` / `derive_decoy_keypair` — code from
`git = "https://github.com/Jmkoygg/supersonic-tx"`, a personal fork of an open,
unmerged PR, not a published/audited crate. This audit's scope is
`account-cooker`, not that dependency, so it cannot confirm whether that SDK
logs, retains, or otherwise mishandles `master_seed` internally. If it does,
the exposure is "predictable per-wallet decoy addresses," not the wallet's
private key — but it's still secret-derived material crossing into unaudited
third-party code.

Already meaningfully mitigated: `weight = 0.0` by default in
`cooker.example.toml`, and the README/THREAT_MODEL are explicit that this
protocol and its dependency are devnet-only, not mainnet-audited.

**Recommendation**: before ever setting `weight > 0` against a funded mainnet
wallet, review `supersonic-sdk`'s handling of the seed it's given, or switch to
a wallet-independent seed persisted locally instead of one derived from the
live secret key.

### M-6: `marinade_test.rs` comment/code mismatch — could send a much larger real transaction than documented

**File**: `src/bin/marinade_test.rs:36-43`

```rust
// Small, config-driven amount: max_balance_fraction picks ~1-2% of
// balance, min_stake_lamports floor is low enough not to block it.
let params: toml::Table = toml::from_str(
    r#"
    max_balance_fraction = 0.20
    min_stake_lamports = 1000000
    "#,
)?;
```

The comment says ~1-2%; the actual value is `0.20` — **20%**, a 10-20x
discrepancy. This binary, when run with `MARINADE_SEND=1`, sends a real
mainnet deposit from the user's actual default wallet
(`~/.config/solana/id.json`, not one of the disposable `wallets/agent-*`
keypairs). A future maintainer trusting the comment could be surprised by how
much of their real wallet gets staked.

**Recommendation**: fix the comment or the value — they should agree.

**Status: FIXED** (working tree, uncommitted). `max_balance_fraction` changed
from `0.20` to `0.02` in `src/bin/marinade_test.rs`, matching the existing
"~1-2% of balance" comment (the comment was left as-is since it was already
accurate; only the value was wrong). Resolved by reducing the real value
rather than rewriting the comment, per explicit preference — this binary
sends real mainnet transactions from the user's actual default wallet when
`MARINADE_SEND=1`, so 20% was judged too large for what's documented and
intended as a small proof-of-work amount.

---

## Low

### L-1: `unwrap()`/`expect()` in production code paths, contrary to the project's own rule

CLAUDE.md / `rust.md` / `anchor.md` all state "NEVER use `unwrap()`/`expect()`
in program code." Five sites violate this outside `#[cfg(test)]` / dedicated
test-harness binaries (`recovery_test.rs`, `state.rs` tests, etc. are exempt
per the project's own stated test policy and are not included below):

| Site | Reachable today? |
|---|---|
| `protocols/mod.rs:58` `ProtocolRegistry::pick()` fallback `.unwrap()` | Only if `entries` is empty — currently prevented by `CookerConfig::validate()`, but that guard lives in a different module and isn't enforced by `ProtocolRegistry` itself |
| `protocols/jupiter.rs:83,88` `pick_pair()` `.expect("validated non-empty in from_params")` | No — guarded by `from_params`'s `noise_mints.len() < 2` check |
| `protocols/marinade.rs:153` `.expect("valid static pubkey")` | No — parses a hardcoded, correct constant |
| `protocols/supersonic_cast.rs:68` `.expect("valid hardcoded program id")` | No — same as above |
| `timing.rs:25` `.unwrap()` in the `LogNormal::new` fallback closure | No — `mean_secs.max(1.0)` upstream guarantees a finite, valid input |

None are live bugs today, but each is a landmine: the guard that makes it safe
lives in a different function (sometimes a different file) than the
unwrap/expect, so a future change to validation logic could silently make one
reachable with no compiler signal. `ProtocolRegistry::pick()` is the one to
prioritize, since `ProtocolRegistry::from_config` is a plausible call site for
future code that doesn't happen to route through `CookerConfig::validate()`
first.

### L-2: `.env` is world-readable (`0644`) while wallet keypairs are correctly `0600`

`ls -la .env` shows `-rw-r--r--`. `.env.example`'s template lists several live
API keys (Helius, Mistral, X Bearer token, DFlow, QuickNode). This audit did
not read `.env`'s actual contents, but if it's populated with real keys on a
shared/multi-user machine, tightening to `chmod 600 .env` costs nothing and
matches the discipline already applied to the wallet files.

### L-3: `bincode 1.3.3` (flagged unmaintained by `cargo-audit`) deserializes the externally-supplied Jupiter transaction

Ties directly into H-1: the crate used to decode `swap.swap_transaction` — data
originating from a third-party HTTP API — is itself flagged
`RUSTSEC-2025-0141` (unmaintained). Not a known-CVE, but worth weighting
alongside H-1's fix rather than considered in isolation.

### L-4: No runtime re-verification that the hardcoded Marinade constants still match live on-chain state

`MarinadeAccounts::derive()` (`marinade.rs:78-110`) derives all PDAs from
hardcoded `MARINADE_STATE`/`MARINADE_PROGRAM_ID` strings with no
`rpc.get_account(&state)` ownership check before building the deposit
instruction. The README states the seed *constants* were verified against live
state during development — that's a provenance claim about how the constants
were written, not a runtime guarantee. Already fails closed today:
`simulate_transaction` runs before send and would reject a stale/wrong
constant before anything is broadcast. Optional hardening only.

### L-5: Residual `cargo-audit` "unmaintained"/"unsound" advisories, transitive-only

`atty` (unmaintained + `RUSTSEC-2021-0145` unsound unaligned read),
`derivative`, `libsecp256k1`, `number_prefix`, `paste` (all unmaintained),
`memmap2` (`RUSTSEC-2026-0186` unsound pointer offset), `rand 0.7.3`
(`RUSTSEC-2026-0097`, coexists with this project's own `rand 0.8` — a
different resolved version pulled in by `ed25519-dalek`/`libsecp256k1`, not by
account-cooker's own code). All transitive via the Solana SDK tree; no evidence
any is reached by account-cooker's own call paths. Logged for tracking, not
actioned.

---

## Automated analysis results

```
cargo fmt --check         → clean, no diff
cargo clippy --all-targets -- -W clippy::all -D warnings
                           → 0 warnings
cargo build --all-targets → success, 0 warnings
cargo test                → 15 passed; 0 failed
cargo audit                → 5 advisories (curve25519-dalek, ed25519-dalek,
                              rustls-webpki x3), 9 unmaintained/unsound warnings
                              — see Medium/Low above for triage
```

`cargo clippy` with `unwrap_used` / `expect_used` / `panic` /
`arithmetic_side_effects` / `cast_possible_truncation` / `cast_sign_loss` /
`cast_precision_loss` added surfaces ~90 additional pedantic warnings. The
large majority are simple loop counters and index arithmetic in
`detectors.rs` / `timing_harness.rs` (measurement/research code, not the
fund-custody path) with no realistic overflow — not reported individually
above as findings. The subset that matters (the `f64` balance-fraction casts,
the checkpoint timestamp subtraction, the `unwrap`/`expect` sites) is captured
in M-4 and L-1.

## Not applicable to this codebase

The `/audit-solana` checklist is written for on-chain Anchor/Pinocchio
programs. This repo has none, so the following sections of that checklist
don't apply and are omitted rather than padded: account discriminators/type
cosplay, PDA bump storage, account-close/revival attacks, CPI target
validation via `Program<'info, T>`, `#[account]`/`#[derive(Accounts)]`
constraints, and CU profiling.

## Recommendations, in priority order

1. [x] **Fixed** — H-1: simulate before sending in `jupiter_swap`, and
   validate the returned transaction's shape before signing.
2. [x] **Fixed (panic only)** — M-1: guard `usable == 0` before `gen_range`
   in all three protocols. Still open: stop discarding `JoinError` in
   `futures_wait_all` so a dead agent task isn't silent.
3. [x] **Fixed** — M-6: reconciled `marinade_test.rs`'s comment and its
   actual `max_balance_fraction` (reduced the value to `0.02`).
4. Add `(0.0..=1.0)` validation for `max_balance_fraction` (M-4).
5. Track upstream Solana SDK dependency bumps for M-2/M-3 — not actionable
   locally today.
6. Add `cargo audit` to CI (`.github/workflows/ci.yml` currently runs
   build/test/clippy/fmt but not a dependency audit).
7. L-1 through L-5 as time permits — none are live bugs.

## Sign-off

- [x] Automated analysis (build, fmt, clippy, test, audit) — complete
- [x] Manual review of every `src/` file — complete
- [x] Secret-leak history scan — complete, clean
- [x] H-1, M-1 (panic), M-6 fixed and re-verified (build/test/clippy/fmt clean
      after each) — **uncommitted, in the working tree, pending human review**
- [ ] All Medium/High issues resolved — M-2 through M-5, and the
      `JoinError`-visibility half of M-1, remain open (see Recommendations)
- [ ] Ready for mainnet at higher `weight`/larger `max_balance_fraction` — the
      two blockers named in the original audit (H-1, M-1) are now fixed, but
      this line isn't checked off until the fixes above are reviewed and
      committed by a human, and the remaining Mediums are triaged

## Fix verification log (2026-07-20 follow-up pass)

Each fix below was applied individually and followed by
`cargo build --release`, `cargo test --release`,
`cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check` — all
four clean after every single fix, not just at the end:

| Fix | Files touched | Build | Test (15 tests) | Clippy | Fmt |
|---|---|---|---|---|---|
| H-1 | `src/protocols/jupiter.rs` | ✅ | ✅ 15 passed | ✅ 0 warnings | ✅ clean |
| M-1 | `jupiter.rs`, `marinade.rs`, `supersonic_cast.rs` | ✅ | ✅ 15 passed | ✅ 0 warnings | ✅ clean |
| M-6 | `src/bin/marinade_test.rs` | ✅ | ✅ 15 passed | ✅ 0 warnings | ✅ clean |

No commit was made — all changes are in the working tree
(`git status --short` shows the four files modified) for the user to review
and commit themselves.

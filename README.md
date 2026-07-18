# account-cooker

Spawns long-lived Solana agents that behave like real humans — trading, staking,
providing liquidity — with irregular, human-plausible timing. The goal is to
make wallet clustering and copy-trading genuinely harder for chain analysis
tools, by manufacturing believable interaction graphs at scale.

Built for the [Superteam Brazil Privacy-Through-Noise bounty](https://earn.superteam.fun).
See [`THREAT_MODEL.md`](./THREAT_MODEL.md) for the full scope, assumptions,
and honest limitations.

## Proof it works — measured, not asserted

Two independent kinds of proof: real signed transactions on mainnet, and a
reproducible harness that measures (not just implements) the timing design's
resistance to a common bot-detection heuristic.

### 1. Real signed transactions on mainnet

No devnet mocks. Every row below is a `jupiter_swap` interaction signed by a
live agent and confirmed on Solana mainnet-beta.

| # | What was tested | Result | Proof |
|---|---|---|---|
| 1 | Quote fetch + swap build + sign + send, wSOL→USDC | **PASS** | [tx](https://solscan.io/tx/6U6Ai4Vhaf9ijRAH35CCuJ96wwiLeuwKYAjaAa3Y1LRaQWi8Y79qFrjcvwQURsYHMfWEpzufvsZbjACegKcPg7F) |
| 2 | Same flow after `noise_mints`/`min_swap_lamports` became config-driven (regression check) | **PASS** | [tx](https://solscan.io/tx/2nC1QzXE2pcbb4SAD215wxfy7rTbuyDhg1tktVcxR2P7qzk5a1ZJXuBqTZzDdnDiDTR5tcXqfmr3LXZLvJKxmFk6) |
| 3 | `validate` catches missing keypair / bad config | **PASS** | terminal output, see below |
| 4 | `status` reads live balances for N configured wallets | **PASS** | terminal output, see below |
| 5 | Marinade `deposit` — manually built instruction (real PDAs derived, no Anchor client), simulated then sent | **PASS** | [tx](https://solscan.io/tx/3UALUZB3ZUe8qYZa3DwYrHd6qa9QRhGRbFi8u46RUDTXubxbpZYTn8YQ39y2SiDWUdTNEhRbMCGqTkajFqSzxrZN) |
$ ./target/release/cooker validate --config cooker.toml
Config is valid. 3 protocol(s), 3 wallet(s) available.
$ ./target/release/cooker status --config cooker.toml
agent-01   8MFkpAEfiRFy4DtqBuRKhT5KiXteqVPaNfmcdYHJQc6t   6.999990 SOL
agent-02   JCvUs7p81BpcVYocTBwKS2Mps3MBodCSwrS7VPyx7kL8   1.500000 SOL
agent-03   J4te3tGAk7XCd9Tuhq4CsD1t5g5qNPVvkMGKAPy6Gboi   1.500000 SOL

Both mainnet swaps routed through Jupiter's aggregator across Meteora DLMM and
Orca Whirlpools liquidity, confirmed at `Finalized` commitment — this is a real
aggregated swap, not a single-pool toy path.

The Marinade deposit (row 5) is not built with the Anchor-generated SDK — the
available crates (`marinade`, `marinade-cpi`) pin an older `solana-program`
incompatible with this project's `solana-sdk = "2.1"`. Instead the `deposit`
instruction is built by hand: PDAs (`reserve_pda`, `liq_pool_sol_leg_pda`,
`liq_pool_msol_leg_authority`, `msol_mint_authority`) are derived via
`Pubkey::find_program_address` using the exact seed constants from
[`marinade-finance/liquid-staking-program`](https://github.com/marinade-finance/liquid-staking-program)
(`state/mod.rs`, `state/liq_pool.rs`), verified against the live on-chain
`State` account rather than trusted from memory, and the caller's mSOL
associated token account is created idempotently before minting. The
transaction is simulated (`simulateTransaction`) before ever being sent for
real — see `src/protocols/marinade.rs` and `src/bin/marinade_test.rs`.

### 2. Timing design vs a naive bot — reproducible number, not a claim
cargo run --release --bin timing_harness -- --n 5000 --seed 1

This measures how often a simple, common chain-analysis heuristic (flag
"suspiciously regular timing" via coefficient of variation) catches a naive
lightly-jittered bot vs account-cooker's actual scheduler timing — using the
exact same `sample_interval_secs` function the real agent calls, not a
reimplementation:
$ cargo run --release --bin timing_harness -- --n 5000 --seed 1
timing_harness — naive fixed-cadence detector (CV < 0.15)
config: mean=45.0min std=30.0min window=8 n_per_class=5000 seed=1 bot_jitter=±5%
ClassFlagged as "fixed-cadence bot"naive bot (±5% jitter)100.00%account-cooker agent (this config)0.00%

At this config, the naive bot's ±5% jitter is nowhere near enough variance to
escape a basic CV<0.15 heuristic — it gets flagged every time. account-cooker's
log-normal timing (mean=45min, std=30min → CV≈0.67 by design) clears the same
threshold with zero false positives. This is the measured gap between "added
some randomness" and "designed the variance on purpose."

See `THREAT_MODEL.md` for the honest scope of this claim: it measures
resistance to one specific heuristic against a synthetic naive-bot baseline,
not indistinguishability from a validated real-human dataset, which this
harness does not have access to.

## Why this design

Naive "bot farms" are trivially detectable: fixed intervals, identical action
sequences, and narrow protocol coverage all show up immediately in clustering
heuristics. This project instead treats **timing and protocol variety as the
product**, not the transactions themselves:

- **Log-normal timing, not fixed sleep.** Real people don't act every N minutes
  exactly — they cluster around a habit with occasional long gaps and bursts.
  `timing::sample_interval_secs` draws from a log-normal distribution
  parameterized by a configurable mean/stddev, which produces exactly that
  shape — and is measured, not just asserted (see above).
- **Active-hours + skip-day modeling.** Each agent has a waking window and a
  daily probability of doing nothing at all — matching how humans actually miss
  days.
- **Weighted protocol selection.** Agents don't just swap. They rotate across
  swaps, staking, and LPing with configurable weights, so the on-chain footprint
  looks like a diversified user rather than a single-purpose script.
- **Independent, isolated agent tasks.** Each wallet runs its own `tokio` task
  with its own RNG state and its own schedule — nothing is shared except the
  RPC client and the protocol registry, which is what lets this scale to
  thousands of concurrent agents without coordination overhead.
- **Nothing behavior-relevant is hardcoded.** Mint selection (`noise_mints`),
  minimum swap size (`min_swap_lamports`), minimum stake size
  (`min_stake_lamports`), timing distribution, active hours, and skip-day
  probability are all read from `cooker.toml` — an operator can reshape an
  agent's entire persona without touching Rust.

## Architecture
src/
lib.rs          exposes agent/config/protocols/scheduler/timing as a library
main.rs         thin CLI entry point (binary: cooker), wires cli -> lib
bin/
timing_harness.rs   standalone binary: measures timing vs naive-bot detector
cli/            clap-based commands: run, status, validate
config/         cooker.toml parsing + validation
timing.rs       pure timing math — shared by the real scheduler AND the harness,
so the harness measures exactly what ships (not a reimplementation)
agent/          single-agent behavior loop (timing, active hours, skip-day)
scheduler/      spawns and supervises the whole fleet
protocols/      the extension point — one file per protocol
jupiter.rs      swap noise across configurable mints via Jupiter Swap API (implemented)
marinade.rs     liquid staking — deposit SOL, mint mSOL (implemented)
orca_lp.rs      concentrated liquidity positions (skeleton, see TODO)

Adding a new protocol means implementing the `Protocol` trait
(`src/protocols/mod.rs`) and registering its name in `ProtocolRegistry::from_config`.
No changes to `agent` or `scheduler` are needed — that's the "trivially
customizable" requirement from the bounty brief.

## Security / threat model

See [`THREAT_MODEL.md`](./THREAT_MODEL.md) for the full scope, assumptions,
named defenses, and — importantly — what this tool explicitly does **not**
defend against (custody compromise, value-channel unlinkability, off-chain
metadata correlation).

## Usage

```bash
# 1. Generate wallets (devnet recommended for testing)
mkdir -p wallets
for i in 1 2 3; do solana-keygen new -o wallets/agent-0$i.json --no-bip39-passphrase; done

# 2. Fund them (devnet faucet or your own transfer)
solana airdrop 1 wallets/agent-01.json --url devnet

# 3. Copy and edit the example config
cp cooker.example.toml cooker.toml

# 4. Sanity-check before a long run
cargo run --release -- validate --config cooker.toml
cargo run --release -- status   --config cooker.toml

# 5. Run the fleet
cargo run --release -- run --config cooker.toml

# 6. Measure the timing design against a naive-bot detector
cargo run --release --bin timing_harness -- --n 5000 --seed 1

# 7. Run the test suite (includes statistical regression tests on timing.rs)
cargo test --release
```

All behavior-relevant parameters (mint list, swap size floor, timing
distribution, active hours, skip-day probability, per-protocol weights) live
in `cooker.toml` — see `cooker.example.toml` for every available field.

## Status

| Protocol        | Status                          |
|-----------------|----------------------------------|
| `jupiter_swap`  | **Implemented** — real quote+swap via Jupiter Swap API, validated with 2 signed mainnet transactions (see proof table above) |
| `marinade_stake`| **Implemented** — hand-built `deposit` instruction against Marinade State with derived PDAs, validated with 1 signed mainnet transaction (see proof table above) |
| `orca_lp`       | Skeleton — instruction building TODO |

## Known limitations

- The default `rpc_url` in the example config is a public endpoint and will
  rate-limit (`429`) under any real fleet size. This did not block correctness
  in testing (the client's built-in retry handled it), but a paid RPC
  (Helius, Triton, QuickNode) is recommended for anything beyond a handful of
  agents.
- Jupiter's aggregator has no devnet liquidity; `jupiter_swap` can only be
  meaningfully tested against mainnet. The proof table above reflects that.
- `timing_harness` measures resistance to one heuristic (fixed-cadence
  detection) against a synthetic naive-bot baseline — see "Honest limitation"
  in `THREAT_MODEL.md`.

## Roadmap

- [ ] Complete Orca Whirlpools integration (Marinade is done — see Status)
- [ ] Fund splitting / periodic consolidation across agent wallets
- [ ] Dust-level interaction mode (sub-cent amounts, higher frequency)
- [ ] Bridge interactions (Wormhole) for cross-chain noise
- [ ] Prometheus metrics endpoint for fleet observability at scale
- [ ] Persona presets (day-trader, hodler, LP-farmer) bundling timing + protocol weights
- [ ] Dedicated/paid RPC support documented (see Known Limitations)
- [ ] Extend `timing_harness` with a learned adversary (logistic regression
      over multiple features, not just CV) for a stronger honest bound

## Disclaimer

This tool manufactures behavioral noise for privacy purposes — it does not
hide fund custody or launder value. See `THREAT_MODEL.md` for the intended
threat model (wallet clustering / copy-trading resistance, not compliance
evasion).

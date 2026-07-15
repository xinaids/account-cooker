# account-cooker

Spawns long-lived Solana agents that behave like real humans — trading, staking,
providing liquidity — with irregular, human-plausible timing. The goal is to
make wallet clustering and copy-trading genuinely harder for chain analysis
tools, by manufacturing believable interaction graphs at scale.

Built for the [Superteam Brazil Privacy-Through-Noise bounty](https://earn.superteam.fun).

## Proof it works — real signed transactions on mainnet

No devnet mocks. Every row below is a `jupiter_swap` interaction signed by a
live agent and confirmed on Solana mainnet-beta.

| # | What was tested | Result | Proof |
|---|---|---|---|
| 1 | Quote fetch + swap build + sign + send, wSOL→USDC | **PASS** | [tx](https://solscan.io/tx/6U6Ai4Vhaf9ijRAH35CCuJ96wwiLeuwKYAjaAa3Y1LRaQWi8Y79qFrjcvwQURsYHMfWEpzufvsZbjACegKcPg7F) |
| 2 | Same flow after `noise_mints`/`min_swap_lamports` became config-driven (regression check) | **PASS** | [tx](https://solscan.io/tx/2nC1QzXE2pcbb4SAD215wxfy7rTbuyDhg1tktVcxR2P7qzk5a1ZJXuBqTZzDdnDiDTR5tcXqfmr3LXZLvJKxmFk6) |
| 3 | `validate` catches missing keypair / bad config | **PASS** | terminal output, see below |
| 4 | `status` reads live balances for N configured wallets | **PASS** | terminal output, see below |

```
$ ./target/release/cooker validate --config cooker.toml
Config is valid. 3 protocol(s), 3 wallet(s) available.

$ ./target/release/cooker status --config cooker.toml
agent-01   8MFkpAEfiRFy4DtqBuRKhT5KiXteqVPaNfmcdYHJQc6t   6.999990 SOL
agent-02   JCvUs7p81BpcVYocTBwKS2Mps3MBodCSwrS7VPyx7kL8   1.500000 SOL
agent-03   J4te3tGAk7XCd9Tuhq4CsD1t5g5qNPVvkMGKAPy6Gboi   1.500000 SOL
```

Both mainnet swaps routed through Jupiter's aggregator across Meteora DLMM and
Orca Whirlpools liquidity, confirmed at `Finalized` commitment — this is a real
aggregated swap, not a single-pool toy path.

## Why this design

Naive "bot farms" are trivially detectable: fixed intervals, identical action
sequences, and narrow protocol coverage all show up immediately in clustering
heuristics. This project instead treats **timing and protocol variety as the
product**, not the transactions themselves:

- **Log-normal timing, not fixed sleep.** Real people don't act every N minutes
  exactly — they cluster around a habit with occasional long gaps and bursts.
  `Agent::next_interval_secs` draws from a log-normal distribution parameterized
  by a configurable mean/stddev, which produces exactly that shape.
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
  minimum swap size (`min_swap_lamports`), timing distribution, active hours,
  and skip-day probability are all read from `cooker.toml` — an operator can
  reshape an agent's entire persona without touching Rust.

## Architecture

```
src/
  main.rs         entry point, wires CLI -> config -> scheduler
  cli/            clap-based commands: run, status, validate
  config/         cooker.toml parsing + validation
  agent/          single-agent behavior loop (timing, active hours, skip-day)
  scheduler/      spawns and supervises the whole fleet
  protocols/      the extension point — one file per protocol
    jupiter.rs      swap noise across configurable mints via Jupiter Swap API (implemented)
    marinade.rs     liquid staking (skeleton, see TODO)
    orca_lp.rs      concentrated liquidity positions (skeleton, see TODO)
```

Adding a new protocol means implementing the `Protocol` trait
(`src/protocols/mod.rs`) and registering its name in `ProtocolRegistry::from_config`.
No changes to `agent` or `scheduler` are needed — that's the "trivially
customizable" requirement from the bounty brief.

## Security / threat model

This tool defends against **behavioral clustering**, not custody compromise.
Defenses currently in place:

- **No shared entropy across agents.** Each agent task owns its own `ThreadRng`
  draws for timing and mint selection — two agents never make correlated
  random choices from a shared seed.
- **Configurable minimum balance guard** (`min_swap_lamports`) prevents an
  agent from broadcasting dust-sized, obviously-scripted transactions when
  underfunded, which would itself be a clustering signal.
- **No fixed cadence anywhere in the hot path.** Both the action interval
  (log-normal) and the "check back later" interval (derived from
  `mean_interval_minutes`, not a hardcoded constant) vary per agent and per
  tick.

Explicitly **out of scope** for this tool: hiding fund custody, transaction
graph unlinkability at the protocol level (see `mirror-pool` in this bounty
for that), or defeating an adversary with access to off-chain metadata (IP,
timing correlation across services, exchange KYC).

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
```

All behavior-relevant parameters (mint list, swap size floor, timing
distribution, active hours, skip-day probability, per-protocol weights) live
in `cooker.toml` — see `cooker.example.toml` for every available field.

## Status

| Protocol        | Status                          |
|-----------------|----------------------------------|
| `jupiter_swap`  | **Implemented** — real quote+swap via Jupiter Swap API, validated with 2 signed mainnet transactions (see proof table above) |
| `marinade_stake`| Skeleton — instruction building TODO |
| `orca_lp`       | Skeleton — instruction building TODO |

## Known limitations

- The default `rpc_url` in the example config is a public endpoint and will
  rate-limit (`429`) under any real fleet size. This did not block correctness
  in testing (the client's built-in retry handled it), but a paid RPC
  (Helius, Triton, QuickNode) is recommended for anything beyond a handful of
  agents.
- Jupiter's aggregator has no devnet liquidity; `jupiter_swap` can only be
  meaningfully tested against mainnet. The proof table above reflects that.

## Roadmap

- [ ] Complete Marinade and Orca Whirlpools integrations
- [ ] Fund splitting / periodic consolidation across agent wallets
- [ ] Dust-level interaction mode (sub-cent amounts, higher frequency)
- [ ] Bridge interactions (Wormhole) for cross-chain noise
- [ ] Prometheus metrics endpoint for fleet observability at scale
- [ ] Persona presets (day-trader, hodler, LP-farmer) bundling timing + protocol weights
- [ ] Dedicated/paid RPC support documented (see Known Limitations)

## Disclaimer

This tool manufactures behavioral noise for privacy purposes — it does not
hide fund custody or launder value. See the Security section above for the
intended threat model (wallet clustering / copy-trading resistance, not
compliance evasion).

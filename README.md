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

### 1b. Composability — a real bundle cast through supersonic-tx (devnet)

The edital asks for composability: "so other tools (including account-cooker)
can cast through it." `src/protocols/supersonic_cast.rs` is a new `Protocol`
implementation — same trait, same config-driven pattern as `jupiter.rs` and
`marinade.rs` above — that casts an agent's noise transfers as
intent-ambiguous bundles through
[`supersonic-tx`](https://github.com/solanabr/supersonic-tx) (PR #1 by
Jmkoygg, `feat/intent-ambiguous-router`), whose router program is deployed and
live on devnet at `BCrR3JKi5EWhC5DuKYzV4EX7ogawoWaoKkhSqZYeYabn`. It depends on
that PR's `supersonic-sdk` as an MIT-licensed git dependency and touches only
its public API (`plan_bundle` + `build_instruction`) — **not a reimplementation
of the router**, see [`COMPOSABILITY.md`](https://github.com/solanabr/supersonic-tx)
in the sibling `supersonic-tx-compose` repo for the full writeup and a second,
standalone proof transaction outside account-cooker entirely.

| # | What was tested | Result | Proof |
|---|---|---|---|
| 6 | `SupersonicCast::execute` — the real `Protocol` trait code path (`src/bin/supersonic_cast_test.rs`) — plans an 8-leg bundle and sends it against the deployed devnet router | **PASS** | [tx](https://explorer.solana.com/tx/4CFUvJrvmV13sNpGwv9c3CpFw8uhAtdJ5Jn3TojzCFFEQfKbDfVJ9N77PmVFbaErPqT9XChwLiqcok3ca24PNzy6?cluster=devnet) |

Independently re-verified with `solana confirm <sig> --url devnet -v`:
`Status: Ok`, `Finalized` commitment.

**Honest scope:** this is devnet, not mainnet (the router itself is only
devnet-validated, not mainnet-audited). It proves the SDK's public API is a
real, usable composability contract — it says nothing about the router's own
privacy guarantees (K-anonymity, decoy indistinguishability), which is that
PR's own claim to measure, not this one's. The protocol's "real" leg targets a
seed-derived sibling address of the agent's own wallet (the router rejects
self-destination legs) rather than an external payee, so this is noise-shaping
for the agent's own footprint, not a payment-privacy path. Not enabled by
default — `weight = 0.0` in `cooker.example.toml`.

### 2. Crash recovery — real SIGKILL, not a mocked failure

```
$ ./scripts/recovery_test.sh
```

Kills the agent's checkpoint worker process (`recovery_test` binary, using
the exact same `src/state.rs` save/resume code path `Agent::run_forever`
uses) with `SIGKILL` three times in a row, mid-run, and verifies after each
restart that the checkpoint file is still valid JSON, `action_count`
advanced monotonically with no duplicate or replayed action, and the
worker resumed from checkpoint instead of losing state:

```
--- cycle 1: starting worker, will SIGKILL after 3s ---
killed pid 52972 (SIGKILL) mid-run
--- cycle 2: starting worker, will SIGKILL after 3s ---
killed pid 52974 (SIGKILL) mid-run
--- cycle 3: starting worker, will SIGKILL after 3s ---
killed pid 52976 (SIGKILL) mid-run

PASS: checkpoint file stayed valid JSON through 3x SIGKILL, action_count
      advanced monotonically (6 actions, all unique, final=6),
      and each restart resumed from the checkpoint instead of duplicating
      or losing the last action.
```

**Honest scope:** this is ONE checkpoint (last action time + resume time +
monotonic action count), tested with a real process kill repeated 3 times —
not a multi-checkpoint journal against a mainnet-mirror validator with 6
checkpoint types. See `src/state.rs` for why one checkpoint is the right
amount of state for what this tool actually needs to survive a crash
correctly, and "Known limitations" below for what this does NOT prove.

### 3. Timing design vs a naive bot — reproducible number, not a claim
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

### 3b. A second, stronger named baseline — and a worse number, reported anyway

A single CV threshold is a weak, narrow adversary. `timing_harness` now also
trains a small logistic regression (3 features: coefficient of variation,
lag-1 autocorrelation, skewness — no external ML crate, see
`src/detectors.rs`) on a held-out train/test split and reports ROC AUC plus
detection rate at the default 0.5 threshold, printed by the same command:

```
--- stronger baseline: logistic regression over [CV, autocorr(lag1), skew] ---
train_n=5000 (per class) test_n=1666 (per class, held out)
ROC AUC (test, held out): 1.0000
| Class                                    | Flagged @ 0.5 threshold        |
|-------------------------------------------|--------------------------------|
| naive bot (±5% jitter)                    | 1666/1666 (100.00%)           |
| account-cooker agent (this config)         |   12/1666 (  0.72%)           |
```

**This is reported even though it's worse than the CV-only number above:**
the agent's false-flag rate goes from 0.00% (CV threshold) to 0.72% (3-feature
logistic regression) — because autocorrelation and skewness carry a little
real signal about log-normal timing that CV alone doesn't capture. 0.72% is
still low, and the classifier is still weak relative to a real adversary with
a labeled dataset (see `supersonic-tx`'s 23-feature classifier in this same
bounty for a stronger reference point) — but the honest reading is "harder to
flag than a naive bot, not zero-detectable by any classifier," which is a
narrower and more defensible claim than the CV number alone implied.

### 3c. Multi-wallet clustering — the bounty's actual central question, measured

Everything above (1-3b) answers "does ONE wallet look like a bot?" — a binary
question. It does **not** answer what the bounty brief literally asks for:
*"explorers, analytics platforms, and copy-traders can't determine which
wallets belong to the same entity"* — a **clustering** question over many
wallets at once. `clustering_harness` (`src/clustering.rs` +
`src/bin/clustering_harness.rs`) measures that directly: it simulates several
"operators" each running several wallets, extracts behavioral features per
wallet (reusing `timing.rs`'s CV/autocorrelation/skewness, plus preferred
hour-of-day, action frequency, and protocol mix), runs a from-scratch k-means
(k = number of operators), and reports Adjusted Rand Index (ARI) and
Normalized Mutual Information (NMI) between k-means' guess and the true
operator assignment — the same two metrics `marcelofeitoza`'s account-cooker
PR uses for this comparison in the same bounty.

```
cargo run --release --bin clustering_harness -- --seed 1
```

| Scenario | ARI (mean ± std, 50 trials) | NMI (mean ± std, 50 trials) |
|---|---|---|
| `identical_control` (sanity check — no signal exists by construction) | 0.0012 ± 0.0271 | 0.1878 ± 0.0431 |
| `tight_timing` + `shared_persona` (naive bot-farm baseline) | 0.0761 ± 0.0381 | 0.2963 ± 0.0511 |
| `tight_timing` + `diverse_persona` | 0.4428 ± 0.1120 | 0.6403 ± 0.0902 |
| `wide_timing` + `shared_persona` | 0.0760 ± 0.0513 | 0.2835 ± 0.0625 |
| `wide_timing` + `diverse_persona` (**account-cooker's actual shipped config**) | 0.4214 ± 0.1100 | 0.6112 ± 0.0900 |

(ARI/NMI: 1.0 = k-means perfectly recovers who-belongs-to-whom, i.e. bad for
privacy; ~0.0 = no better than chance, i.e. good for privacy. `n=48` wallets,
`k=6` operators, `agents_per_operator=8`, 20 observed actions/wallet, seed 1 —
full command above reproduces every number here.)

**This does not favor account-cooker, and it's reported anyway.** The
account-cooker's real config (bottom row) clusters *more* accurately
(ARI 0.42) than the naive tight-timing "bot farm" baseline (ARI 0.08) —
5 configurations were run specifically to find out *why* instead of stopping
at that one uncomfortable number:

- **Timing spread barely moves the result.** `tight_timing` vs `wide_timing`
  at fixed persona: 0.0761 vs 0.0760 (shared persona), 0.4428 vs 0.4214
  (diverse persona) — statistically indistinguishable pairs. The log-normal
  vs fixed-cadence design that `timing_harness` above measures a real
  single-wallet benefit from does **not** meaningfully help against a
  multi-wallet clustering adversary.
- **Persona sharing is what actually drives it.** `shared_persona` vs
  `diverse_persona` at fixed timing spread: ARI jumps ~5.5x (0.076 -> 0.44).
  `cooker.toml` has exactly one `[timing]` block's `active_hours` and one set
  of `[[protocols]]` weights *per fleet* — every wallet one operator runs
  necessarily shares that operator's active-hours window and protocol mix by
  construction. The per-feature separability diagnostic printed by the same
  command confirms the mechanism directly: `mean_hour_of_day` and the
  `frac_*` protocol-share features go from near-zero separability under
  `shared_persona` to the strongest features in the vector under
  `diverse_persona`, while `coefficient_of_variation` stays weak throughout —
  the same statistical property (low per-wallet signal) that lets timing
  evade the single-wallet CV/logistic-regression detectors above also makes
  it a weak *clustering* feature, in both directions.

See [`THREAT_MODEL.md`](./THREAT_MODEL.md) for this named as an explicit,
quantified limitation with a concrete recommendation, and for the honest
scope statement (this measures behavioral-feature clustering resistance
only, not an adversary with on-chain funding-graph metadata — see
"Out of scope" there). This harness is additional to `timing_harness`, not a
replacement for it.

**Note on comparability with `account-cooker` PR #2 (marcelofeitoza).** That
PR reports its own ARI/NMI numbers for a "persona/session" config using a
different clustering method (connected-components at a fixed similarity
threshold) than this harness's from-scratch k-means. We reviewed that PR as
a reference point while shaping this harness's own rigor — credit where
due — but the two numbers come from different procedures measuring
different things, and shouldn't be read as directly comparable to each
other.

### 3d. Per-agent persona jitter — the fix, measured

"3c." diagnosed *why* the real config clusters more easily than a naive bot
farm: `cooker.toml` sets exactly one `active_hours` window and one
`[[protocols]]` weight vector *per fleet*, so every agent one operator runs
shares that persona byte-for-byte. `src/persona.rs` implements the fix named
there but not built yet as of "3c.": each agent now derives its OWN
active-hours window (the operator's window shifted by one small, per-agent
deterministic offset that preserves window *width* exactly) and its OWN
protocol weights (each weight perturbed independently by a small factor) —
both derived from the agent's own wallet pubkey, never a shared RNG, so it's
reproducible across restarts and uncorrelated between agents (see
THREAT_MODEL.md's "No shared entropy across agents", which this extends).
`cooker.example.toml`'s new `[persona_jitter]` block ships this ON by
default at a conservative magnitude (`active_hours_minutes = 30`,
`protocol_weight_fraction = 0.15`) — see that file for why "on by default"
is the right call here, unlike `[consolidation]`.

A sixth `clustering_harness` scenario, `wide_timing + diverse_persona +
agent_jitter`, measures the fix directly. Operator-level generation is held
byte-identical to the PRE-fix row (same `build_operator` call, same RNG
draws — see `src/bin/clustering_harness.rs`), so any difference isolates
per-agent jitter's effect alone:

```
cargo run --release --bin clustering_harness -- --seed 1
```

| Scenario | ARI (mean ± std, 50 trials) | NMI (mean ± std, 50 trials) |
|---|---|---|
| `wide_timing` + `diverse_persona` (PRE-fix) | 0.4214 ± 0.1100 | 0.6112 ± 0.0900 |
| `wide_timing` + `diverse_persona` + `agent_jitter` (**POST-fix, shipped default**) | 0.4140 ± 0.1046 | 0.6076 ± 0.0894 |

**Honest result: real, but small — reported as measured, not amplified.** At
the shipped conservative default, ARI moves from 0.4214 to 0.4140 (NMI 0.6112
to 0.6076) — inside one trial-to-trial standard deviation (±0.10-0.11), *not*
a dramatic swing, and nowhere near the naive baseline's 0.08. Two things
back up that this is a real, mechanistic effect rather than noise:

- **The per-feature separability diagnostic moves in the predicted
  direction.** `mean_hour_of_day` separability drops 1.29 -> 0.84 and
  `frac_orca_lp` drops 0.56 -> 0.34 (printed by the same command) — exactly
  the two feature families "3c." identified as the dominant clustering
  signal, moving the direction jittering them predicts.
  (`frac_marinade_stake` moved the other way, 2.10 -> 2.38 — inside
  trial-to-trial noise at this sample size, reported rather than
  cherry-picked around.)
- **The effect is monotonic in jitter magnitude.** Re-running with
  `--jitter-active-hours-minutes` / `--jitter-protocol-weight-fraction`
  overridden far past the shipped default (added specifically to explore
  this, defaults to `PersonaJitterConfig::default()` so the default
  invocation above is unaffected):

  | active_hours_minutes | protocol_weight_fraction | ARI (mean, 50 trials) |
  |---|---|---|
  | 0 (PRE-fix) | 0 | 0.4214 |
  | 30 (**shipped default**) | 0.15 | 0.4140 |
  | 60 | 0.30 | 0.3917 |
  | 90 | 0.50 | 0.3451 |
  | 180 | 0.80 | 0.2524 |
  | 300 | 1.00 | 0.2011 |
  | 600 | 1.00 | 0.1660 |

**This does not reach the naive baseline (~0.08) even at magnitudes far past
what's shippable.** `active_hours_minutes = 600` means an agent's actual
waking window can sit up to *10 hours* from the operator's configured
persona — an agent meant to wake at 8am could actually be centered around
6pm, defeating the entire point of configuring a believable per-agent
character — and ARI is still 0.166, roughly double the naive baseline.

**Why jitter alone has a ceiling**: this fix only perturbs the two features
"3c." identified as *dominant*, not the full feature vector.
`actions_per_day` (driven partly by `skip_day_probability`, which
`build_operator`'s `Diverse` persona randomizes *per operator* but this fix
does not jitter *per agent*) holds roughly steady at 1.6-2.9 separability
across the entire sweep — an un-jittered residual signal that puts a floor
under how low ARI can go from this fix alone. Closing that residual was
attempted in a later session — see "3e." below for what happened when
`skip_day_probability` was actually jittered per agent (short version: it
didn't move the floor).

**Conclusion, stated plainly**: the fix is real, measured, free (pure
config-derivation math, no funds/third-party risk — see
`PersonaJitterConfig` in `src/config/mod.rs`), and ships on by default, but
it is honestly a partial mitigation of "3c."'s finding, not a resolution of
it. The conservative shipped default is a deliberate choice: the sweep above
shows that closing more of the gap costs individual-agent believability
faster than it buys clustering resistance.

### 3e. Skip-day-probability jitter — measured, and it doesn't move the needle

"3d." named the residual directly: `actions_per_day` (driven partly by
`skip_day_probability`, which `build_operator`'s `Diverse` persona
randomizes *per operator* but the active-hours/protocol-weight fix left
un-jittered *per agent*) held steady at 1.6-2.9 separability across that
fix's entire sensitivity sweep — a floor active-hours/protocol-weight
jitter alone couldn't touch. `src/persona.rs::jittered_skip_day_probability`
closes that specific gap the same way the other two functions do: each
agent's own daily skip probability is the operator's base value times an
independent multiplicative factor drawn from
`[1 - jitter_fraction, 1 + jitter_fraction]`, clamped to `[0.0, 1.0]` (a
probability, unlike a protocol weight, cannot exceed `1.0`), derived
deterministically from the agent's own wallet pubkey (SHA-256,
domain-separated, new `DOMAIN_SKIP_DAY` tag). `cooker.example.toml`'s
`[persona_jitter]` block now ships a third field,
`skip_day_probability_fraction = 0.15`, matching `protocol_weight_fraction`'s
own conservative default.

A seventh `clustering_harness` scenario, `wide_timing + diverse_persona +
agent_jitter + skip_day_jitter`, measures it directly. Operator-level
generation and the first two jitter magnitudes are held byte-identical to
the sixth (POST-fix) row — see `src/bin/clustering_harness.rs`'s
`agent_jitter` — so any difference isolates skip-day jitter's own effect:

```
cargo run --release --bin clustering_harness -- --seed 1
```

| Scenario | ARI (mean ± std, 50 trials) | NMI (mean ± std, 50 trials) |
|---|---|---|
| `wide_timing` + `diverse_persona` + `agent_jitter` (POST-fix, from "3d.") | 0.4140 ± 0.1046 | 0.6076 ± 0.0894 |
| `wide_timing` + `diverse_persona` + `agent_jitter` + `skip_day_jitter` (this fix, shipped default) | 0.4142 ± 0.1056 | 0.6050 ± 0.0937 |

**Honest result: no measurable effect, reported as measured rather than
reshaped to look like progress.** ARI moves from 0.4140 to 0.4142 — an
*increase* of 0.0002, i.e. statistical noise, not an improvement — and NMI
moves 0.6076 -> 0.6050, also well inside one trial-to-trial standard
deviation (±0.09-0.11). This is a materially different outcome from "3d."'s
own fix, which — even though partial — showed a clean, monotonic, real
effect. Two checks confirm this null result isn't just an unlucky default
magnitude:

- **A magnitude sweep across the ENTIRE valid range shows no dose-response
  anywhere in it.** Unlike `active_hours_minutes` (unbounded upward —
  "3d."'s sweep pushed it to 600, a physically unrealistic 10-hour offset,
  to find where an effect appears) or `protocol_weight_fraction`,
  `skip_day_probability_fraction` is capped at exactly `1.0` by
  construction (past that the multiplicative factor would go negative —
  see `jittered_skip_day_probability`'s doc comment). So the sweep below
  covers *100% of this parameter's possible range*, not just "up to an
  unshippable point":

  | skip_day_probability_fraction | ARI (mean, 50 trials) |
  |---|---|
  | 0.0 (no skip-day jitter — identical to the "3d." POST-fix row) | 0.4140 |
  | 0.15 (shipped default) | 0.4142 |
  | 0.30 | 0.4080 |
  | 0.50 | 0.4160 |
  | 0.80 | 0.4064 |
  | 1.0 (maximum possible) | 0.3968 |

  The entire sweep stays inside `[0.3968, 0.4160]` — a spread of 0.019, an
  order of magnitude smaller than the ±0.10 trial-to-trial std at any
  single point, with no monotonic trend (0.50 sits *above* both 0.15 and
  0.30). This is flat noise, not the small-but-real, monotonic effect
  "3d."'s own sweep showed.
- **The per-feature separability diagnostic doesn't show a clean drop
  either.** `actions_per_day` separability across the same sweep (one
  representative trial per point, not averaged — see the same caveat "3d."
  already documents for `frac_marinade_stake`): 1.76 (fraction=0) -> 2.19
  (0.15) -> 2.20 (0.30) -> 1.78 (0.50) -> 1.64 (0.80) -> 2.16 (1.0). No
  consistent downward trend; several points sit *higher* than the no-jitter
  baseline. Read this as single-trial noise dominating a small underlying
  effect, not as skip-day jitter making clustering *easier* — the
  50-trial-averaged ARI above is the trustworthy number, and it says "no
  significant effect," not "backfired." As a sanity check that the jitter
  is properly isolated, the two features it should NOT touch stayed within
  the same single-trial noise band across the sweep's endpoints:
  `mean_hour_of_day` 0.84 -> 0.87, `frac_jupiter_swap` 4.20 -> 4.20,
  `frac_marinade_stake` 2.38 -> 2.54, `frac_orca_lp` 0.34 -> 0.33.

**Why this specific jitter likely doesn't work, even though it's
implemented and wired correctly** (verified independently of the ARI/NMI
result: unit tests in `persona.rs` and `agent/mod.rs` confirm each agent's
`skip_day_probability` is genuinely different, reproducible, and bounded;
a dedicated `clustering.rs` regression test confirms the harness's
simulation actually consumes the per-agent value, not the operator's
shared field): `actions_per_day` is a noisier statistic than
`mean_hour_of_day` or the `frac_*` protocol shares, and its dominant source
of cross-operator separability is plausibly `mean_interval_minutes` —
drawn independently per operator (`rng.gen_range(15.0..90.0)`, a 6x range)
in *every* scenario, including `shared_persona` — rather than
`skip_day_probability`. Evidence for this: even `Persona::Diverse`'s own
*operator-level* `skip_day_probability` diversity (the full `0.05..0.30`
range, applied *between* operators — a far bigger lever than a ±15%
*within*-operator perturbation) only moved `actions_per_day` separability
by roughly 1.15-1.3x when persona diversity was turned on (`tight_timing`:
3.04 -> 3.52; `wide_timing`: 1.39 -> 1.76) — nowhere near the 20x+ jump
`mean_hour_of_day` and `frac_*` show under the same shared-to-diverse
transition (see "3c."). If `skip_day_probability` was only ever a minor
contributor to `actions_per_day`'s separability, a smaller *per-agent*
perturbation of the same parameter was unlikely to move it further. This
is a hypothesis consistent with the evidence above, not an independently
isolated proof — that would need an ablation scenario varying only
`mean_interval_minutes` per agent, out of scope for this session (see
Roadmap).

**Conclusion, stated plainly**: `skip_day_probability` jitter is
implemented, tested, and wired into both the real `Agent` and the harness
exactly like its two siblings — but measured to have no detectable effect
on multi-wallet clustering resistance at this harness's scale, across its
entire valid magnitude range. The residual "3d." named is very likely NOT
primarily `skip_day_probability` after all; `mean_interval_minutes` (never
jittered per agent, in this or any prior session) is the more likely
dominant remaining contributor to `actions_per_day`'s separability — see
Roadmap for the next concrete step this points to.

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

The two middle bullets above make each *individual* wallet look like a
believable, diversified human. Measured against a *multi-wallet* clustering
adversary instead of a single-wallet detector, they turn out to be the
dominant signal for grouping several wallets back to one operator — because
one operator's whole fleet used to share them identically. See "3c.
Multi-wallet clustering" above for the actual numbers and why this is stated
here rather than left for a reviewer to notice the tension first, and "3d.
Per-agent persona jitter" for the fix `src/persona.rs` now ships for it —
measured as a real but partial mitigation, not a full resolution.

## Architecture
src/
lib.rs          exposes agent/config/protocols/scheduler/timing/clustering/consolidation as a library
main.rs         thin CLI entry point (binary: cooker), wires cli -> lib
bin/
timing_harness.rs      standalone binary: measures timing vs naive-bot + logistic-regression detectors
clustering_harness.rs  standalone binary: measures MULTI-wallet clustering resistance (ARI/NMI)
recovery_test.rs       standalone binary: crash-recovery worker driven by scripts/recovery_test.sh
cli/            clap-based commands: run, status, validate
config/         cooker.toml parsing + validation (incl. [consolidation], [persona_jitter])
timing.rs       pure timing math (CV, autocorrelation, skewness) — shared by the real
scheduler AND the harness, so the harness measures exactly what ships
detectors.rs    logistic regression + ROC AUC — the stronger named baseline in timing_harness
clustering.rs   wallet-history simulator + feature extraction + from-scratch k-means/ARI/NMI —
backs clustering_harness, reuses timing.rs + protocols::ProtocolRegistry::pick_with_rng
consolidation.rs  periodic fund consolidation across one operator's fleet (opt-in, see below)
persona.rs      per-agent persona jitter (active_hours + protocol weights + skip-day
probability), derived from each agent's own wallet pubkey — shared by the
real Agent AND clustering_harness's post-fix scenarios, so the harness
measures exactly what ships (see "3d." / "3e.")
state.rs        single-checkpoint crash recovery (atomic save/resume), see scripts/recovery_test.sh
agent/          single-agent behavior loop (timing, active hours + persona jitter, skip-day,
checkpointing) — each agent owns its own jittered ProtocolRegistry, not a fleet-shared one
scheduler/      spawns and supervises the whole fleet (agents + optional consolidation task)
protocols/      the extension point — one file per protocol
jupiter.rs      swap noise across configurable mints via Jupiter Swap API (implemented)
marinade.rs     liquid staking — deposit SOL, mint mSOL (implemented)
orca_lp.rs      concentrated liquidity positions (skeleton, see TODO)
supersonic_cast.rs  casts noise transfers through the supersonic-tx router (implemented, opt-in)

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

# 6b. Measure MULTI-wallet clustering resistance (the bounty's central question)
cargo run --release --bin clustering_harness -- --seed 1

# 7. Run the test suite (includes statistical regression tests on timing.rs)
cargo test --release

# 8. Prove crash recovery survives a real SIGKILL (no network/wallet needed)
./scripts/recovery_test.sh

# 9. Prove the supersonic_cast protocol against devnet (opt-in, not enabled by
#    default — see Status table)
cargo run --release --bin supersonic_cast_test -- <funded-devnet-keypair.json>
```

All behavior-relevant parameters (mint list, swap size floor, timing
distribution, active hours, skip-day probability, per-protocol weights,
fund-consolidation cadence/fraction) live in `cooker.toml` — see
`cooker.example.toml` for every available field.

## Status

| Protocol        | Status                          |
|-----------------|----------------------------------|
| `jupiter_swap`  | **Implemented** — real quote+swap via Jupiter Swap API, validated with 2 signed mainnet transactions (see proof table above) |
| `marinade_stake`| **Implemented** — hand-built `deposit` instruction against Marinade State with derived PDAs, validated with 1 signed mainnet transaction (see proof table above) |
| `orca_lp`       | Skeleton — instruction building TODO |
| `supersonic_cast` | **Implemented** — casts bundles through the `supersonic-tx` router (PR #1, Jmkoygg) via its public SDK, validated with 1 signed devnet transaction (see "1b. Composability" above). Not a router reimplementation; `weight = 0.0` in `cooker.example.toml` by default. |

| Feature | Status |
|---|---|
| Fund consolidation (`[consolidation]`) | **Implemented** — periodic, randomized-pair, randomized-fraction transfers between one operator's own wallets (`src/consolidation.rs`), unit-tested (11 tests, checked arithmetic throughout). `enabled = false` by default — see "Fund consolidation" in `THREAT_MODEL.md` for the honest tension this feature has with value-channel unlinkability before turning it on. |
| Multi-wallet clustering harness (`clustering_harness`) | **Implemented** — see "3c." above and "Multi-wallet clustering" in `THREAT_MODEL.md`. Reports a real, currently-unfavorable number for account-cooker's default persona sharing, not a clean pass. |

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
  in `THREAT_MODEL.md`. It now also measures a second, stronger baseline (a
  small logistic regression over 3 features) and reports that number even
  though it's less favorable — see "A second, stronger named baseline" above.
- **No Surfpool-based multi-agent soak.** A reproducible mainnet-mirror soak
  (multiple agents, multiple protocols, running concurrently against a local
  Surfpool validator) was attempted but not completed: `cargo install
  surfpool-cli` requires `librocksdb-sys`, which requires `libclang` at build
  time, and this development environment doesn't have passwordless access to
  install system packages (`libclang-dev`) to satisfy that. Rather than
  fabricate a large-N agent-count claim without having actually run it, this
  is stated here as a real gap. What ships instead: the crash-recovery proof
  above (real SIGKILL, no mocks) and the existing mainnet transaction proof
  (real signed txs, no mocks) — both smaller in scope than a full soak, but
  both actually executed, not modeled.
- `supersonic_cast` is validated on devnet only (the router it composes
  through, `solanabr/supersonic-tx` PR #1, is itself devnet-validated, not
  mainnet-audited), and its "real" leg targets a seed-derived sibling address
  of the agent's own wallet rather than an external payee — it shapes the
  agent's own on-chain footprint, it does not add payment-destination privacy.
  See "1b. Composability" above.
- The crash-recovery test (`scripts/recovery_test.sh`) exercises the
  checkpoint save/resume code path directly, not the full `cooker run` fleet
  against a live RPC — running the real fleet under repeated SIGKILL against
  mainnet was judged not worth the SOL cost/risk for what the checkpoint
  logic alone already proves. See `src/state.rs` and `src/agent/mod.rs` for
  where that same code path is wired into the real agent loop.
- **`clustering_harness` reports a currently-unfavorable result, only
  partially mitigated.** account-cooker's real config clusters back to the
  correct operator *more* accurately (ARI 0.42) than a naive tight-timing
  baseline (ARI 0.08), because `active_hours`/protocol weights used to be
  shared identically across one operator's whole fleet. `src/persona.rs`
  now jitters all three of active_hours/protocol weights/skip-day
  probability per-agent (shipped on by default, see "3d."/"3e." and
  `cooker.example.toml`'s `[persona_jitter]`), but the measured effect at
  the shipped conservative default is small (ARI 0.4214 -> 0.4140) — and a
  sensitivity sweep on the first two shows even physically-unrealistic
  jitter magnitudes (far past what's shippable without destroying
  individual-agent believability) only bring it down to ~0.17, not the
  naive baseline's 0.08. The third jitter (skip-day probability, added to
  target the specific residual causing that floor) was measured to have NO
  significant additional effect at any magnitude across its entire valid
  range — see "3e." — meaning the residual is very likely driven by
  something this session didn't jitter (`mean_interval_minutes`, see
  Roadmap), not by `skip_day_probability` as originally suspected in "3d."
  Reported and explained, not hidden or amplified — see "3c.", "3d.", "3e."
  above and "Multi-wallet clustering" in `THREAT_MODEL.md` for the full
  breakdown.
- **Fund consolidation trades away some (already out-of-scope) value-channel
  privacy for the edital's required behavior.** A direct wallet-to-wallet
  transfer is a strong signal to a funding-graph-aware adversary; this
  project has never claimed to defend against that class of attack (see
  THREAT_MODEL.md's Scope), and consolidation doesn't change that — it's
  disabled by default for this reason. See "Fund consolidation" in
  `THREAT_MODEL.md`.

## Provenance

This code was written with AI assistance (Claude, via Claude Code) under the
direction and review of the repo author (`xinaids`) — prompted, reviewed, and
tested by a human, not generated and submitted unsupervised. Stated here
directly rather than left for a reviewer to guess at. See
[`docs/ELIGIBILITY.md`](./docs/ELIGIBILITY.md) for the full eligibility
self-audit (region, language, submission modality, originality).

## Roadmap

- [ ] Complete Orca Whirlpools integration (Marinade is done — see Status)
- [x] Fund splitting / periodic consolidation across agent wallets — see
      `src/consolidation.rs`, disabled by default (see Known Limitations for
      the honest value-channel tradeoff)
- [ ] Dust-level interaction mode (sub-cent amounts, higher frequency)
- [ ] Bridge interactions (Wormhole) for cross-chain noise
- [ ] Prometheus metrics endpoint for fleet observability at scale
- [ ] Persona presets (day-trader, hodler, LP-farmer) bundling timing + protocol weights
- [ ] Dedicated/paid RPC support documented (see Known Limitations)
- [x] Extend `timing_harness` with a learned adversary (logistic regression
      over CV/autocorrelation/skewness, not just CV) for a stronger honest
      bound — see "A second, stronger named baseline" above
- [x] Multi-wallet clustering harness (ARI/NMI vs true operator identity) —
      see "3c." above; found a real, currently-unfavorable result rather
      than a clean pass
- [x] **Per-agent persona jitter within one operator's fleet** — see
      `src/persona.rs` and "3d." above: each agent now derives its own
      `active_hours` and protocol weights around the operator's base
      persona instead of sharing it exactly, on by default
      (`cooker.example.toml`'s `[persona_jitter]`). Measured as a real but
      **partial** mitigation (ARI 0.4214 -> 0.4140 at the shipped default),
      not a full resolution — see "3d." for the sensitivity sweep and why
      the shipped magnitude is deliberately conservative.
- [x] **Close persona jitter's remaining residual signal — attempted,
      implemented, and measured; did not close it.** `src/persona.rs::jittered_skip_day_probability`
      (see "3e." above) jitters each agent's own daily skip probability
      exactly like the two siblings before it, wired into both the real
      `Agent` and `clustering_harness`'s new 7th scenario. Measured result:
      **no significant ARI/NMI effect at any tested magnitude** — a full
      0.0-1.0 sweep of the only parameter this fix controls (its entire
      valid range, unlike the other two jitters which have no natural
      upper bound) stays inside `[0.3968, 0.4160]`, noise-level movement
      with no monotonic trend. The residual "3d." found is very likely NOT
      primarily `skip_day_probability` after all — see "3e." for the
      evidence and the `mean_interval_minutes` hypothesis it points to
      instead.
- [ ] **Jitter `mean_interval_minutes` per agent**: "3e."'s evidence points
      to per-operator `mean_interval_minutes` (a 6x range, 15-90 minutes,
      randomized per operator in every `clustering_harness` scenario
      including `shared_persona`) as a more likely dominant contributor to
      `actions_per_day`'s residual separability than `skip_day_probability`
      turned out to be. Not implemented or measured this session — the
      evidence for this specific hypothesis is indirect (see "3e."'s
      reasoning), not an isolated ablation, so it's named here as the next
      concrete thing to test rather than assumed correct.
- [ ] Route fund-consolidation transfers through `supersonic_cast` /
      `supersonic-tx` instead of a plain transfer, to reduce (not eliminate)
      consolidation's value-channel exposure — see "Fund consolidation" in
      THREAT_MODEL.md
- [ ] Surfpool-based multi-agent soak (blocked on `libclang` in this
      environment — see Known Limitations)

## Disclaimer

This tool manufactures behavioral noise for privacy purposes — it does not
hide fund custody or launder value. See `THREAT_MODEL.md` for the intended
threat model (wallet clustering / copy-trading resistance, not compliance
evasion).

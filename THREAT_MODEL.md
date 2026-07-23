# Threat model

## Scope

account-cooker defends against **behavioral clustering** — the class of
attack where an observer (block explorer, analytics platform, copy-trader,
or an AI model trained on-chain) links multiple wallets to the same entity,
or flags a wallet as automated/scripted, purely from *how* it behaves:
timing regularity, protocol-selection patterns, and interaction cadence.

It does **not** defend against:

- **Custody compromise.** This tool never claims to hide who controls a
  wallet's private key. If an adversary already has the key, or subpoenas
  an exchange, account-cooker offers nothing.
- **Transaction-graph unlinkability at the value level.** Funds moved by an
  agent are visible on-chain as normal transfers. Making the *value channel*
  itself unlinkable (e.g. hiding which output corresponds to which input) is
  the explicit mandate of `supersonic-tx` in this same bounty, not this tool.
- **Off-chain metadata correlation.** IP address reuse across agents,
  timing correlation with off-chain services (Telegram, Discord, exchange
  logins), or KYC linkage are entirely out of scope. An operator running
  all agents from one IP, on one schedule of *their own* choosing at the
  infrastructure level, defeats this tool regardless of on-chain behavior.
- **A well-resourced adversary with a labeled real-human dataset.** See
  "Honest limitation" below — our claim is comparative, not absolute.

## Assumptions

- The operator controls key material for every agent wallet and does not
  reuse infrastructure (RPC session, IP, hosting) in a way that re-links
  wallets at a layer this tool doesn't touch.
- The adversary observes on-chain data only (public ledger, mempool) and
  applies statistical/heuristic analysis to it — not a targeted
  investigation with subpoena power or insider access.

## Defenses (named)

| Defense | Mechanism | File |
|---|---|---|
| **No shared entropy across agents** | Each agent task owns its own `ThreadRng`; two agents never derive timing or protocol choices from a shared seed | `src/agent/mod.rs` |
| **Log-normal timing, not fixed interval** | `sample_interval_secs` draws from a log-normal distribution parameterized by configurable mean/stddev — mostly clusters near the mean but produces occasional long gaps and short bursts | `src/timing.rs` |
| **Active-hours + skip-day modeling** ⚠️ | Each agent has a waking window and a per-day probability of doing nothing — matching how real humans miss days entirely | `src/agent/mod.rs` |
| **Weighted, configurable protocol rotation** ⚠️ | Agents rotate across swaps/staking/LPing per configured weights rather than hammering one protocol | `src/protocols/mod.rs` |
| **Configurable minimum balance guard** | `min_swap_lamports` / `min_stake_lamports` stop an agent from broadcasting dust-sized, obviously-scripted transactions when underfunded — itself a clustering tell | `src/protocols/jupiter.rs`, `src/protocols/marinade.rs` |
| **No hardcoded behavior parameters** | Mint selection, swap-size floor, timing distribution, active hours, skip-day probability, per-protocol weights all live in `cooker.toml` | `cooker.example.toml` |

⚠️ These two rows make an individual wallet look human, but `clustering_harness`
(see "Multi-wallet clustering" below) found they are currently the *dominant*
signal for grouping several wallets back to one operator, because `cooker.toml`
sets `active_hours` and protocol weights once per fleet — every wallet one
operator runs shares them identically. Named here rather than left as a
pleasant-sounding row that a stronger measurement later contradicts.

## Measured, not asserted

`timing_harness` (a separate binary in this crate) measures — rather than
just implements — one specific, common chain-analysis heuristic: flagging
"suspiciously regular timing" via coefficient of variation (CV) of
consecutive action intervals.

```
cargo run --release --bin timing_harness -- --n 5000 --seed 1
```

It compares two classes head-to-head using the *exact same*
`sample_interval_secs` function the real scheduler calls:

- A **naive bot** with light jitter (±5% of its mean interval) — modeling a
  bot author who added *some* randomness but not enough.
- **account-cooker's agent** at the example config's default timing
  (`mean_interval_minutes = 45`, `stddev_interval_minutes = 30`).

Output is a detection-rate table for both classes at a fixed, documented
threshold (CV < 0.15) — not a threshold tuned to this specific data, so the
result isn't circular.

### Honest limitation

This measures resistance to **one heuristic** (fixed-cadence detection via
CV), using a **synthetic naive-bot baseline**, not a validated dataset of
real human wallet timing. We do not have access to such a dataset, so we do
not claim "indistinguishable from real humans" — only "meaningfully harder
to flag than a lightly-jittered naive bot under this specific, common
heuristic."

`timing_harness` also now runs a second, stronger baseline: a small logistic
regression (`src/detectors.rs`) over three features — CV, lag-1
autocorrelation, and skewness — trained and evaluated on a held-out split.
At the example config (mean=45min, std=30min, seed=1), this reports:

- ROC AUC on the held-out test set: **1.0000** — still fully separable at
  this sample size.
- Agent false-flag rate at a 0.5 threshold: **0.72%**, up from **0.00%**
  under the single-feature CV threshold above.

That 0.72% is reported **because it is the measured number, not because it's
favorable** — it is worse than the CV-only result. It means autocorrelation
and skewness carry a little real signal about log-normal timing that a bare
CV threshold misses, i.e., CV alone is an incomplete adversary model, exactly
as this document already said before this measurement existed. It does not
mean the timing design fails: 0.72% is still a low false-flag rate against a
learned 3-feature classifier, and this classifier remains far weaker than a
real adversary with a labeled human dataset and more features — such as
`supersonic-tx`'s 23-feature logistic regression in this same bounty, which
is the sharper reference point. Extending `timing_harness` further (more
features, a real held-out human dataset if one becomes available) remains
open work; this update is scoped honestly as "one step stronger," not "solved."

### Multi-wallet clustering — measured, and not favorable

Everything above measures "does ONE wallet look like a bot" — a binary
question. It does not measure the bounty brief's actual central claim:
*"explorers, analytics platforms, and copy-traders can't determine which
wallets belong to the same entity"* — a clustering question over several
wallets at once. `clustering_harness` (`src/clustering.rs` +
`src/bin/clustering_harness.rs`) measures that: it simulates several
operators each running several wallets (reusing `Agent::run_forever`'s
control flow — skip-day roll, active-hours gate — and the real
`timing::sample_interval_secs` / `ProtocolRegistry::pick_with_rng`, not
reimplementations), extracts a behavioral feature vector per wallet
(coefficient of variation, lag-1 autocorrelation, skewness, mean hour of
day, actions/day, and per-protocol action share), standardizes them, and
runs a from-scratch k-means (k = number of true operators) — then scores
the recovered grouping against ground truth with Adjusted Rand Index (ARI)
and Normalized Mutual Information (NMI).

```
cargo run --release --bin clustering_harness -- --seed 1
```

Five scenarios, crossing two axes (intra-operator timing spread: tight/naive
vs account-cooker's real wide log-normal; persona diversity: identical
`active_hours`/protocol weights across operators vs independently
randomized per operator) plus one fully-degenerate sanity control:

| Scenario | ARI (mean ± std, 50 trials) | NMI (mean ± std, 50 trials) |
|---|---|---|
| `identical_control` (no signal exists by construction) | 0.0012 ± 0.0271 | 0.1878 ± 0.0431 |
| `tight_timing` + `shared_persona` (naive bot-farm) | 0.0761 ± 0.0381 | 0.2963 ± 0.0511 |
| `tight_timing` + `diverse_persona` | 0.4428 ± 0.1120 | 0.6403 ± 0.0902 |
| `wide_timing` + `shared_persona` | 0.0760 ± 0.0513 | 0.2835 ± 0.0625 |
| `wide_timing` + `diverse_persona` (account-cooker's real config) | 0.4214 ± 0.1100 | 0.6112 ± 0.0900 |

**`identical_control` is a sanity check, not a privacy claim.** With every
operator byte-identical, no learnable signal exists by construction; ARI
lands at 0.0012 (chance) as it must, which is what lets the other four
numbers be trusted rather than suspected of harness bias. Its NMI (0.19, not
0) is not a bug: standard NMI, unlike ARI, is not chance-corrected for finite
samples — a small-sample positive bias at `n=48`, `k=6` is expected and
documented in `src/clustering.rs`'s test suite (`nmi_is_low_for_random_partitions_on_average`
shows the same bias shrinking at larger `n`). ARI is the more reliable of the
two numbers in a near-chance regime; both are still reported.

**account-cooker's real config (bottom row) clusters *more* accurately
(ARI 0.42) than the naive tight-timing "bot-farm" baseline (ARI 0.08) — this
is reported as measured, not reshaped until it looked better.** The 5-way
split exists specifically to explain that number instead of leaving it as an
uncomfortable mystery:

- **Timing spread (tight vs wide) barely moves ARI**: 0.0761 vs 0.0760 at
  shared persona, 0.4428 vs 0.4214 at diverse persona — both pairs
  statistically indistinguishable given the trial-to-trial std. The
  log-normal-vs-fixed-cadence design that demonstrably helps against the
  single-wallet CV/logistic-regression detectors above does **not**
  meaningfully help against a multi-wallet clustering adversary using these
  features.
- **Persona diversity (shared vs diverse `active_hours`/protocol weights)
  drives essentially all of it**: ARI moves ~5.5x (0.076 -> 0.44) holding
  timing spread fixed either way. The per-feature separability diagnostic
  printed by the same command (between/within-group variance ratio against
  true labels, independent of what k-means actually finds) confirms the
  mechanism directly — at seed 1: `mean_hour_of_day` separability goes from
  0.51 (tight+shared) / 0.27 (wide+shared) to 11.44 (tight+diverse) / 1.29
  (wide+diverse); the three `frac_*` protocol-share features go from ~0.03-0.16
  (shared, i.e. indistinguishable from noise) to 1.02-5.13 (diverse). Meanwhile
  `coefficient_of_variation` separability stays low throughout (0.03-0.68) —
  the same low per-wallet signal that lets log-normal timing evade the
  single-wallet CV/logistic-regression detectors above is *also* why timing
  alone is a weak clustering feature; that property cuts both ways.

**Why this happens mechanically**: `cooker.toml` has exactly one `[timing]`
block's `active_hours` and one set of `[[protocols]]` weights *per fleet* —
every wallet one operator runs necessarily shares that operator's
active-hours window and protocol mix, by construction. That was designed to
make each *individual* wallet look like a diversified, believable human
persona (see "Weighted protocol selection" / "Active-hours + skip-day
modeling" in `README.md`'s "Why this design"); this harness shows the same
sharing is, at fleet scale, a stronger cross-wallet fingerprint than the
timing-regularity axis this project's other measurements focus on.

**Recommendation (not implemented this session — named as real, scoped
future work, not silently deferred)**: per-agent jitter on `active_hours`
and protocol weights *within* one operator's fleet — e.g. each agent's
actual window is the operator's base window nudged by a small independent
random offset, and each agent's protocol weights are the operator's base
vector perturbed slightly per agent — would directly attack the mechanism
this harness identified as dominant, without giving up the "each wallet
looks like a diversified human" property that motivated sharing a persona
in the first place. This is a concrete, falsifiable next step for whoever
extends this work, not a vague "could be improved."

**Scope**: this measures clustering resistance on observable **behavioral**
features only (timing shape, active-hours signature, protocol mix) — the
same class of signal `timing_harness` measures for one wallet, aggregated
across many. It does **not** model an adversary with on-chain **metadata**
(funding graph, common-funder heuristics, address reuse) — see "Out of
scope, restated" below, unchanged by this measurement. It is additional to
`timing_harness`, not a replacement for it, and reports ARI/NMI as the
comparison metrics — not ROC AUC/F1/Precision@K, which describe a different
task (ranked pairwise similarity, already covered for the single-wallet
binary case by the logistic-regression baseline above).

## Fund consolidation — a feature with its own tension

The bounty brief explicitly asks for a tool that "periodically consolidates
and redistributes assets" across an operator's wallets. `src/consolidation.rs`
implements that: on a randomized cadence (the same `timing::sample_interval_secs`
log-normal sampler, at hour granularity — `mean_interval_hours = 72`,
`stddev_interval_hours = 48` by default), one randomly-chosen eligible wallet
transfers a randomized fraction (5-20% by default) of its balance to another
randomly-chosen sibling wallet in the same fleet. Neither the pair nor the
timing is fixed, so the pattern itself doesn't become a predictable
"always wallet-1 -> wallet-2, every N days" hub tell.

**This is honestly in tension with this project's own stated scope, and that
tension is not hidden.** A direct on-chain transfer between two wallets is
one of the strongest possible signals to an adversary who *does* do
funding-graph / common-funder analysis — exactly the class of attack this
document already places out of scope ("Transaction-graph unlinkability at
the value level" — see Scope above, and `supersonic-tx`'s mandate in this
same bounty). Enabling consolidation trades some of that (already
out-of-scope, already unaddressed) funding-graph exposure for the edital's
required functional behavior. The mitigations implemented here — no fixed
pair, no fixed interval, no fixed fraction — only help against a *weaker*
adversary who can't see value-graph correlation directly but would notice a
mechanically regular consolidation pattern; they do not, and are not claimed
to, make consolidation transfers unlinkable at the value level. Consequences
of this tradeoff, stated plainly:

- `enabled = false` by default (`cooker.example.toml`) — an operator who
  cares about funding-graph unlinkability more than the edital's
  consolidation requirement should leave it off.
- Routing these transfers through something like `supersonic_cast` /
  `supersonic-tx` instead of a plain `SystemProgram::transfer` is a plausible
  follow-up (this tool's own noise transfers already have that option) but
  is **not** implemented for consolidation in this session — named here as
  real future work, not silently assumed.

## Out of scope, restated

- Fund custody / key security
- Value-channel unlinkability (see `supersonic-tx`)
- Off-chain metadata (IP, hosting, exchange KYC)
- Destination-history channel: if an agent repeatedly interacts with the
  same counterparty wallet that has its own identifiable history, that
  channel isn't defended by timing alone — this is the same gap
  `supersonic-tx`'s PR explicitly calls out as a companion problem

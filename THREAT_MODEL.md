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
| **Active-hours + skip-day modeling** | Each agent has a waking window and a per-day probability of doing nothing — matching how real humans miss days entirely | `src/agent/mod.rs` |
| **Weighted, configurable protocol rotation** | Agents rotate across swaps/staking/LPing per configured weights rather than hammering one protocol | `src/protocols/mod.rs` |
| **Configurable minimum balance guard** | `min_swap_lamports` / `min_stake_lamports` stop an agent from broadcasting dust-sized, obviously-scripted transactions when underfunded — itself a clustering tell | `src/protocols/jupiter.rs`, `src/protocols/marinade.rs` |
| **No hardcoded behavior parameters** | Mint selection, swap-size floor, timing distribution, active hours, skip-day probability, per-protocol weights all live in `cooker.toml` | `cooker.example.toml` |

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

## Out of scope, restated

- Fund custody / key security
- Value-channel unlinkability (see `supersonic-tx`)
- Off-chain metadata (IP, hosting, exchange KYC)
- Destination-history channel: if an agent repeatedly interacts with the
  same counterparty wallet that has its own identifiable history, that
  channel isn't defended by timing alone — this is the same gap
  `supersonic-tx`'s PR explicitly calls out as a companion problem

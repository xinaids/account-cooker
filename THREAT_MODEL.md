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

**Recommendation (implemented this session — `src/persona.rs`, measured
below, not just claimed)**: per-agent jitter on `active_hours` and protocol
weights *within* one operator's fleet — each agent's actual window is the
operator's base window nudged by a small independent random offset, and
each agent's protocol weights are the operator's base vector perturbed
slightly per agent — attacks the mechanism this harness identified as
dominant, without giving up the "each wallet looks like a diversified
human" property that motivated sharing a persona in the first place. Both
the offset and the per-weight perturbation are derived deterministically
from the agent's own wallet pubkey (SHA-256, domain-separated —
`src/persona.rs`), never a shared RNG, so they reproduce across restarts
and stay uncorrelated between agents — this extends "No shared entropy
across agents" above rather than weakening it. `cooker.example.toml`'s new
`[persona_jitter]` block ships this ON by default at a conservative
magnitude (`active_hours_minutes = 30`, `protocol_weight_fraction = 0.15`).

A sixth `clustering_harness` scenario, `wide_timing + diverse_persona +
agent_jitter`, measures the fix directly. Operator-level generation is held
byte-identical to the PRE-fix row above (same `build_operator` call, same
RNG draw sequence — see `src/bin/clustering_harness.rs`), so any difference
isolates per-agent jitter's effect alone:

| Scenario | ARI (mean ± std, 50 trials) | NMI (mean ± std, 50 trials) |
|---|---|---|
| `wide_timing` + `diverse_persona` (PRE-fix, same row as above) | 0.4214 ± 0.1100 | 0.6112 ± 0.0900 |
| `wide_timing` + `diverse_persona` + `agent_jitter` (POST-fix, shipped default) | 0.4140 ± 0.1046 | 0.6076 ± 0.0894 |

**Honest result: real, but partial — reported as measured, not amplified to
look better.** At the shipped conservative default, ARI moves 0.4214 ->
0.4140 (NMI 0.6112 -> 0.6076): inside one trial-to-trial standard deviation
(±0.10-0.11), not a dramatic swing, and nowhere near the naive baseline's
0.08. Two things support this being a real mechanistic effect rather than
noise: the per-feature separability diagnostic moves in the predicted
direction (`mean_hour_of_day` separability 1.29 -> 0.84, `frac_orca_lp`
0.56 -> 0.34 — the two feature families identified above as dominant;
`frac_marinade_stake` moved the other way, 2.10 -> 2.38, inside
trial-to-trial noise at this sample size and reported rather than
cherry-picked around), and the effect is monotonic in jitter magnitude. A
sensitivity sweep (`--jitter-active-hours-minutes` /
`--jitter-protocol-weight-fraction`, both defaulting to the shipped values
so the command above is unaffected) confirms the dose-response — and its
limit:

| active_hours_minutes | protocol_weight_fraction | ARI (mean, 50 trials) |
|---|---|---|
| 0 (PRE-fix) | 0 | 0.4214 |
| 30 (shipped default) | 0.15 | 0.4140 |
| 60 | 0.30 | 0.3917 |
| 90 | 0.50 | 0.3451 |
| 180 | 0.80 | 0.2524 |
| 300 | 1.00 | 0.2011 |
| 600 | 1.00 | 0.1660 |

Even at `active_hours_minutes = 600` (an agent's actual window can sit up to
*10 hours* from the operator's configured persona — one meant to wake at
8am could actually be centered around 6pm, defeating the point of a
believable per-agent character) ARI only reaches 0.166 — still roughly
double the naive baseline. **Why jitter alone has a ceiling**: this fix only
perturbs the two feature families identified above as dominant, not the
full feature vector. `actions_per_day` (driven partly by
`skip_day_probability`, which `build_operator`'s `Diverse` persona
randomizes *per operator* but this fix does not jitter *per agent*) holds
roughly steady at 1.6-2.9 separability across the entire sweep — an
un-jittered residual signal that puts a floor under how low ARI can go from
this fix alone. Closing that residual is scoped future work (see
`README.md`'s Roadmap: "Close persona jitter's remaining residual signal"),
not solved here. The conservative shipped default is a deliberate choice,
not an oversight: the sweep shows that closing more of the gap costs
individual-agent believability faster than it buys clustering resistance.
Full methodology, diagnostic output, and narrative: `README.md`'s "3d.
Per-agent persona jitter — the fix, measured".

**Skip-day-probability jitter — attempted, measured, did not close the
gap.** `src/persona.rs::jittered_skip_day_probability` extends the same
deterministic-per-wallet-pubkey approach to the one parameter the
active-hours/protocol-weight fix above left un-jittered per agent: each
agent's own daily skip probability is now the operator's base value times
an independent multiplicative factor (clamped to `[0.0, 1.0]`, since a
probability — unlike a protocol weight — cannot exceed `1.0`), rather than
every agent in a fleet sharing the operator's exact value. A seventh
`clustering_harness` scenario (`... + agent_jitter + skip_day_jitter`)
measures it, holding operator-level generation and the other two jitter
magnitudes byte-identical to the sixth (POST-fix) row so any difference
isolates this fix's own effect:

| Scenario | ARI (mean ± std, 50 trials) | NMI (mean ± std, 50 trials) |
|---|---|---|
| `... + agent_jitter` (POST-fix) | 0.4140 ± 0.1046 | 0.6076 ± 0.0894 |
| `... + agent_jitter + skip_day_jitter` (this fix, shipped default) | 0.4142 ± 0.1056 | 0.6050 ± 0.0937 |

**Honest result: no measurable effect.** ARI moves by +0.0002 (noise, not
an improvement); NMI moves by -0.0026 — both far inside one trial-to-trial
standard deviation. A sweep across `skip_day_probability_fraction`'s ENTIRE
valid range (`0.0` to `1.0` — unlike the other two jitter parameters, this
one has a natural upper bound, since past `1.0` the multiplicative factor
would go negative) stays inside `[0.3968, 0.4160]` with no monotonic
trend — flat noise, not the small-but-real dose-response the
active-hours/protocol-weight sweep showed. The most likely explanation:
`actions_per_day`'s separability is plausibly dominated by
`mean_interval_minutes` (randomized per operator, a 6x range, in every
scenario including `shared_persona`), not `skip_day_probability` —
`Persona::Diverse`'s own much-larger *operator-level*
`skip_day_probability` range only moved `actions_per_day` separability
~1.15-1.3x, far short of the 20x+ jump the two genuinely-dominant features
show under the same shared-to-diverse transition. This is a hypothesis,
not an isolated proof — see README.md's "3e." for the full evidence and
reasoning, and README.md's Roadmap for the concrete next step
(`mean_interval_minutes` per-agent jitter) this points to instead.
Implemented, tested (unit tests confirm the per-agent value is genuinely
different, reproducible, and bounded; a dedicated regression test confirms
the harness's simulation consumes the per-agent value, not the operator's
shared field), and shipped — but a measured non-improvement, reported as
such rather than hidden. Full methodology, diagnostic output, and
narrative: README.md's "3e. Skip-day-probability jitter — measured, and it
doesn't move the needle".

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

## DAO governance voting — real votes, zero weight by construction

The bounty brief asks for agents that "vote in DAOs." `src/protocols/dao_vote.rs`
implements that against SPL Governance (Realms), the shared governance
program most Solana DAOs run on (`GovER5Lthms3bLBqWub97yVrMmEogzX7xNjdXpPPCVZw`,
verified live and byte-identical on both mainnet-beta and devnet).

**The safety argument, stated precisely.** `CastVote` requires a
`TokenOwnerRecord` in the target realm. `CreateTokenOwnerRecord` is
permissionless — verified directly in the program's
`process_create_token_owner_record.rs`: it checks only that the mint is valid
for the realm, never that the caller holds any of it, and always initializes
`governing_token_deposit_amount: 0`. `CastVote`
(`process_cast_vote.rs`) never checks that the resolved voter weight is
nonzero before recording a full on-chain `VoteRecordV2` and adding the weight
to the tally via `checked_add`. So a wallet that has never held a DAO's
governing token can cast a fully real, on-chain vote that is *mathematically
incapable* of changing that DAO's outcome — not "unlikely to matter," zero
by construction of the program itself, independent of which real DAO or
proposal it's pointed at.

**What would break this guarantee, named explicitly — and now enforced, not
just documented.** This protocol never issues a `DepositGoverningTokens`
instruction — it only ever creates a zero-balance record and votes with it.
But if the *operator* independently deposits real governing tokens into that
same `TokenOwnerRecord` outside this protocol (a manual, deliberate action
this code never takes on its own), a subsequent vote from that wallet would
otherwise carry that real weight. The zero-weight property is a property of
a fresh wallet in a realm it has never otherwise touched, not an invariant
`CastVote` enforces on-chain itself — so `build_and_simulate` checks it
client-side: if a `TokenOwnerRecord` already exists for this wallet in this
realm+mint, it reads the real `governing_token_deposit_amount` field
(`parse_token_owner_record_deposit_amount`) and refuses to vote if it's
nonzero, rather than silently casting a weighted vote. This was found and
closed during this feature's own review (an adversarial pass specifically
asked "how could the zero-weight guarantee break," not just "does the code
work") — the fresh-wallet path this project actually tests against was
re-verified unaffected afterward (same devnet/mainnet accounts, identical
simulation results).

**Scope limitations, named rather than silently hit.** Only realms using the
default vote-weight source (deposited SPL governing tokens, no plugin) are
supported — `voter_weight_record`/`max_voter_weight_record` are always
omitted, so a plugin-gated realm (NFT voting, Civic gateway) fails simulation
with a clear on-chain error rather than silently misbehaving. Only
`GovernanceV2`-typed governance accounts are supported, not
`MintGovernanceV2`/`ProgramGovernanceV2`/`TokenGovernanceV2` — encountered
directly during testing (10/40 real mainnet candidates sampled during
development used `MintGovernanceV2`) and rejected with a clear error rather
than mis-parsed.

**A real, evidenced search found devnet currently has no open third-party
target — reported rather than hidden.** `ProposalState::Voting` is a stored
byte that only changes when someone calls `FinalizeVote`; if nobody does
(common for abandoned test proposals), a proposal reads as "Voting" forever
even long after its actual `voting_at` + `max_voting_time` deadline. A direct
on-chain query plus a local parse of the real timestamp fields (not just the
`state` byte) found the single freshest of 1946 devnet `ProposalV2` accounts
in `Voting` state was already ~144 hours past its deadline; 102 real
candidates were tried against live devnet RPC and all were legitimately
rejected by the program itself. Two real, currently-active mainnet proposals
were found and cleanly simulated the same way. See README.md's "1c." for the
full account and proof table.

## NFT mint + transfer — fresh-mint-only, sibling never a third party

The bounty brief asks for agents that mint and move NFTs. `src/protocols/nft_flip.rs`
implements that against the Metaplex Token Metadata program
(`metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s`, verified live and byte-identical on
both mainnet-beta and devnet, same methodology as the governance program above).

**The safety argument, stated precisely — structurally stronger than `dao_vote`'s.**
`dao_vote` interacts with a real proposal it doesn't control (its safety argument is
"the vote carries zero weight"); `nft_flip` doesn't need that kind of argument because
it never touches anything it didn't just create. The mint passed to `CreateV1` is a
`Keypair::new()` generated fresh inside `build_and_simulate` on every call — there is no
config field, cache, or RPC lookup anywhere in the file that can resolve to a
pre-existing mint. It is not "unlikely" to touch a real third party's NFT; there is no
code path through which it could.

**What the optional "flip" leg does and doesn't do.** `flip_to_sibling` (default on)
transfers the freshly-minted NFT to `derive_sibling_pubkey(wallet)` — a pubkey computed
as `keypair_from_seed(Sha256(SIBLING_TAG || wallet.to_bytes()))`, discarding the derived
keypair's secret half immediately. This is the same construction as
`supersonic_cast.rs`'s `derive_master_seed` → `derive_decoy_keypair`, reimplemented
locally (not calling into `supersonic_sdk`) with an independent domain-separation tag
(`account-cooker/nft_flip/sibling/v1` vs `account-cooker/supersonic-cast/master/v1`) so
the two protocols' sibling addresses are provably unrelated even for the same wallet —
regression-tested directly (`sibling_tag_is_domain_separated_from_supersonic_cast`).
Unlike `dao_vote.rs` (`proposal_pubkey`) or `supersonic_cast.rs`
(`router_program_id`), **no field in this protocol's config can be set to an
operator- or attacker-supplied external address** — the sibling is always derived, never
read from `params`. There is no misconfiguration that turns this into a transfer to a
real third party.

**Real send caught a real bug — the kind a simulation-only review misses.** The first
devnet attempt failed on-chain with `Missing SPL token program`
(`custom program error: 0x95`): `mpl-token-metadata`'s `CreateV1Builder` does not
default `spl_token_program` to the real Token program the way it defaults
`system_program`/`sysvar_instructions` — left unset, it silently substitutes the
Metadata program's own ID as a placeholder account, which the on-chain processor then
rejects. Fixed by setting `.spl_token_program(Some(token_program))` explicitly; the
corrected version was simulated clean, sent for real on devnet, and the resulting token
balances were independently re-queried afterward (`spl-token balance`/`supply`, not just
the program's own reported success) to confirm the NFT landed exactly where designed:
0 in the wallet's own associated token account, 1 in the sibling's. See README.md's "1d."
for the full transaction, mint address, and balance-verification transcript.

**Scope limitations, named rather than silently hit.** Only the classic
`TokenStandard::NonFungible` asset type is supported — no Programmable NFT
(`token_record`/`authorization_rules` are always omitted). No off-chain metadata JSON is
hosted, uploaded, or validated by this protocol; `uri` can be empty or point anywhere,
and this code makes no claim about what a wallet or indexer displays for it. Real cost
was measured directly rather than estimated: a `flip_to_sibling = true` transaction costs
0.02169584 SOL all-in (2 new associated-token accounts + mint + metadata + master edition
rent, plus fee) — `min_balance_lamports`'s default (0.025 SOL) is set with headroom above
that measured number, not a guess.

**No mainnet send performed as part of this work — a deliberate decision, not a gap.**
The distinction that makes mainnet load-bearing for `jupiter_swap` (real liquidity and
routing that devnet simply doesn't have) doesn't apply here: Metaplex Token Metadata is
the same program, same instructions, same account layout on both clusters (verified
byte-identical above), so a real devnet mint already exercises the identical code path
`Protocol::execute` would run on mainnet. The independent `spl-token` re-verification
(balances, not just the program's reported success) confirms that proof is conclusive on
its own terms. Sending again on mainnet would spend real SOL to re-confirm a result
already established, not to test anything new — same posture as `dao_vote.rs`'s "no real
mainnet vote" call.

## Out of scope, restated

- Fund custody / key security
- Value-channel unlinkability (see `supersonic-tx`)
- Off-chain metadata (IP, hosting, exchange KYC)
- Destination-history channel: if an agent repeatedly interacts with the
  same counterparty wallet that has its own identifiable history, that
  channel isn't defended by timing alone — this is the same gap
  `supersonic-tx`'s PR explicitly calls out as a companion problem

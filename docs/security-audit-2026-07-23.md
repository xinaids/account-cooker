# account-cooker Security Audit — Follow-up (new code only)

**Date**: 2026-07-23
**Scope**: Only the three files added since the previous audit that haven't been
reviewed yet: `src/consolidation.rs`, `src/clustering.rs` +
`src/bin/clustering_harness.rs`, and the `pick_with_rng` addition to
`src/protocols/mod.rs`. `jupiter.rs`, `marinade.rs`, `supersonic_cast.rs`, and
`state.rs` were covered in
[`security-audit-2026-07-20.md`](./security-audit-2026-07-20.md) and are not
re-reviewed here except where this new code touches them directly.
**Auditor**: claude (via `/audit-solana`)
**Commit at audit time**: working tree on `feat/jupiter-swap-scaffold`, clean
(`git status` — no uncommitted changes at audit start)
**Action taken**: initial pass was report-only. H-1 has since been fixed (see
"H-1 — FIXED" below); the 3 Lows remain open, documented as backlog per
explicit instruction not to fix them in this pass.

## Summary

| Severity | Count | Status |
|---|---|---|
| Critical | 0 | — |
| High | 1 | **Fixed** |
| Medium | 0 | — |
| Low | 3 | Open (backlog) |

The one High finding was in `src/consolidation.rs` — the file flagged as most
important — and was reported first, before any other findings, per
instruction. It is now fixed; see "H-1 — FIXED" below.

## What this audit confirms is right (the user's specific checklist)

Before the findings: the five things explicitly asked about `consolidation.rs`,
checked directly against the code and its test suite:

| Question asked | Verdict |
|---|---|
| Value validation (`fraction_min`/`max`, overflow in balance-fraction math) | **Clean.** `compute_transfer_lamports` uses `checked_sub` for the reserve, explicitly validates the sampled fraction is in `[0.0, 1.0]`, and clamps the result to never exceed the available balance. 6 dedicated unit tests, including `u64::MAX` and zero-balance inputs. `CookerConfig::validate()` also rejects `fraction_min`/`fraction_max` outside `[0,1]` or `fraction_min > fraction_max` at config-load time. |
| Can the same wallet be source AND destination? | **No — verified.** `pick_source_destination`'s destination search explicitly excludes the source index (`if candidate != source`), with a dedicated regression test for the one case that could otherwise infinite-loop (exactly 2 wallets, 1 eligible). |
| Insufficient-balance handling | **Clean.** Every transfer is built, then passed to `rpc.simulate_transaction()` before `send_and_confirm_transaction()` — the same fail-closed pattern already validated in `marinade.rs`/`supersonic_cast.rs`. A stale balance snapshot can't reach the network as a bad transfer; simulation catches it first. |
| Hardcoded values that should be config | **Clean inside this file** — `enabled`, `mean_interval_hours`, `stddev_interval_hours`, `fraction_min/max`, `min_balance_lamports`, `reserve_lamports` are all `ConsolidationConfig` fields with sane defaults. **However, see H-1 below**: the file calls into a *shared* function whose clamp bounds are hardcoded for a different timescale than consolidation needs — that's the one real hardcoded-value problem this audit found, and it's significant. |
| Opt-in mode (`enabled=false`) — any leakage when disabled? | **Clean, defense in depth.** `scheduler::run_fleet` only spawns the consolidation task at all when `cfg.consolidation.enabled`; `run_consolidation_loop` independently re-checks `cfg.enabled` before its loop. No RPC calls, no keypair loads, no logging happen on this path when disabled. |

---

## High

### H-1 — FIXED: `consolidation.rs` reused the noise-scheduler's `[30s, 12h]` clamp for an hours-scale cadence — ~99.6% of ticks collapsed to a near-fixed 12-hour interval, defeating the feature's own anti-fixed-interval design goal

**Files**: `src/consolidation.rs:111-115` (the call site), root cause in
`src/timing.rs:27` (the hardcoded clamp bounds)

```rust
// consolidation.rs — converts the configured hours to minutes, then calls
// the SAME sampler the per-action noise scheduler uses:
let sleep_secs = sample_interval_secs(
    cfg.mean_interval_hours * 60.0,      // e.g. 72h -> 4320 "minutes"
    cfg.stddev_interval_hours * 60.0,    // e.g. 48h -> 2880 "minutes"
    &mut rand::thread_rng(),
);
```

```rust
// timing.rs — the shared sampler, whose final clamp is a bare literal:
let draw = dist.sample(rng);
draw.clamp(30.0, 60.0 * 60.0 * 12.0) as u64   // hard ceiling: 12 hours
```

`sample_interval_secs`'s `[30s, 12h]` clamp is documented and correct for its
original purpose — the per-action noise scheduler, where the example config's
mean is 45 **minutes**. A 12-hour ceiling there is a generous safety valve
("a pathological draw never produces a near-zero or multi-day wait").

`consolidation.rs` reuses the identical function for a cadence whose
documented and default mean is 72 **hours** (`mean_interval_hours = 72.0`,
`stddev_interval_hours = 48.0` in `ConsolidationConfig`'s defaults). At that
scale, the 12-hour ceiling isn't a generous safety valve anymore — it's
tighter than the median of the intended distribution, so it clips almost
every draw.

**Verified, not just calculated by hand.** Replicated `lognormal_mu_sigma` and
the exact clamp in a standalone Python simulation (200,000 draws, matching
the `rand_distr::LogNormal` parameterization):

```
mu=12.2815 sigma=0.6064 median_hours=59.91
fraction of raw draws EXCEEDING the 12h clamp ceiling: 99.57%
fraction of raw draws below the 30s clamp floor: 0.0000%
mean of ACTUAL (post-clamp) interval used by the code: 11.99 hours (config asked for 72h)
fraction of actual sleeps that land EXACTLY at the 12h ceiling: 99.57%
```

At the shipped default config, **the real-world consolidation cadence is a
near-constant 12 hours** (99.57% of ticks land on exactly the clamp value),
not the documented log-normal(mean=72h, std=48h). The realized mean interval
(11.99h) is roughly 6x tighter than configured.

**Why this matters for this specific project**: this isn't a generic off-by-N
bug — it silently inverts the exact property `consolidation.rs`'s own module
doc comment and `THREAT_MODEL.md` claim for this feature:

> "Deliberately NOT the same wallet pair every time and NOT a fixed interval:
> a consolidation pattern that always moves funds wallet-1 -> wallet-2 on a
> fixed schedule is itself a clustering tell" — `consolidation.rs:12-15`

> "Neither the pair nor the timing is fixed, so the pattern itself doesn't
> become a predictable 'always wallet-1 -> wallet-2, every N days' hub tell."
> — `THREAT_MODEL.md`

A ~99.6%-of-the-time-exactly-12-hours cadence *is* a fixed interval in
practice — it is precisely the "suspiciously regular timing" signature this
project's own `timing_harness` measures via coefficient of variation (the
CV < 0.15 heuristic that catches "naive bot" in that harness's own output
table). The mitigation this module claims to provide against a funding-graph
observer noticing a periodic hub pattern is, as shipped, largely not present.

**No test would have caught this**: `sample_interval_secs`'s own regression
test (`sample_interval_matches_configured_mean_std` in `timing.rs`) only
exercises minute-scale inputs (mean=45, std=30 — well inside the clamp).
`consolidation.rs`'s test module covers only the pure, deterministic
`pick_source_destination` and `compute_transfer_lamports` — never the
sleep-interval sampling path itself.

**Not a fund-safety bug** — no loss, no panic, no crash. This is a
detection-resistance defect in the feature whose entire stated purpose is
detection resistance, shipped silently (no error, no warning, no failing
test).

**Recommendation**:
1. Give hour-scale callers their own clamp range (e.g. something like
   `[1h, 14d]`), either as a second function (`sample_interval_secs_hours` or
   similar) or by making the clamp bounds a parameter instead of the current
   hardcoded literal in `timing.rs:27`.
2. Add a regression test mirroring `sample_interval_matches_configured_mean_std`
   but at consolidation's actual scale (mean=72h/std=48h) asserting the
   realized mean/std stays reasonably close to configured — this specific
   test would have caught the bug immediately, the same way the existing one
   already guards the minute-scale case.

**Fix applied** (2026-07-23, same day, follow-up commit — not yet committed,
left in working tree):

- `timing.rs` now has a private `sample_interval_secs_clamped(mean_secs,
  std_secs, min_secs, max_secs, rng)` core that takes the clamp range as a
  parameter instead of a hardcoded literal. `sample_interval_secs` (minutes,
  per-action noise) is now a thin wrapper over it with the original `[30s,
  12h]` clamp — unchanged behavior, verified by the pre-existing
  `sample_interval_matches_configured_mean_std` /
  `sample_interval_respects_clamp_bounds` tests still passing byte-for-byte.
- A new `sample_interval_hours(mean_hours, std_hours, rng)` wraps the same
  core with a `[60s, 4 weeks]` clamp, sized for consolidation's actual scale.
  Bounds were chosen empirically (Python replica of `lognormal_mu_sigma` +
  clamp, 200k draws at the default mean=72h/std=48h): realized mean 71.90h
  against a 72h target, 0.00% of draws pinned at the ceiling — versus the old
  clamp's 11.99h realized mean and 99.6% pinned. A tighter `[60s, 2 weeks]`
  candidate was also checked and already fixes the bug (0.21% pinned,
  71.76h realized) but 4 weeks was chosen for extra headroom against
  higher-stddev configs than the default.
- `consolidation.rs`'s `run_consolidation_loop` now calls
  `sample_interval_hours(cfg.mean_interval_hours, cfg.stddev_interval_hours,
  &mut rand::thread_rng())` directly (hours in, no more `* 60.0` minutes
  conversion at the call site). The module's top-of-file doc comment was
  updated to stop describing the old (buggy) "reuse the same function"
  behavior.
- New regression tests in `timing.rs`, run at consolidation's actual
  mean=72h/std=48h scale (the exact scale the original bug required to
  reproduce — the existing minute-scale test never exercised this path):
  - `sample_interval_hours_matches_configured_mean` — asserts the empirical
    mean of 20,000 draws lands within 15% of the configured 72h target, and
    separately asserts fewer than 5% of draws are pinned at the clamp
    ceiling. Either assertion alone would have caught the original bug (old
    behavior: mean off by ~83%, 99.6% pinned).
  - `sample_interval_hours_respects_clamp_bounds` — 5,000 draws, asserts
    every sample stays within `[60s, 4 weeks]`.
- Verified post-fix: `cargo build --release`, `cargo test --release` (39
  passed, up from 37 — the 2 new tests), `cargo clippy --all-targets -- -D
  warnings` (0 warnings), `cargo fmt --check` (clean). All four green.
- Not changed: `src/config/mod.rs` (`ConsolidationConfig` fields/defaults
  untouched — this was a sampling-function bug, not a config-schema issue),
  `src/scheduler/mod.rs` (call site unaffected).

---

## Low

### L-1: `fraction_min > fraction_max` would panic in `consolidation_tick`, guarded only by `CookerConfig::validate()` in a different module

**File**: `src/consolidation.rs:158`

```rust
let fraction = rng.gen_range(cfg.fraction_min..=cfg.fraction_max);
```

`Rng::gen_range` on a `RangeInclusive` panics if `start > end`. The only thing
preventing `fraction_min > fraction_max` from reaching this line today is
`CookerConfig::validate()` (`src/config/mod.rs:161-163`) — a different module,
called exactly once, at the top of `scheduler::run_fleet`, before this code
path is ever reached. Not reachable via the current single call site.

Worth flagging specifically because `run_consolidation_loop`'s own doc comment
overclaims here: *"this function re-checks so it fails safe if ever called
directly"* — true for `cfg.enabled` and `wallets.len() < 2` (both are
re-checked at `consolidation.rs:106`), but **not** true for the fraction
relationship, which isn't re-checked anywhere in this file. A future direct
caller (a test, a new CLI subcommand, a library consumer constructing
`ConsolidationConfig` by hand) would panic instead of failing safely as the
comment implies. Same risk category as the prior audit's L-1
(`ProtocolRegistry::pick()`'s fallback `.unwrap()`) — a guard living in a
different function than the risk it guards, with no compiler-enforced link
between them.

Note: out-of-`[0,1]`-range values (as opposed to a reversed range) are
already safe — `compute_transfer_lamports` independently validates the
*sampled* fraction and returns `None` for anything outside `[0.0, 1.0]`,
regardless of what `fraction_min`/`fraction_max` were.

**Recommendation**: normalize defensively at the top of `consolidation_tick`,
e.g. `let (lo, hi) = if cfg.fraction_min <= cfg.fraction_max { (cfg.fraction_min, cfg.fraction_max) } else { (cfg.fraction_max, cfg.fraction_min) };` — cheap, and actually delivers the fail-safe behavior the doc comment already claims.

### L-2: No validated relationship between `reserve_lamports` and `min_balance_lamports` — a misconfiguration silently makes consolidation a permanent no-op

**Files**: `src/config/mod.rs` (`CookerConfig::validate`), `src/consolidation.rs`
(`compute_transfer_lamports`)

`min_balance_lamports` (eligibility floor to be picked as a source) and
`reserve_lamports` (amount always left behind) are independently configurable
with no cross-validation. If an operator sets `reserve_lamports >=
min_balance_lamports`, every wallet that clears the eligibility bar will still
fail `compute_transfer_lamports`'s `balance.checked_sub(reserve_lamports)`
(underflows → `None`), so `consolidation_tick` returns `Ok(None)` — logged
only at `debug` level ("skipped this tick (no eligible move)"). An operator
running with default log levels would see consolidation simply never happen,
with no indication why. The shipped defaults are sane and don't trigger this
(`min_balance_lamports = 10_000_000` > `reserve_lamports = 5_000_000`) — this
is a footgun for manual misconfiguration, not a live bug, and it fails safe
(no funds moved, no panic) rather than dangerously.

**Recommendation**: add
`if self.consolidation.reserve_lamports >= self.consolidation.min_balance_lamports { bail!(...) }`
alongside the existing fraction checks in `CookerConfig::validate()`.

### L-3: Sequential (non-batched) `get_balance` RPC calls in `consolidation_tick`

**File**: `src/consolidation.rs:135-138`

```rust
let mut balances = Vec::with_capacity(wallets.len());
for w in wallets {
    balances.push(rpc.get_balance(&w.keypair.pubkey()).await?);
}
```

One serialized RPC round-trip per wallet, every tick. For the project's
stated scale target ("thousands of concurrent agents"), this is N sequential
calls against a cadence that — once H-1 above is fixed — will run roughly
every 72 hours (or, as currently shipped, every ~12h). Not a security issue;
already broadly covered by the README's general RPC-rate-limiting caveat, and
the low tick frequency limits real-world impact either way.

**Recommendation**: batch via `rpc.get_multiple_accounts()` (chunked at 100
pubkeys per Solana RPC limits) instead of N individual `get_balance` calls.

---

## `src/clustering.rs` + `src/bin/clustering_harness.rs` — both specific asks verified

### Determinism: clean

Full read of both files plus a targeted grep found exactly one `thread_rng()`
reference in the entire audit scope, and it's the correct one — `pick()`'s
own live-agent default in `protocols/mod.rs:49`, not anything inside the
clustering code:

```
$ grep -n "thread_rng" src/clustering.rs src/bin/clustering_harness.rs src/protocols/mod.rs
src/protocols/mod.rs:49:        self.pick_with_rng(&mut rand::thread_rng())
src/protocols/mod.rs:55:    /// agent's `thread_rng()` — this is the exact weighting logic the live
```

Every RNG stream in the harness traces back to the single `--seed` CLI
argument through explicit, deterministic derivations:
- `simulate_trial`: `ChaCha8Rng::seed_from_u64(seed)` for the trial, then
  `ChaCha8Rng::seed_from_u64(rng.gen())` per agent — each agent gets an
  independent stream, but the sub-seed itself is drawn from the deterministic
  parent stream, so the whole tree is reproducible from one top-level seed
  (and this mirrors `THREAT_MODEL.md`'s real "no shared entropy across
  agents" production defense on purpose).
- `run_trial`'s k-means RNG: `ChaCha8Rng::seed_from_u64(seed ^ 0x9E37_79B9_7F4A_7C15)`
  — a standard decorrelation constant (golden-ratio hash-combine), still
  fully deterministic.
- `main`'s per-trial seeds: `args.seed.wrapping_add((t as u64).wrapping_mul(0x0000_0100_0000_01B3))`
  — `wrapping_*` used correctly (no overflow panic risk), still deterministic.
- `clustering.rs` itself never constructs an RNG — every function takes
  `rng: &mut impl Rng` as a parameter, so determinism is entirely inherited
  from the caller. Confirmed by reading the full file, not just the grep.

### ARI/NMI math: correct, checked against known cases by hand — not just by re-reading the existing tests

Independently re-derived both formulas against the standard definitions and
worked two cases by hand, then cross-checked against the code's actual
control flow (including the degenerate-denominator branches) and the existing
test suite:

- **Identical partitions** (`[0,0,0,1,1,1,2,2,2]` vs itself): by hand,
  `sum_ij=9, sum_a=sum_b=9, total=36, expected=2.25, max_index=9` →
  `ARI=(9-2.25)/(9-2.25)=1.0`. NMI: `mi=1.0986, h_true=h_pred=1.0986` →
  `NMI=1.0`. Both match the code path and `ari_is_one_for_identical_partitions`
  / `nmi_is_one_for_identical_partitions`.
- **3-cluster ground truth collapsed to 1 predicted cluster** (the common
  degenerate failure mode): by hand, `sum_ij=9, sum_a=9, sum_b=36, total=36,
  expected=9, max_index=22.5` → `ARI=(9-9)/(22.5-9)=0.0` (denominator
  non-degenerate here, no special-case branch triggered). NMI: `h_pred=0`
  (single trivial cluster) triggers the `h_true==0.0 || h_pred==0.0` branch,
  correctly returns `0.0` since only one side is trivial. Both match
  `ari_is_zero_for_a_single_cluster_prediction` /
  `nmi_is_zero_when_prediction_is_a_single_trivial_cluster`.
- ARI's formula matches the standard Hubert & Arabie (1985) pair-counting
  definition (sklearn's `adjusted_rand_score` uses the same one); NMI's
  matches the geometric-mean normalization the code's own comment claims
  (`NMI = MI / sqrt(H(U)H(V))`), with the degenerate-entropy fallback
  correctly requiring *both* sides trivial for a `1.0` (not just one).
- `n_choose_2` guards `n < 2` before the `n - 1` subtraction, so it can't
  underflow-panic on `n=0` in a debug build.
- All 9 clustering-related unit tests pass (`cargo test`, see below).

No math errors found in either metric.

### Checked but clean (not findings, ruled out explicitly)

- `extract_features` on a single-action history (`actions.len() == 1`, so
  `.windows(2)` yields an empty `intervals`): does **not** produce `NaN`.
  Verified by reading `timing.rs` — `coefficient_of_variation` explicitly
  returns `0.0` for `n==0`, `autocorrelation_lag1` and `skewness` both
  explicitly return `0.0` for `n<2`. Checked because it looked like a
  plausible gap given `extract_features` only special-cases the fully-empty
  case explicitly; it isn't one.
- `build_operator`'s randomized `active_hours` generation (`clustering_harness.rs`):
  confirmed by range analysis that `start < end` always holds given the
  generator's bounds (`start` ∈ `[5,10]`, `span` ∈ `[8,14]`, `end =
  (start+span).min(23)`), so it can never produce the degenerate
  zero-width/inverted active window `CookerConfig::validate()` would reject
  in a real config.
- `kmeans`/`adjusted_rand_index`/`normalized_mutual_info` use `assert!`/
  `assert_eq!` on internal invariants (mismatched-length inputs, `k==0`,
  empty data) rather than returning `Result`. This is technically a
  panic-on-violation pattern, but it operates only on this module's own
  internally-generated simulation data, never live RPC/user input — the same
  category the prior audit explicitly scoped `detectors.rs`/`timing_harness.rs`
  out of individual findings for ("measurement/research code, not the
  fund-custody path"). Noted for completeness, not raised as a finding, for
  consistency with that precedent.

---

## `src/protocols/mod.rs` — `pick_with_rng` verified against the specific ask

**Verdict: `pick()`'s behavior is unchanged.**

```rust
pub fn pick(&self) -> &dyn Protocol {
    self.pick_with_rng(&mut rand::thread_rng())
}

pub fn pick_with_rng(&self, rng: &mut impl Rng) -> &dyn Protocol {
    let total: f64 = self.entries.iter().map(|(w, _)| w).sum();
    let mut roll = rng.gen_range(0.0..total);
    for (w, p) in &self.entries {
        if roll < *w { return p.as_ref(); }
        roll -= w;
    }
    self.entries.choose(rng).unwrap().1.as_ref()
}
```

This is a pure extract-and-delegate refactor: the weighted-selection
algorithm (sum weights → uniform roll in `[0, total)` → walk cumulative
weights → floating-point-edge-case fallback) is byte-for-byte the same logic,
just one level deeper. `pick()` now does nothing but supply `thread_rng()` to
it. Provably behavior-identical for the live path — there's no new branch,
no changed weighting, no changed fallback condition. `entries.push((c.weight.max(0.0001), proto))`
(the min-weight clamp preventing a zero/negative weight from degenerating the
distribution) is untouched, in `from_config`, unaffected by this change.

**Carried-over note, not a new issue**: the prior audit's L-1
(`.unwrap()` on the empty-registry fallback, safe only because
`CookerConfig::validate()` — a different module — rejects an empty
`protocols` list) still exists. It now lives inside `pick_with_rng` (shared
by both `pick()` and the harness's direct calls) rather than directly inside
the old `pick()` body — relocated, not introduced, not newly reachable.
Flagging the move for the record, not re-counting it as a new Low in this
report's summary table.

---

## Automated analysis results

Initial pass (before the H-1 fix):

```
cargo fmt --check                                    → clean, no diff
cargo build --all-targets                             → success, 0 warnings
cargo test --release                                  → 37 passed; 0 failed
cargo clippy --all-targets -- -W clippy::all -D warnings → 0 warnings
```

Re-run after the H-1 fix (below):

```
cargo build --release                                 → success, 0 warnings
cargo test --release                                   → 39 passed; 0 failed  (+2: the new sample_interval_hours tests)
cargo clippy --all-targets -- -D warnings               → 0 warnings
cargo fmt --check                                       → clean, no diff
```

`git diff 64e0bbd..HEAD -- Cargo.toml` shows only a new `[[bin]]` entry for
`clustering_harness` — no new dependencies were introduced for any of the
three files in scope, so the dependency-level findings from the prior audit
(M-2, M-3, L-3, L-5 there) are unaffected and not re-run here. The H-1 fix
also introduced no new dependencies.

## Not applicable to this codebase

Same as the prior audit: this is an off-chain Rust agent, not an on-chain
Anchor/Pinocchio program. Account discriminators, PDA bump storage,
account-close/revival, `#[account]`/`#[derive(Accounts)]` constraints, and CU
profiling don't apply and are omitted rather than padded.

## Recommendations, in priority order

1. ~~**H-1** — give `consolidation.rs`'s sleep-interval sampling its own clamp
   range instead of reusing the noise-scheduler's `[30s, 12h]` bounds, and add
   a regression test at consolidation's actual (hours) scale.~~ **Fixed
   2026-07-23** — see "Fix applied" under H-1 above.
2. **L-1** (open, backlog) — normalize `fraction_min`/`fraction_max` defensively inside
   `consolidation_tick` so the existing doc comment's fail-safe claim is
   actually true.
3. **L-2** (open, backlog) — validate `reserve_lamports < min_balance_lamports` in
   `CookerConfig::validate()`.
4. **L-3** (open, backlog) — batch balance fetches via `get_multiple_accounts` if/when fleet
   size makes the sequential RPC pattern a practical bottleneck.

## Sign-off

- [x] Automated analysis (build, test, clippy, fmt) — complete, all clean
- [x] Manual review of all three in-scope files/areas — complete
- [x] Every specific question in the audit request answered explicitly above
      (consolidation validation, self-transfer, insufficient-balance handling,
      hardcoded config, opt-in leakage; clustering determinism; ARI/NMI math;
      `pick_with_rng` regression check)
- [x] H-1 verified empirically (standalone simulation), not just derived by
      hand
- [x] H-1 fixed (2026-07-23): `sample_interval_hours` added to `timing.rs`
      with an hours-appropriate `[60s, 4 weeks]` clamp, `consolidation.rs`
      switched to it, 2 new regression tests added, build/test/clippy/fmt
      all re-verified clean. Not committed — left in working tree per
      instruction.
- [ ] L-1/L-2/L-3 — left open as backlog, not fixed in this pass per
      explicit instruction

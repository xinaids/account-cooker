//! Per-agent persona jitter.
//!
//! `clustering_harness` (`src/clustering.rs` + `src/bin/clustering_harness.rs`)
//! found that `cooker.toml` setting exactly one `[timing].active_hours` window
//! and one set of `[[protocols]]` weights *per fleet* — every wallet one
//! operator runs sharing them byte-for-byte — is the dominant signal for
//! grouping several wallets back to one operator (ARI 0.42 vs a naive
//! tight-timing baseline's 0.08; see THREAT_MODEL.md's "Multi-wallet
//! clustering"). This module is the named-but-not-yet-implemented
//! recommendation from that section: each agent derives its OWN active-hours
//! window, protocol weights, and daily skip probability from the operator's
//! base persona, nudged by a small perturbation. The third of those
//! (`jittered_skip_day_probability`) closes a specific residual signal
//! README.md's "3d." found AFTER the first two shipped: `actions_per_day`
//! (driven partly by `skip_day_probability`, which `build_operator`'s
//! `Diverse` persona randomizes per OPERATOR but nothing jittered per AGENT)
//! held roughly steady at 1.6-2.9 separability across the entire
//! active-hours/protocol-weight jitter sweep — this function is that
//! missing per-agent jitter.
//!
//! The perturbation is derived deterministically from each agent's own
//! wallet pubkey (`persona_seed` below), never a shared/global RNG —
//! consistent with THREAT_MODEL.md's existing "No shared entropy across
//! agents" defense (`src/agent/mod.rs`: each agent task owns its own
//! `ThreadRng`), which this extends to persona derivation rather than
//! weakening it. Two consequences of that choice:
//!   1. Reproducible: the same wallet always derives the same persona, so a
//!      restarted agent doesn't drift to a new "character" mid-fleet-lifetime.
//!   2. Uncorrelated: no two agents' jitter can be linked through a shared
//!      random stream, even if an adversary suspected the derivation scheme.
//!
//! Uses the wallet's PUBLIC key only (not the full keypair, unlike
//! `protocols::supersonic_cast::derive_master_seed`, which derives an
//! actual secret) — there is no secrecy property to protect here, only
//! reproducible-but-uncorrelated behavioral variation, so there's no reason
//! to touch private key material for it.

use crate::config::ProtocolConfig;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use sha2::{Digest, Sha256};

const DOMAIN_ACTIVE_HOURS: &[u8] = b"account-cooker/persona-jitter/active-hours/v1";
const DOMAIN_PROTOCOL_WEIGHTS: &[u8] = b"account-cooker/persona-jitter/protocol-weights/v1";
const DOMAIN_SKIP_DAY: &[u8] = b"account-cooker/persona-jitter/skip-day/v1";

const MINUTES_PER_DAY: i64 = 24 * 60;

/// Derives a deterministic 64-bit seed from an agent's own identity bytes
/// (in practice, its wallet's public key) plus a domain-separation tag, so
/// the active-hours offset and the protocol-weight perturbation for the
/// SAME agent don't reuse identical randomness (each call gets its own
/// fresh `ChaCha8Rng` stream), while two DIFFERENT agents never collide
/// short of a SHA-256 collision.
fn persona_seed(identity_bytes: &[u8], domain: &[u8]) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(identity_bytes);
    let digest = hasher.finalize();
    u64::from_le_bytes(
        digest[0..8]
            .try_into()
            .expect("sha256 digest is 32 bytes, at least 8 are always available"),
    )
}

/// Derives one agent's own active-hours window (in minutes-since-midnight)
/// from the operator's base `[timing].active_hours` (hours) plus a small,
/// per-agent deterministic offset.
///
/// A SINGLE offset is drawn and applied to BOTH boundaries equally, so the
/// window's WIDTH (how many minutes/day this agent is "awake") is preserved
/// exactly — only where in the day it sits shifts. This models early-bird
/// vs. night-owl variation among an operator's agents while leaving each
/// individual agent's own waking-hours duration exactly what the operator
/// configured (the property that made active-hours modeling a believable
/// single-wallet persona in the first place — see README.md's "Why this
/// design").
///
/// `jitter_minutes <= 0.0` disables jitter entirely and returns the
/// operator's exact window converted to minutes — this is the pre-fix
/// behavior, and is also what a `cooker.toml` predating
/// `[persona_jitter]` gets by default (see `PersonaJitterConfig`).
///
/// The drawn offset itself (not each boundary independently) is clamped to
/// keep the whole window inside a single day (`[0, 1440]` minutes), so
/// width is preserved EXACTLY in every case — including a jitter magnitude
/// large enough to reach a day boundary for a window that sits close to
/// one (e.g. `active_hours = [8, 23]` has only 60 minutes of headroom on
/// the end side). Clamping each boundary independently instead would
/// silently shrink width for exactly those near-boundary operators — the
/// same class of "distribution silently collapses at a ceiling" hazard
/// already documented for a different clamp in `src/timing.rs` (see
/// `docs/security-audit-2026-07-23.md`, H-1) — so this project deliberately
/// avoids repeating it here.
pub fn jittered_active_hours_minutes(
    active_hours: [u8; 2],
    jitter_minutes: f64,
    identity_bytes: &[u8],
) -> (u32, u32) {
    let base_start = active_hours[0] as i64 * 60;
    let base_end = active_hours[1] as i64 * 60;

    let magnitude = jitter_minutes.max(0.0).round() as i64;
    if magnitude <= 0 {
        return (base_start as u32, base_end as u32);
    }

    let mut rng = ChaCha8Rng::seed_from_u64(persona_seed(identity_bytes, DOMAIN_ACTIVE_HOURS));
    let offset = rng.gen_range(-magnitude..=magnitude);

    let min_offset = -base_start;
    let max_offset = MINUTES_PER_DAY - base_end;
    let clamped_offset = offset.clamp(min_offset, max_offset);

    let shifted_start = base_start + clamped_offset;
    let shifted_end = base_end + clamped_offset;

    (shifted_start as u32, shifted_end as u32)
}

/// Derives one agent's own protocol weights from the operator's base
/// `[[protocols]]` weights: each weight is perturbed INDEPENDENTLY by a
/// factor drawn from `[1 - jitter_fraction, 1 + jitter_fraction]`.
///
/// A base weight of exactly `0.0` (a protocol the operator explicitly
/// disabled, e.g. `supersonic_cast`'s default in `cooker.example.toml`)
/// always stays exactly `0.0` — a multiplicative perturbation of zero is
/// zero regardless of factor, so jitter can never accidentally re-enable a
/// disabled protocol for some agent.
///
/// Deliberately NOT rescaled back to the operator's exact weight sum
/// afterward: `ProtocolRegistry::pick_with_rng` picks by
/// `weight_i / sum(current weights)`, so a uniform rescale of the whole
/// vector cancels out of that ratio and would change nothing about
/// selection probability — it would be purely cosmetic, and for a
/// single-protocol registry it would silently cancel ALL jitter back to
/// exactly the base weight (rescaling a one-element vector to a target sum
/// forces that exact value, no matter the perturbation). Leaving weights
/// unrescaled keeps every weight's perturbation independent and avoids that
/// degenerate case.
///
/// `jitter_fraction <= 0.0` disables jitter entirely and returns the
/// operator's exact weights — the pre-fix behavior, and also what a
/// `cooker.toml` predating `[persona_jitter]` gets by default.
pub fn jittered_protocol_weights(
    protocols: &[ProtocolConfig],
    jitter_fraction: f64,
    identity_bytes: &[u8],
) -> Vec<f64> {
    if protocols.is_empty() || jitter_fraction <= 0.0 {
        return protocols.iter().map(|p| p.weight).collect();
    }

    let mut rng = ChaCha8Rng::seed_from_u64(persona_seed(identity_bytes, DOMAIN_PROTOCOL_WEIGHTS));
    let fraction = jitter_fraction.min(1.0);

    protocols
        .iter()
        .map(|p| {
            let factor = rng.gen_range((1.0 - fraction)..=(1.0 + fraction));
            (p.weight * factor).max(0.0)
        })
        .collect()
}

/// Derives one agent's own daily skip probability from the operator's base
/// `[timing].skip_day_probability`: the base probability perturbed by a
/// factor drawn independently from `[1 - jitter_fraction, 1 + jitter_fraction]`
/// — the same multiplicative-perturbation shape as `jittered_protocol_weights`,
/// applied to a single scalar instead of a vector.
///
/// Clamped to `[0.0, 1.0]` after perturbation. This is the one genuinely new
/// piece of range-handling relative to this module's other two functions: a
/// protocol weight has no natural upper bound (only `jittered_protocol_weights`'s
/// `.max(0.0)` floor), but a probability cannot legitimately exceed `1.0`
/// either. An operator configured near either end of the valid range (e.g.
/// `skip_day_probability = 0.95`) combined with a large jitter fraction could
/// otherwise push the perturbed value outside `[0.0, 1.0]` — which
/// `rand::Rng::gen_bool` (the real consumer, in `Agent::run_forever`) panics
/// on.
///
/// `jitter_fraction <= 0.0` disables jitter entirely and returns the
/// operator's exact base probability — the pre-fix behavior, and also what a
/// `cooker.toml` predating this field's introduction to `PersonaJitterConfig`
/// gets by default (backward-compatible, matching the other two functions in
/// this module).
pub fn jittered_skip_day_probability(
    base_probability: f64,
    jitter_fraction: f64,
    identity_bytes: &[u8],
) -> f64 {
    if jitter_fraction <= 0.0 {
        return base_probability;
    }

    let mut rng = ChaCha8Rng::seed_from_u64(persona_seed(identity_bytes, DOMAIN_SKIP_DAY));
    let fraction = jitter_fraction.min(1.0);
    let factor = rng.gen_range((1.0 - fraction)..=(1.0 + fraction));
    (base_probability * factor).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protocols(weights: &[f64]) -> Vec<ProtocolConfig> {
        weights
            .iter()
            .enumerate()
            .map(|(i, &weight)| ProtocolConfig {
                name: format!("protocol_{i}"),
                weight,
                params: toml::Table::new(),
            })
            .collect()
    }

    #[test]
    fn zero_jitter_reproduces_base_active_hours_exactly() {
        assert_eq!(
            jittered_active_hours_minutes([8, 23], 0.0, b"any-agent"),
            (8 * 60, 23 * 60)
        );
    }

    #[test]
    fn zero_jitter_reproduces_base_protocol_weights_exactly() {
        let base = protocols(&[3.0, 1.0, 0.0]);
        assert_eq!(
            jittered_protocol_weights(&base, 0.0, b"any-agent"),
            vec![3.0, 1.0, 0.0]
        );
    }

    #[test]
    fn zero_jitter_reproduces_base_skip_day_probability_exactly() {
        assert_eq!(jittered_skip_day_probability(0.15, 0.0, b"any-agent"), 0.15);
    }

    #[test]
    fn same_identity_gives_identical_jitter_on_every_call() {
        let a1 = jittered_active_hours_minutes([8, 23], 30.0, b"wallet-fixed-bytes");
        let a2 = jittered_active_hours_minutes([8, 23], 30.0, b"wallet-fixed-bytes");
        assert_eq!(
            a1, a2,
            "same wallet identity must derive the same window every time"
        );

        let base = protocols(&[3.0, 1.0, 1.0]);
        let w1 = jittered_protocol_weights(&base, 0.15, b"wallet-fixed-bytes");
        let w2 = jittered_protocol_weights(&base, 0.15, b"wallet-fixed-bytes");
        assert_eq!(
            w1, w2,
            "same wallet identity must derive the same weights every time"
        );

        let p1 = jittered_skip_day_probability(0.15, 0.15, b"wallet-fixed-bytes");
        let p2 = jittered_skip_day_probability(0.15, 0.15, b"wallet-fixed-bytes");
        assert_eq!(
            p1, p2,
            "same wallet identity must derive the same skip-day probability every time"
        );
    }

    #[test]
    fn different_identities_get_different_active_hours() {
        let a = jittered_active_hours_minutes([8, 23], 30.0, b"agent-aaaa");
        let b = jittered_active_hours_minutes([8, 23], 30.0, b"agent-bbbb");
        assert_ne!(
            a, b,
            "two different agent identities under the same operator config \
             should not collide on jitter"
        );
    }

    #[test]
    fn different_identities_get_different_protocol_weights() {
        let base = protocols(&[3.0, 1.0, 1.0]);
        let a = jittered_protocol_weights(&base, 0.15, b"agent-aaaa");
        let b = jittered_protocol_weights(&base, 0.15, b"agent-bbbb");
        assert_ne!(
            a, b,
            "two different agent identities under the same operator config \
             should not collide on jitter"
        );
    }

    #[test]
    fn different_identities_get_different_skip_day_probability() {
        let a = jittered_skip_day_probability(0.15, 0.15, b"agent-aaaa");
        let b = jittered_skip_day_probability(0.15, 0.15, b"agent-bbbb");
        assert_ne!(
            a, b,
            "two different agent identities under the same operator config \
             should not collide on jitter"
        );
    }

    #[test]
    fn active_hours_jitter_stays_within_bound_and_preserves_window_width() {
        let base_width_minutes = 23 * 60 - 8 * 60;
        for i in 0..500u32 {
            let identity = i.to_le_bytes();
            let (start, end) = jittered_active_hours_minutes([8, 23], 30.0, &identity);
            assert!(
                (start as i64 - 8 * 60).abs() <= 30,
                "start {start} outside configured 30-minute jitter bound"
            );
            assert!(
                (end as i64 - 23 * 60).abs() <= 30,
                "end {end} outside configured 30-minute jitter bound"
            );
            assert_eq!(
                end - start,
                base_width_minutes as u32,
                "a shared offset on both boundaries must preserve window width"
            );
        }
    }

    /// Regression test for the near-day-boundary case: `[8, 23]` has only
    /// 60 minutes of headroom on the end side (23:00 -> 24:00). A jitter
    /// magnitude (300) far larger than that headroom would, under a
    /// naive "clamp each boundary independently" implementation, shrink
    /// window width for any agent whose draw pushes past the boundary —
    /// exactly the bug this function's offset-clamping design avoids (see
    /// its doc comment). Width must stay exact regardless of magnitude.
    #[test]
    fn active_hours_jitter_preserves_width_even_with_magnitude_past_day_boundary() {
        let base_width_minutes = (23 * 60 - 8 * 60) as u32;
        for i in 0..500u32 {
            let identity = i.to_le_bytes();
            let (start, end) = jittered_active_hours_minutes([8, 23], 300.0, &identity);
            assert_eq!(
                end - start,
                base_width_minutes,
                "window width must be preserved exactly even when the jitter \
                 magnitude exceeds the day-boundary headroom"
            );
            assert!(start <= end);
            assert!((end as i64) <= MINUTES_PER_DAY);
        }
    }

    /// A window that already spans the full day has zero headroom in
    /// either direction: the offset must clamp to exactly 0 (no panic, no
    /// out-of-range window), regardless of jitter magnitude.
    #[test]
    fn active_hours_jitter_is_a_no_op_for_a_full_day_window() {
        for i in 0..50u32 {
            let identity = i.to_le_bytes();
            let (start, end) = jittered_active_hours_minutes([0, 24], 300.0, &identity);
            assert_eq!((start, end), (0, 24 * 60));
        }
    }

    /// Regression test for systematic bias: offsets are drawn uniformly
    /// from `[-30, 30]` minutes, so across many independent agent
    /// identities the mean offset should land near zero. Standard error of
    /// the mean at n=5000 for this distribution is ~0.24 minutes; 3 minutes
    /// is a >10-sigma tolerance — generous enough to never flake, tight
    /// enough to catch a real bug (e.g. a sign error making every agent
    /// skew the same direction).
    #[test]
    fn active_hours_jitter_offset_mean_is_near_zero_across_many_agents() {
        let n = 5000u64;
        let mut total_offset = 0i64;
        for i in 0..n {
            let identity = i.to_le_bytes();
            let (start, _end) = jittered_active_hours_minutes([8, 23], 30.0, &identity);
            total_offset += start as i64 - 8 * 60;
        }
        let mean_offset = total_offset as f64 / n as f64;
        assert!(
            mean_offset.abs() < 3.0,
            "expected near-zero mean jitter offset, got {mean_offset}"
        );
    }

    #[test]
    fn protocol_weight_jitter_never_unmutes_an_explicitly_disabled_protocol() {
        let base = protocols(&[3.0, 1.0, 0.0]); // third protocol disabled (weight 0.0)
        for i in 0..200u32 {
            let identity = i.to_le_bytes();
            let out = jittered_protocol_weights(&base, 0.15, &identity);
            assert_eq!(
                out[2], 0.0,
                "a weight=0.0 protocol must stay exactly 0.0 after jitter"
            );
        }
    }

    #[test]
    fn protocol_weight_jitter_stays_within_configured_multiplicative_bound() {
        let base = protocols(&[3.0, 1.0, 2.0]);
        for i in 0..500u32 {
            let identity = i.to_le_bytes();
            let out = jittered_protocol_weights(&base, 0.15, &identity);
            for (p, &jittered) in base.iter().zip(out.iter()) {
                let lo = p.weight * 0.85 - 1e-9;
                let hi = p.weight * 1.15 + 1e-9;
                assert!(
                    (lo..=hi).contains(&jittered),
                    "jittered weight {jittered} outside [{lo}, {hi}] for base {}",
                    p.weight
                );
            }
        }
    }

    /// Regression test for systematic bias in weight jitter, mirroring the
    /// active-hours mean-offset test above. A single-protocol registry
    /// isolates the raw perturbation factor directly (base weight 1.0, so
    /// the output IS the factor) — this also happens to be the case that
    /// would silently break (factor forced to exactly 1.0 every time) if a
    /// sum-preserving rescale were reintroduced here, so this test doubles
    /// as a regression guard against that.
    #[test]
    fn protocol_weight_jitter_mean_factor_is_near_one_across_many_agents() {
        let base = protocols(&[1.0]);
        let n = 5000u64;
        let mut total = 0.0;
        for i in 0..n {
            let identity = i.to_le_bytes();
            let out = jittered_protocol_weights(&base, 0.15, &identity);
            total += out[0];
        }
        let mean_factor = total / n as f64;
        assert!(
            (mean_factor - 1.0).abs() < 0.02,
            "expected near-unbiased mean perturbation factor, got {mean_factor}"
        );
    }

    #[test]
    fn skip_day_probability_jitter_stays_within_configured_multiplicative_bound() {
        let base_probability = 0.15;
        for i in 0..500u32 {
            let identity = i.to_le_bytes();
            let out = jittered_skip_day_probability(base_probability, 0.15, &identity);
            let lo = base_probability * 0.85 - 1e-9;
            let hi = base_probability * 1.15 + 1e-9;
            assert!(
                (lo..=hi).contains(&out),
                "jittered skip-day probability {out} outside [{lo}, {hi}] for base {base_probability}"
            );
        }
    }

    /// Regression test for systematic bias in skip-day jitter, mirroring the
    /// active-hours mean-offset and protocol-weight mean-factor tests above.
    /// Uses a base probability (0.15) with enough headroom on both sides
    /// that `[0.0, 1.0]` clamping never triggers at this jitter fraction
    /// (0.15 * [0.85, 1.15] = [0.1275, 0.1725], nowhere near either bound) —
    /// otherwise the clamp itself would bias the mean and this test would be
    /// checking clamp behavior instead of perturbation bias. Standard error
    /// of the mean at n=5000 for this distribution is ~0.00018; 0.005 is a
    /// 25-sigma-plus tolerance — generous enough to never flake, tight
    /// enough to catch a real bug (e.g. a sign error making every agent
    /// skew the same direction).
    #[test]
    fn skip_day_probability_jitter_mean_is_near_base_across_many_agents() {
        let base_probability = 0.15;
        let n = 5000u64;
        let mut total = 0.0;
        for i in 0..n {
            let identity = i.to_le_bytes();
            total += jittered_skip_day_probability(base_probability, 0.15, &identity);
        }
        let mean = total / n as f64;
        assert!(
            (mean - base_probability).abs() < 0.005,
            "expected near-unbiased mean around base probability {base_probability}, got {mean}"
        );
    }

    /// Regression test for the one piece of range-handling this function
    /// adds beyond its two siblings: output must never leave `[0.0, 1.0]`,
    /// even for base probabilities near either boundary combined with a
    /// large jitter fraction (the case that would otherwise make
    /// `rand::Rng::gen_bool` panic in `Agent::run_forever`).
    #[test]
    fn skip_day_probability_jitter_never_escapes_valid_probability_range() {
        let cases = [(0.95, 0.5), (0.02, 1.0), (1.0, 1.0), (0.0, 1.0)];
        for (base, fraction) in cases {
            for i in 0..200u32 {
                let identity = i.to_le_bytes();
                let out = jittered_skip_day_probability(base, fraction, &identity);
                assert!(
                    (0.0..=1.0).contains(&out),
                    "jittered skip-day probability {out} outside [0.0, 1.0] for base={base} fraction={fraction}"
                );
            }
        }
    }
}

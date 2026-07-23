use rand::Rng;
use rand_distr::{Distribution, LogNormal};

/// Converts a desired arithmetic mean/std (in seconds) into the (mu, sigma)
/// parameters of the underlying log-normal distribution.
pub fn lognormal_mu_sigma(mean_secs: f64, std_secs: f64) -> (f64, f64) {
    let variance = std_secs.powi(2);
    let mu = (mean_secs.powi(2) / (mean_secs.powi(2) + variance).sqrt()).ln();
    let sigma = ((variance / mean_secs.powi(2)) + 1.0).ln().sqrt();
    (mu, sigma)
}

/// Core log-normal sampler shared by every timing caller: draws one interval
/// (in seconds) from a log-normal distribution parameterized by a desired
/// arithmetic mean/stddev (also in seconds), clamped to `[min_secs, max_secs]`
/// so a pathological draw never produces a near-zero or unbounded wait.
///
/// The clamp range is a parameter, not a constant, because callers operate at
/// very different timescales (minutes for per-action noise vs. hours/days for
/// fund consolidation) — see `sample_interval_secs` and `sample_interval_hours`
/// below. Reusing one hardcoded clamp across both scales previously collapsed
/// ~99.6% of consolidation draws to a fixed ceiling instead of the configured
/// distribution (docs/security-audit-2026-07-23.md, H-1).
fn sample_interval_secs_clamped(
    mean_secs: f64,
    std_secs: f64,
    min_secs: f64,
    max_secs: f64,
    rng: &mut impl Rng,
) -> u64 {
    let mean_secs = mean_secs.max(1.0);
    let std_secs = std_secs.max(1.0);
    let (mu, sigma) = lognormal_mu_sigma(mean_secs, std_secs);
    let dist =
        LogNormal::new(mu, sigma).unwrap_or_else(|_| LogNormal::new(mean_secs.ln(), 0.5).unwrap());
    let draw = dist.sample(rng);
    draw.clamp(min_secs, max_secs) as u64
}

/// Draws one interval (in seconds) from a log-normal distribution parameterized
/// by a desired arithmetic mean/stddev (also in minutes), clamped to
/// `[30s, 12h]` — sized for per-action noise-transaction timing.
///
/// This is the single source of truth for agent timing — both `Agent` (the
/// real scheduler) and `timing_harness` (the measurement tool below) call
/// this same function, so the harness measures exactly what ships.
pub fn sample_interval_secs(mean_minutes: f64, std_minutes: f64, rng: &mut impl Rng) -> u64 {
    sample_interval_secs_clamped(
        mean_minutes * 60.0,
        std_minutes * 60.0,
        30.0,
        60.0 * 60.0 * 12.0,
        rng,
    )
}

/// Draws one interval (in seconds) from a log-normal distribution parameterized
/// by a desired arithmetic mean/stddev (in hours), clamped to `[1 minute, 4 weeks]`
/// — sized for fund-consolidation cadence (`ConsolidationConfig`'s default
/// mean is 72h), not per-action noise. Using `sample_interval_secs`'s
/// `[30s, 12h]` clamp here was the H-1 bug: at a 72h mean, that ceiling is
/// tighter than the distribution's median, so it pinned ~99.6% of draws to a
/// near-fixed 12-hour interval instead of the intended spread.
pub fn sample_interval_hours(mean_hours: f64, std_hours: f64, rng: &mut impl Rng) -> u64 {
    sample_interval_secs_clamped(
        mean_hours * 3600.0,
        std_hours * 3600.0,
        60.0,
        60.0 * 60.0 * 24.0 * 28.0,
        rng,
    )
}

/// Coefficient of variation (stdev / mean) of a slice of intervals. A
/// perfectly fixed cadence has CV ≈ 0; this is the statistic the naive-bot
/// detector in `timing_harness` keys on, since "suspiciously regular timing"
/// is the most common real-world chain-analysis heuristic for bot detection.
pub fn coefficient_of_variation(intervals: &[f64]) -> f64 {
    let n = intervals.len() as f64;
    if n == 0.0 {
        return 0.0;
    }
    let mean: f64 = intervals.iter().sum::<f64>() / n;
    if mean == 0.0 {
        return 0.0;
    }
    let variance: f64 = intervals.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    variance.sqrt() / mean
}

/// Lag-1 autocorrelation of a slice of intervals. A naive bot with
/// independent jitter around a fixed mean has autocorrelation near 0; some
/// detectors use this as a second signal alongside CV since CV alone is a
/// single, well-known heuristic that's easy to defeat by construction
/// without actually being harder to distinguish on other axes.
pub fn autocorrelation_lag1(intervals: &[f64]) -> f64 {
    let n = intervals.len();
    if n < 2 {
        return 0.0;
    }
    let mean: f64 = intervals.iter().sum::<f64>() / n as f64;
    let mut num = 0.0;
    for i in 0..n - 1 {
        num += (intervals[i] - mean) * (intervals[i + 1] - mean);
    }
    let den: f64 = intervals.iter().map(|x| (x - mean).powi(2)).sum();
    if den == 0.0 {
        0.0
    } else {
        num / den
    }
}

/// Sample skewness of a slice of intervals. Log-normal timing is right-
/// skewed by construction (occasional long gaps); a fixed-mean-plus-jitter
/// bot built from a symmetric distribution is not. This is a third,
/// independent feature a learned classifier can use beyond CV.
pub fn skewness(intervals: &[f64]) -> f64 {
    let n = intervals.len() as f64;
    if n < 2.0 {
        return 0.0;
    }
    let mean: f64 = intervals.iter().sum::<f64>() / n;
    let variance: f64 = intervals.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n;
    let std = variance.sqrt();
    if std == 0.0 {
        return 0.0;
    }
    let m3: f64 = intervals.iter().map(|x| (x - mean).powi(3)).sum::<f64>() / n;
    m3 / std.powi(3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    /// Regression test: over many draws, the empirical mean/std of samples
    /// should land close to the configured target. This locks the
    /// mean/std -> mu/sigma conversion so a future refactor can't silently
    /// break the "log-normal timing" claim without a test failing.
    #[test]
    fn sample_interval_matches_configured_mean_std() {
        let mut rng = ChaCha8Rng::seed_from_u64(42);
        let mean_minutes = 45.0;
        let std_minutes = 30.0;
        let n = 20_000;
        let samples: Vec<f64> = (0..n)
            .map(|_| sample_interval_secs(mean_minutes, std_minutes, &mut rng) as f64)
            .collect();
        let mean: f64 = samples.iter().sum::<f64>() / n as f64;
        let variance: f64 = samples.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64;
        let std = variance.sqrt();

        let target_mean = mean_minutes * 60.0;
        let target_std = std_minutes * 60.0;

        // Clamping (30s floor, 12h ceiling) means we won't match target
        // exactly; tolerance is loose but tight enough to catch a broken
        // mean/std -> mu/sigma conversion.
        assert!(
            (mean - target_mean).abs() / target_mean < 0.15,
            "sample mean {mean} too far from target {target_mean}"
        );
        assert!(
            (std - target_std).abs() / target_std < 0.35,
            "sample std {std} too far from target {target_std}"
        );
    }

    #[test]
    fn sample_interval_respects_clamp_bounds() {
        let mut rng = ChaCha8Rng::seed_from_u64(7);
        for _ in 0..5_000 {
            let s = sample_interval_secs(45.0, 30.0, &mut rng);
            assert!((30..=60 * 60 * 12).contains(&s));
        }
    }

    /// Regression test for H-1 (docs/security-audit-2026-07-23.md):
    /// `consolidation.rs` used to call `sample_interval_secs` — whose
    /// `[30s, 12h]` clamp is sized for minute-scale noise timing — at
    /// consolidation's actual hours scale (default mean=72h, std=48h). That
    /// ceiling sat below the distribution's median, so ~99.6% of draws
    /// collapsed to exactly 12h instead of following the configured
    /// log-normal(72h, 48h) spread. This test locks the empirical mean at
    /// that exact scale close to the configured target, and separately
    /// checks the draws aren't dominated by clamp-ceiling pinning — either
    /// assertion alone would have caught the original bug.
    #[test]
    fn sample_interval_hours_matches_configured_mean() {
        let mut rng = ChaCha8Rng::seed_from_u64(99);
        let mean_hours = 72.0;
        let std_hours = 48.0;
        let n = 20_000;
        let samples: Vec<f64> = (0..n)
            .map(|_| sample_interval_hours(mean_hours, std_hours, &mut rng) as f64)
            .collect();
        let mean_secs: f64 = samples.iter().sum::<f64>() / n as f64;
        let target_secs = mean_hours * 3600.0;

        assert!(
            (mean_secs - target_secs).abs() / target_secs < 0.15,
            "sample mean {:.1}h too far from configured target {mean_hours}h — clamp may be \
             truncating the distribution again",
            mean_secs / 3600.0
        );

        let max_secs = 60.0 * 60.0 * 24.0 * 28.0;
        let pinned_at_ceiling = samples.iter().filter(|&&s| s >= max_secs).count();
        assert!(
            (pinned_at_ceiling as f64 / n as f64) < 0.05,
            "{pinned_at_ceiling}/{n} draws pinned at the clamp ceiling — this is exactly the H-1 \
             failure mode (a near-fixed interval instead of the configured spread)"
        );
    }

    #[test]
    fn sample_interval_hours_respects_clamp_bounds() {
        let mut rng = ChaCha8Rng::seed_from_u64(13);
        for _ in 0..5_000 {
            let s = sample_interval_hours(72.0, 48.0, &mut rng);
            assert!((60..=60 * 60 * 24 * 28).contains(&s));
        }
    }

    #[test]
    fn coefficient_of_variation_zero_for_constant_series() {
        let constant = vec![100.0; 10];
        assert_eq!(coefficient_of_variation(&constant), 0.0);
    }

    #[test]
    fn autocorrelation_zero_for_constant_series() {
        let constant = vec![100.0; 10];
        assert_eq!(autocorrelation_lag1(&constant), 0.0);
    }

    #[test]
    fn skewness_zero_for_symmetric_series() {
        let symmetric = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert!(skewness(&symmetric).abs() < 1e-9);
    }
}

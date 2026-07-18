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

/// Draws one interval (in seconds) from a log-normal distribution parameterized
/// by a desired arithmetic mean/stddev (also in seconds), clamped to a sane
/// range so a pathological draw never produces a near-zero or multi-day wait.
///
/// This is the single source of truth for agent timing — both `Agent` (the
/// real scheduler) and `timing_harness` (the measurement tool below) call
/// this same function, so the harness measures exactly what ships.
pub fn sample_interval_secs(mean_minutes: f64, std_minutes: f64, rng: &mut impl Rng) -> u64 {
    let mean_secs = (mean_minutes * 60.0).max(1.0);
    let std_secs = (std_minutes * 60.0).max(1.0);
    let (mu, sigma) = lognormal_mu_sigma(mean_secs, std_secs);
    let dist =
        LogNormal::new(mu, sigma).unwrap_or_else(|_| LogNormal::new(mean_secs.ln(), 0.5).unwrap());
    let draw = dist.sample(rng);
    draw.clamp(30.0, 60.0 * 60.0 * 12.0) as u64
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

    #[test]
    fn coefficient_of_variation_zero_for_constant_series() {
        let constant = vec![100.0; 10];
        assert_eq!(coefficient_of_variation(&constant), 0.0);
    }
}

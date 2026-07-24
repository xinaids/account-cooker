//! Multi-wallet behavioral clustering support — the machinery behind
//! `clustering_harness` (src/bin/clustering_harness.rs).
//!
//! `timing_harness` answers "does this ONE wallet look like a bot?" — a
//! binary question. It does not answer the bounty brief's actual central
//! question: "can an observer group MULTIPLE wallets back to the same
//! operator by behavior alone?" — a clustering question over many wallets
//! at once. This module provides that: a wallet-history simulator that
//! reuses the real scheduling logic, a behavioral feature extractor built
//! on `timing.rs`'s existing statistics, a from-scratch k-means, and
//! from-scratch Adjusted Rand Index / Normalized Mutual Information —
//! matching this crate's existing pattern of hand-rolled statistical
//! tooling (see `detectors.rs`'s logistic regression) instead of pulling in
//! an ML crate.
//!
//! See `src/bin/clustering_harness.rs` for how these pieces are assembled
//! into the actual experiment (scenario design, honest scope statement).

use crate::config::{ProtocolConfig, TimingConfig};
use crate::protocols::ProtocolRegistry;
use crate::timing::{
    autocorrelation_lag1, coefficient_of_variation, sample_interval_secs, skewness,
};
use rand::Rng;

pub const SECS_PER_DAY: u64 = 86_400;
pub const SECS_PER_HOUR: u64 = 3_600;
pub const SECS_PER_MINUTE: u64 = 60;

/// One simulated operator's persona — the same shape of config a real
/// operator would put in `cooker.toml`'s `[timing]` + `[[protocols]]`
/// tables. All K agents belonging to one operator share this struct,
/// mirroring "agents from the same operator share the operator's config."
#[derive(Clone)]
pub struct OperatorConfig {
    pub timing: TimingConfig,
    pub protocols: Vec<ProtocolConfig>,
}

/// One completed action recorded during a simulated wallet's history.
pub struct SimAction {
    pub timestamp_secs: u64,
    pub protocol: String,
}

/// Simulates one wallet's action history under `operator`'s config until
/// `actions_target` actions are recorded.
///
/// Mirrors the control flow of `Agent::run_forever` (src/agent/mod.rs):
/// roll skip-day once per virtual day, gate on `active_hours`, otherwise
/// act — choosing a protocol via the real `ProtocolRegistry::pick_with_rng`
/// — and draw the next interval via the real `timing::sample_interval_secs`.
/// Both are the exact functions the live scheduler calls, not
/// reimplementations, so this harness measures what actually ships.
///
/// Diverges from `Agent::run_forever` in two ways, both required to make a
/// seeded, reproducible harness instead of a live wall-clock loop:
///   1. Uses a synthetic virtual clock (seconds since a per-wallet random
///      phase offset) instead of `chrono::Local::now()`.
///   2. Jumps directly to the next relevant boundary (next day / next
///      active window) instead of polling on `Agent`'s recheck timer —
///      same observable outcome, instant instead of wall-clock-paced.
pub fn simulate_wallet(
    operator: &OperatorConfig,
    registry: &ProtocolRegistry,
    actions_target: usize,
    rng: &mut impl Rng,
) -> Vec<SimAction> {
    let active_window_minutes = (
        operator.timing.active_hours[0] as u32 * 60,
        operator.timing.active_hours[1] as u32 * 60,
    );
    simulate_wallet_with_window(
        operator,
        registry,
        actions_target,
        active_window_minutes,
        operator.timing.skip_day_probability,
        rng,
    )
}

/// Same simulation as `simulate_wallet`, but with the active-hours window
/// AND the daily skip probability given explicitly instead of derived from
/// `operator.timing`. Lets a caller (e.g. `clustering_harness`'s
/// persona-jitter scenarios) supply a per-AGENT jittered window and/or skip
/// probability — see `persona::jittered_active_hours_minutes` /
/// `persona::jittered_skip_day_probability` — instead of the operator's
/// shared ones, while everything else (protocol choice, interval sampling)
/// is identical to `simulate_wallet`.
///
/// `simulate_wallet` is a thin wrapper around this function passing the
/// operator's own window and skip probability through unmodified, so it is
/// unaffected by this split: for integer-hour boundaries, `floor(x/60) <
/// 60k` iff `floor(x/3600) < k` for any non-negative integers `x, k`, so
/// comparing minute-of-day against an exact hour boundary expressed in
/// minutes gives bit-identical results to the old hour-granularity
/// comparison, and forwarding `operator.timing.skip_day_probability`
/// unchanged gives bit-identical results to the old internal-lookup version.
pub fn simulate_wallet_with_window(
    operator: &OperatorConfig,
    registry: &ProtocolRegistry,
    actions_target: usize,
    active_window_minutes: (u32, u32),
    skip_day_probability: f64,
    rng: &mut impl Rng,
) -> Vec<SimAction> {
    let mut actions = Vec::with_capacity(actions_target);

    // Random phase offset: real operators don't all start their fleets at
    // the same virtual instant, and this avoids every wallet's first
    // decision being made from identical clock=0 state.
    let mut clock: u64 = rng.gen_range(0..SECS_PER_DAY);
    let mut current_day = clock / SECS_PER_DAY;
    let mut skip_today = rng.gen_bool(skip_day_probability);

    let active_start = active_window_minutes.0 as u64;
    let active_end = active_window_minutes.1 as u64;

    // Safety valve for pathological configs (e.g. skip_day_probability
    // near 1.0 combined with a near-zero active window). Never expected to
    // trigger for the realistic ranges `clustering_harness` generates, but
    // guarantees termination instead of an unbounded loop.
    let mut guard = 0u64;
    const GUARD_LIMIT: u64 = 5_000_000;

    while actions.len() < actions_target {
        guard += 1;
        if guard > GUARD_LIMIT {
            break;
        }

        let day = clock / SECS_PER_DAY;
        if day != current_day {
            current_day = day;
            skip_today = rng.gen_bool(skip_day_probability);
        }

        let minute_of_day = (clock % SECS_PER_DAY) / SECS_PER_MINUTE;

        if skip_today {
            clock = (current_day + 1) * SECS_PER_DAY;
            continue;
        }
        if minute_of_day < active_start {
            clock = current_day * SECS_PER_DAY + active_start * SECS_PER_MINUTE;
            continue;
        }
        if minute_of_day >= active_end {
            clock = (current_day + 1) * SECS_PER_DAY + active_start * SECS_PER_MINUTE;
            continue;
        }

        let protocol = registry.pick_with_rng(rng).name().to_string();
        actions.push(SimAction {
            timestamp_secs: clock,
            protocol,
        });

        let interval = sample_interval_secs(
            operator.timing.mean_interval_minutes,
            operator.timing.stddev_interval_minutes,
            rng,
        );
        clock += interval;
    }

    actions
}

/// Behavioral feature vector extracted from one wallet's observed action
/// history — this is what an external observer (block explorer, analytics
/// platform) plausibly has: timestamps and which protocol was touched, NOT
/// funding source or other on-chain metadata (see module docs and
/// `clustering_harness`'s scope statement).
///
/// Features: [coefficient_of_variation, autocorrelation_lag1, skewness,
/// mean_hour_of_day, actions_per_day, then one fraction per entry in
/// `protocol_names` giving that protocol's share of this wallet's actions].
/// The first three reuse `timing.rs` directly — the exact statistics
/// `timing_harness` already uses for its single-wallet detector.
pub fn extract_features(actions: &[SimAction], protocol_names: &[&str]) -> Vec<f64> {
    if actions.is_empty() {
        return vec![0.0; 5 + protocol_names.len()];
    }

    let intervals: Vec<f64> = actions
        .windows(2)
        .map(|w| (w[1].timestamp_secs - w[0].timestamp_secs) as f64)
        .collect();

    let cv = coefficient_of_variation(&intervals);
    let autocorr = autocorrelation_lag1(&intervals);
    let skew = skewness(&intervals);

    let n = actions.len() as f64;
    let mean_hour: f64 = actions
        .iter()
        .map(|a| ((a.timestamp_secs % SECS_PER_DAY) / SECS_PER_HOUR) as f64)
        .sum::<f64>()
        / n;

    let first = actions.first().map(|a| a.timestamp_secs).unwrap_or(0);
    let last = actions.last().map(|a| a.timestamp_secs).unwrap_or(0);
    let span_secs = last.saturating_sub(first);
    let actions_per_day = if span_secs > 0 {
        n / (span_secs as f64 / SECS_PER_DAY as f64)
    } else {
        0.0
    };

    let mut features = vec![cv, autocorr, skew, mean_hour, actions_per_day];
    for name in protocol_names {
        let frac = actions.iter().filter(|a| a.protocol == *name).count() as f64 / n;
        features.push(frac);
    }
    features
}

/// One-way-ANOVA-style separability score per feature dimension: ratio of
/// between-group variance to within-group variance, computed against the
/// TRUE operator labels (not a clustering algorithm's output). This is a
/// diagnostic, not a clustering metric — it answers "which features
/// actually carry operator signal in this scenario," independent of
/// whether k-means manages to find it, which is what makes a surprising
/// ARI/NMI result explainable instead of just reported as a mystery
/// number. Larger = more separable by that feature alone; `f64::INFINITY`
/// means zero within-group spread (every operator has a single exact
/// value — only possible in a fully degenerate scenario).
pub fn feature_separability(rows: &[Vec<f64>], labels: &[usize]) -> Vec<f64> {
    let n_features = rows[0].len();
    let n_groups = labels.iter().max().map(|&m| m + 1).unwrap_or(0);
    let n = rows.len() as f64;

    (0..n_features)
        .map(|j| {
            let grand_mean: f64 = rows.iter().map(|r| r[j]).sum::<f64>() / n;

            let mut group_sums = vec![0.0; n_groups];
            let mut group_counts = vec![0usize; n_groups];
            for (row, &g) in rows.iter().zip(labels.iter()) {
                group_sums[g] += row[j];
                group_counts[g] += 1;
            }

            let between: f64 = group_sums
                .iter()
                .zip(group_counts.iter())
                .filter(|&(_, &count)| count > 0)
                .map(|(sum, &count)| {
                    let group_mean = sum / count as f64;
                    count as f64 * (group_mean - grand_mean).powi(2)
                })
                .sum();

            let within: f64 = rows
                .iter()
                .zip(labels.iter())
                .map(|(row, &g)| {
                    let group_mean = group_sums[g] / group_counts[g] as f64;
                    (row[j] - group_mean).powi(2)
                })
                .sum();

            if within > 1e-12 {
                between / within
            } else {
                f64::INFINITY
            }
        })
        .collect()
}

// ---------- k-means (from scratch, no ML crate — see module docs) ----------

pub struct KMeansResult {
    pub assignments: Vec<usize>,
    pub inertia: f64,
}

fn squared_dist(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b.iter()).map(|(x, y)| (x - y).powi(2)).sum()
}

/// K-means++ initialization: first centroid uniform-random, subsequent
/// centroids chosen with probability proportional to squared distance from
/// the nearest already-chosen centroid — spreads initial centroids out,
/// reducing the chance of a bad local optimum vs plain random init. This
/// is a *more* favorable starting point for the adversary this harness
/// models, which is the conservative choice for our own privacy claim.
fn kmeans_plus_plus_init(data: &[Vec<f64>], k: usize, rng: &mut impl Rng) -> Vec<Vec<f64>> {
    let mut centroids: Vec<Vec<f64>> = Vec::with_capacity(k);
    centroids.push(data[rng.gen_range(0..data.len())].clone());

    while centroids.len() < k {
        let dists: Vec<f64> = data
            .iter()
            .map(|p| {
                centroids
                    .iter()
                    .map(|c| squared_dist(p, c))
                    .fold(f64::INFINITY, f64::min)
            })
            .collect();
        let total: f64 = dists.iter().sum();
        if total <= 0.0 {
            // Remaining points coincide with an existing centroid; fall
            // back to uniform pick rather than divide by zero.
            centroids.push(data[rng.gen_range(0..data.len())].clone());
            continue;
        }
        let mut roll = rng.gen_range(0.0..total);
        let mut chosen = data.len() - 1;
        for (i, d) in dists.iter().enumerate() {
            if roll < *d {
                chosen = i;
                break;
            }
            roll -= d;
        }
        centroids.push(data[chosen].clone());
    }
    centroids
}

/// Lloyd's-algorithm k-means with k-means++ init and multiple restarts
/// (keeping the lowest-inertia run). `data` should already be standardized
/// (see `detectors::standardize`) — k-means is scale-sensitive and the
/// features extracted by `extract_features` span wildly different ranges
/// (a coefficient of variation near 1.0 vs an hour-of-day near 15.0).
pub fn kmeans(data: &[Vec<f64>], k: usize, restarts: usize, rng: &mut impl Rng) -> KMeansResult {
    assert!(!data.is_empty() && k > 0 && k <= data.len());
    let n_features = data[0].len();

    let mut best: Option<(Vec<usize>, f64)> = None;

    for _ in 0..restarts.max(1) {
        let mut centroids = kmeans_plus_plus_init(data, k, rng);
        let mut assignments = vec![0usize; data.len()];

        for _ in 0..100 {
            let mut changed = false;
            for (assignment, point) in assignments.iter_mut().zip(data.iter()) {
                let mut best_c = 0usize;
                let mut best_d = f64::INFINITY;
                for (c, centroid) in centroids.iter().enumerate() {
                    let d = squared_dist(point, centroid);
                    if d < best_d {
                        best_d = d;
                        best_c = c;
                    }
                }
                if *assignment != best_c {
                    changed = true;
                }
                *assignment = best_c;
            }

            let mut sums = vec![vec![0.0; n_features]; k];
            let mut counts = vec![0usize; k];
            for (point, &c) in data.iter().zip(assignments.iter()) {
                counts[c] += 1;
                for (sum_j, &point_j) in sums[c].iter_mut().zip(point.iter()) {
                    *sum_j += point_j;
                }
            }
            for ((centroid, &count), sum) in
                centroids.iter_mut().zip(counts.iter()).zip(sums.iter())
            {
                if count == 0 {
                    // Empty cluster: reseed to a random data point rather
                    // than leaving it permanently dead.
                    let idx = rng.gen_range(0..data.len());
                    *centroid = data[idx].clone();
                } else {
                    let n = count as f64;
                    for (centroid_j, &sum_j) in centroid.iter_mut().zip(sum.iter()) {
                        *centroid_j = sum_j / n;
                    }
                }
            }

            if !changed {
                break;
            }
        }

        let inertia: f64 = data
            .iter()
            .zip(assignments.iter())
            .map(|(p, &c)| squared_dist(p, &centroids[c]))
            .sum();

        if best.as_ref().map(|(_, bi)| inertia < *bi).unwrap_or(true) {
            best = Some((assignments, inertia));
        }
    }

    let (assignments, inertia) = best.unwrap();
    KMeansResult {
        assignments,
        inertia,
    }
}

// ---------- clustering agreement metrics (from scratch) ----------

/// Builds the contingency table between two label vectors: `table[i][j]` =
/// number of points with true label i and predicted label j.
fn contingency_table(true_labels: &[usize], pred_labels: &[usize]) -> Vec<Vec<u64>> {
    let n_true = true_labels.iter().max().map(|&m| m + 1).unwrap_or(0);
    let n_pred = pred_labels.iter().max().map(|&m| m + 1).unwrap_or(0);
    let mut table = vec![vec![0u64; n_pred]; n_true];
    for (&t, &p) in true_labels.iter().zip(pred_labels.iter()) {
        table[t][p] += 1;
    }
    table
}

fn row_sums(table: &[Vec<u64>]) -> Vec<u64> {
    table.iter().map(|row| row.iter().sum()).collect()
}

fn col_sums(table: &[Vec<u64>]) -> Vec<u64> {
    if table.is_empty() {
        return vec![];
    }
    (0..table[0].len())
        .map(|j| table.iter().map(|row| row[j]).sum())
        .collect()
}

fn n_choose_2(n: u64) -> f64 {
    if n < 2 {
        0.0
    } else {
        (n as f64) * ((n - 1) as f64) / 2.0
    }
}

/// Adjusted Rand Index between two partitions of the same n points.
/// 1.0 = identical partitions (up to relabeling), ~0.0 = agreement no
/// better than chance, negative = worse than chance. Standard pair-counting
/// formula (Hubert & Arabie 1985) — the same definition sklearn's
/// `adjusted_rand_score` uses; unit tests below check this implementation
/// against known-answer cases (identical partition, pure relabeling,
/// single-cluster collapse, random partitions).
pub fn adjusted_rand_index(true_labels: &[usize], pred_labels: &[usize]) -> f64 {
    assert_eq!(true_labels.len(), pred_labels.len());
    let n = true_labels.len() as u64;
    let table = contingency_table(true_labels, pred_labels);

    let sum_ij: f64 = table
        .iter()
        .flat_map(|row| row.iter())
        .map(|&nij| n_choose_2(nij))
        .sum();

    let row_sums = row_sums(&table);
    let col_sums = col_sums(&table);

    let sum_a: f64 = row_sums.iter().map(|&a| n_choose_2(a)).sum();
    let sum_b: f64 = col_sums.iter().map(|&b| n_choose_2(b)).sum();

    let total = n_choose_2(n);
    if total == 0.0 {
        return 1.0;
    }
    let expected = sum_a * sum_b / total;
    let max_index = 0.5 * (sum_a + sum_b);

    if (max_index - expected).abs() < 1e-9 {
        // Degenerate denominator (e.g. both partitions are the trivial
        // all-in-one-cluster or all-singletons case). Matches sklearn
        // convention: 1.0 when the index also equals max_index (both
        // partitions are the same trivial shape), else 0.0.
        return if (sum_ij - max_index).abs() < 1e-9 {
            1.0
        } else {
            0.0
        };
    }

    (sum_ij - expected) / (max_index - expected)
}

/// Normalized Mutual Information between two partitions, using the
/// geometric-mean normalization NMI = MI / sqrt(H(U) * H(V)) (natural log
/// throughout). 1.0 = identical partitions (up to relabeling), 0.0 =
/// independent. Documented explicitly because NMI has several
/// normalization conventions (arithmetic mean, geometric mean, max, min)
/// that disagree on the exact value — this implementation always uses
/// geometric mean.
pub fn normalized_mutual_info(true_labels: &[usize], pred_labels: &[usize]) -> f64 {
    assert_eq!(true_labels.len(), pred_labels.len());
    let n = true_labels.len() as f64;
    if n == 0.0 {
        return 1.0;
    }
    let table = contingency_table(true_labels, pred_labels);
    let row_sums = row_sums(&table);
    let col_sums = col_sums(&table);

    let mut mi = 0.0;
    for (i, row) in table.iter().enumerate() {
        for (j, &nij) in row.iter().enumerate() {
            if nij == 0 {
                continue;
            }
            let p_ij = nij as f64 / n;
            let p_i = row_sums[i] as f64 / n;
            let p_j = col_sums[j] as f64 / n;
            mi += p_ij * (p_ij / (p_i * p_j)).ln();
        }
    }

    let entropy = |sums: &[u64]| -> f64 {
        -sums
            .iter()
            .filter(|&&s| s > 0)
            .map(|&s| {
                let p = s as f64 / n;
                p * p.ln()
            })
            .sum::<f64>()
    };

    let h_true = entropy(&row_sums);
    let h_pred = entropy(&col_sums);

    if h_true == 0.0 || h_pred == 0.0 {
        // One (or both) side(s) is a single trivial cluster containing
        // everything. Matches sklearn convention: 1.0 only if BOTH sides
        // are trivial (mutual information is vacuously perfect there),
        // else 0.0 (a trivial side carries no information to normalize by).
        return if h_true == 0.0 && h_pred == 0.0 {
            1.0
        } else {
            0.0
        };
    }

    (mi / (h_true * h_pred).sqrt()).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    #[test]
    fn feature_separability_is_high_for_a_clearly_separating_feature() {
        // Feature 0 cleanly separates the two groups (10.0 vs 0.0, no
        // within-group spread); feature 1 is pure within-group noise
        // uncorrelated with group membership.
        let rows = vec![
            vec![10.0, 0.1],
            vec![10.0, -0.2],
            vec![10.0, 0.3],
            vec![0.0, -0.1],
            vec![0.0, 0.2],
            vec![0.0, -0.3],
        ];
        let labels = vec![0, 0, 0, 1, 1, 1];
        let scores = feature_separability(&rows, &labels);
        assert_eq!(scores[0], f64::INFINITY, "zero within-group spread");
        assert!(
            scores[1] < 5.0,
            "noise feature should not look separable, got {}",
            scores[1]
        );
    }

    #[test]
    fn ari_is_one_for_identical_partitions() {
        let labels = vec![0, 0, 0, 1, 1, 1, 2, 2, 2];
        assert!((adjusted_rand_index(&labels, &labels) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ari_is_one_for_a_relabeling_of_the_same_partition() {
        let true_labels = vec![0, 0, 0, 1, 1, 1, 2, 2, 2];
        // Same groupings, different label numbers/order.
        let pred_labels = vec![2, 2, 2, 0, 0, 0, 1, 1, 1];
        assert!((adjusted_rand_index(&true_labels, &pred_labels) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ari_is_zero_for_a_single_cluster_prediction() {
        // Predicting everything as one giant cluster vs a real 3-way split
        // is a common degenerate failure mode — must not score as "good."
        let true_labels = vec![0, 0, 0, 1, 1, 1, 2, 2, 2];
        let pred_labels = vec![0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(adjusted_rand_index(&true_labels, &pred_labels).abs() < 1e-9);
    }

    #[test]
    fn ari_is_near_chance_for_random_partitions_on_average() {
        let mut rng = ChaCha8Rng::seed_from_u64(99);
        let true_labels: Vec<usize> = (0..300).map(|i| i / 30).collect(); // 10 balanced clusters of 30
        let mut total = 0.0;
        let trials = 50;
        for _ in 0..trials {
            let pred_labels: Vec<usize> = (0..300).map(|_| rng.gen_range(0..10)).collect();
            total += adjusted_rand_index(&true_labels, &pred_labels);
        }
        let mean_ari = total / trials as f64;
        assert!(
            mean_ari.abs() < 0.05,
            "expected near-zero mean ARI for random partitions, got {mean_ari}"
        );
    }

    #[test]
    fn nmi_is_one_for_identical_partitions() {
        let labels = vec![0, 0, 1, 1, 2, 2];
        assert!((normalized_mutual_info(&labels, &labels) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn nmi_is_zero_when_prediction_is_a_single_trivial_cluster() {
        let true_labels = vec![0, 0, 0, 1, 1, 1, 2, 2, 2];
        let pred_labels = vec![0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(normalized_mutual_info(&true_labels, &pred_labels).abs() < 1e-9);
    }

    #[test]
    fn nmi_is_low_for_random_partitions_on_average() {
        let mut rng = ChaCha8Rng::seed_from_u64(7);
        let true_labels: Vec<usize> = (0..300).map(|i| i / 30).collect();
        let mut total = 0.0;
        let trials = 50;
        for _ in 0..trials {
            let pred_labels: Vec<usize> = (0..300).map(|_| rng.gen_range(0..10)).collect();
            total += normalized_mutual_info(&true_labels, &pred_labels);
        }
        let mean_nmi = total / trials as f64;
        assert!(
            mean_nmi < 0.15,
            "expected low mean NMI for random partitions, got {mean_nmi}"
        );
    }

    #[test]
    fn kmeans_recovers_well_separated_clusters() {
        let mut rng = ChaCha8Rng::seed_from_u64(1);
        // Three tight, well-separated 2D blobs.
        let mut data = Vec::new();
        let mut true_labels = Vec::new();
        for (cluster, center) in [(0usize, 0.0), (1, 50.0), (2, 100.0)] {
            for _ in 0..20 {
                let jitter = rng.gen_range(-1.0..1.0);
                data.push(vec![center + jitter, center + jitter]);
                true_labels.push(cluster);
            }
        }
        let result = kmeans(&data, 3, 5, &mut rng);
        let ari = adjusted_rand_index(&true_labels, &result.assignments);
        assert!(ari > 0.95, "expected near-perfect recovery, got ARI={ari}");
    }

    #[test]
    fn simulate_wallet_reaches_action_target_and_respects_active_hours() {
        let mut rng = ChaCha8Rng::seed_from_u64(3);
        let operator = OperatorConfig {
            timing: TimingConfig {
                mean_interval_minutes: 45.0,
                stddev_interval_minutes: 30.0,
                active_hours: [8, 23],
                skip_day_probability: 0.15,
            },
            protocols: vec![ProtocolConfig {
                name: "jupiter_swap".to_string(),
                weight: 1.0,
                params: toml::Table::new(),
            }],
        };
        let registry = ProtocolRegistry::from_config(&operator.protocols).unwrap();
        let actions = simulate_wallet(&operator, &registry, 20, &mut rng);
        assert_eq!(actions.len(), 20);
        for a in &actions {
            let hour = (a.timestamp_secs % SECS_PER_DAY) / SECS_PER_HOUR;
            assert!(
                (8..23).contains(&hour),
                "action fired outside active hours: {hour}"
            );
        }
    }

    #[test]
    fn simulate_wallet_with_window_respects_a_non_hour_aligned_jittered_window() {
        // 8:20am - 10:00am (500..600 minutes-of-day) — deliberately NOT
        // hour-aligned, exercising the minute-granularity path a jittered
        // per-agent window actually produces (unlike simulate_wallet's
        // thin wrapper, which always passes exact hour multiples).
        let mut rng = ChaCha8Rng::seed_from_u64(11);
        let operator = OperatorConfig {
            timing: TimingConfig {
                mean_interval_minutes: 45.0,
                stddev_interval_minutes: 30.0,
                active_hours: [8, 23], // ignored: window is passed explicitly below
                skip_day_probability: 0.15,
            },
            protocols: vec![ProtocolConfig {
                name: "jupiter_swap".to_string(),
                weight: 1.0,
                params: toml::Table::new(),
            }],
        };
        let registry = ProtocolRegistry::from_config(&operator.protocols).unwrap();
        let skip_day_probability = operator.timing.skip_day_probability;
        let actions = simulate_wallet_with_window(
            &operator,
            &registry,
            20,
            (500, 600),
            skip_day_probability,
            &mut rng,
        );
        assert_eq!(actions.len(), 20);
        for a in &actions {
            let minute_of_day = (a.timestamp_secs % SECS_PER_DAY) / SECS_PER_MINUTE;
            assert!(
                (500..600).contains(&minute_of_day),
                "action fired outside jittered window: minute {minute_of_day}"
            );
        }
    }

    /// Regression test proving the explicit `skip_day_probability` PARAMETER
    /// drives the skip-day roll, not `operator.timing.skip_day_probability`
    /// (which stays at a moderate 0.15 here). `1.0` forces every simulated
    /// day to be skipped, so the wallet never reaches `actions_target` and
    /// the function's own pathological-config guard (see its doc comment)
    /// terminates the loop with zero actions — a result the operator's own
    /// 0.15 field could never produce on its own within this guard limit.
    #[test]
    fn simulate_wallet_with_window_uses_the_explicit_skip_day_probability_override() {
        let mut rng = ChaCha8Rng::seed_from_u64(17);
        let operator = OperatorConfig {
            timing: TimingConfig {
                mean_interval_minutes: 45.0,
                stddev_interval_minutes: 30.0,
                active_hours: [8, 23],
                skip_day_probability: 0.15,
            },
            protocols: vec![ProtocolConfig {
                name: "jupiter_swap".to_string(),
                weight: 1.0,
                params: toml::Table::new(),
            }],
        };
        let registry = ProtocolRegistry::from_config(&operator.protocols).unwrap();
        let actions =
            simulate_wallet_with_window(&operator, &registry, 5, (8 * 60, 23 * 60), 1.0, &mut rng);
        assert!(
            actions.is_empty(),
            "skip_day_probability=1.0 override should never act, got {} actions",
            actions.len()
        );
    }

    #[test]
    fn extract_features_has_expected_dimensionality() {
        let mut rng = ChaCha8Rng::seed_from_u64(5);
        let operator = OperatorConfig {
            timing: TimingConfig {
                mean_interval_minutes: 45.0,
                stddev_interval_minutes: 30.0,
                active_hours: [8, 23],
                skip_day_probability: 0.15,
            },
            protocols: vec![ProtocolConfig {
                name: "jupiter_swap".to_string(),
                weight: 1.0,
                params: toml::Table::new(),
            }],
        };
        let registry = ProtocolRegistry::from_config(&operator.protocols).unwrap();
        let actions = simulate_wallet(&operator, &registry, 15, &mut rng);
        let features = extract_features(&actions, &["jupiter_swap", "marinade_stake", "orca_lp"]);
        assert_eq!(features.len(), 8);
        // Only jupiter_swap is configured, so its fraction must be 1.0 and
        // the other two protocols' fractions must be 0.0.
        assert!((features[5] - 1.0).abs() < 1e-9);
        assert_eq!(features[6], 0.0);
        assert_eq!(features[7], 0.0);
    }
}

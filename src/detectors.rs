//! A stronger, named adversary baseline for `timing_harness`: logistic
//! regression over multiple timing features, instead of a single
//! threshold on coefficient of variation.
//!
//! This is deliberately small (3 features, plain gradient descent, no
//! external ML crate) — it is not a claim of matching a real adversary's
//! sophistication, only a second, honestly weaker point of comparison to
//! the single-feature CV heuristic already in `timing_harness`. See
//! `THREAT_MODEL.md` for how this result is scoped.

/// (standardized train rows, standardized test rows, column means, column stds)
type StandardizeOutput = (Vec<Vec<f64>>, Vec<Vec<f64>>, Vec<f64>, Vec<f64>);

/// Standardizes each feature column to zero mean / unit variance using the
/// TRAIN set's statistics, then applies the same transform to a second set.
/// Returns (train_std, test_std, means, stds) so the caller can reuse them.
pub fn standardize(train: &[Vec<f64>], test: &[Vec<f64>]) -> StandardizeOutput {
    let n_features = train[0].len();
    let n = train.len() as f64;
    let mut means = vec![0.0; n_features];
    let mut stds = vec![1.0; n_features];

    for j in 0..n_features {
        let mean: f64 = train.iter().map(|row| row[j]).sum::<f64>() / n;
        let var: f64 = train.iter().map(|row| (row[j] - mean).powi(2)).sum::<f64>() / n;
        let std = var.sqrt();
        means[j] = mean;
        stds[j] = if std > 1e-9 { std } else { 1.0 };
    }

    let apply = |rows: &[Vec<f64>]| -> Vec<Vec<f64>> {
        rows.iter()
            .map(|row| {
                row.iter()
                    .enumerate()
                    .map(|(j, x)| (x - means[j]) / stds[j])
                    .collect()
            })
            .collect()
    };

    (apply(train), apply(test), means, stds)
}

fn sigmoid(z: f64) -> f64 {
    1.0 / (1.0 + (-z).exp())
}

pub struct LogisticRegression {
    pub weights: Vec<f64>,
    pub bias: f64,
}

impl LogisticRegression {
    /// Trains via plain batch gradient descent on standardized features.
    /// `labels` are 1.0 (bot / positive class) or 0.0 (agent / negative class).
    pub fn train(features: &[Vec<f64>], labels: &[f64], lr: f64, epochs: usize) -> Self {
        let n = features.len() as f64;
        let n_features = features[0].len();
        let mut weights = vec![0.0; n_features];
        let mut bias = 0.0;

        for _ in 0..epochs {
            let mut grad_w = vec![0.0; n_features];
            let mut grad_b = 0.0;

            for (row, &label) in features.iter().zip(labels.iter()) {
                let z: f64 = row
                    .iter()
                    .zip(weights.iter())
                    .map(|(x, w)| x * w)
                    .sum::<f64>()
                    + bias;
                let pred = sigmoid(z);
                let err = pred - label;
                for j in 0..n_features {
                    grad_w[j] += err * row[j];
                }
                grad_b += err;
            }

            for j in 0..n_features {
                weights[j] -= lr * grad_w[j] / n;
            }
            bias -= lr * grad_b / n;
        }

        Self { weights, bias }
    }

    pub fn predict_proba(&self, row: &[f64]) -> f64 {
        let z: f64 = row
            .iter()
            .zip(self.weights.iter())
            .map(|(x, w)| x * w)
            .sum::<f64>()
            + self.bias;
        sigmoid(z)
    }
}

/// ROC AUC via the rank-sum (Mann-Whitney U) formula — exact, not
/// threshold-swept, so it isn't sensitive to a bucket size choice.
/// `scores`/`labels` must be the same length; label 1.0 = positive class.
pub fn roc_auc(labels: &[f64], scores: &[f64]) -> f64 {
    let mut pairs: Vec<(f64, f64)> = scores.iter().copied().zip(labels.iter().copied()).collect();
    pairs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

    let n = pairs.len();
    let mut ranks = vec![0.0; n];
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j + 1 < n && pairs[j + 1].0 == pairs[i].0 {
            j += 1;
        }
        // Average rank for ties (1-indexed).
        let avg_rank = ((i + 1) + (j + 1)) as f64 / 2.0;
        for r in ranks.iter_mut().take(j + 1).skip(i) {
            *r = avg_rank;
        }
        i = j + 1;
    }

    let n_pos = pairs.iter().filter(|(_, l)| *l == 1.0).count() as f64;
    let n_neg = n as f64 - n_pos;
    if n_pos == 0.0 || n_neg == 0.0 {
        return 0.5;
    }

    let rank_sum_pos: f64 = pairs
        .iter()
        .zip(ranks.iter())
        .filter(|((_, l), _)| *l == 1.0)
        .map(|(_, r)| r)
        .sum();

    (rank_sum_pos - n_pos * (n_pos + 1.0) / 2.0) / (n_pos * n_neg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roc_auc_is_one_for_perfectly_separated_scores() {
        let labels = vec![0.0, 0.0, 1.0, 1.0];
        let scores = vec![0.1, 0.2, 0.8, 0.9];
        assert!((roc_auc(&labels, &scores) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn roc_auc_is_half_for_random_scores_matching_labels() {
        let labels = vec![0.0, 1.0, 0.0, 1.0];
        let scores = vec![0.5, 0.5, 0.5, 0.5];
        assert!((roc_auc(&labels, &scores) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn logistic_regression_separates_linearly_separable_data() {
        let features = vec![
            vec![0.0, 0.0],
            vec![0.1, -0.1],
            vec![5.0, 5.0],
            vec![5.1, 4.9],
        ];
        let labels = vec![0.0, 0.0, 1.0, 1.0];
        let model = LogisticRegression::train(&features, &labels, 0.5, 2000);
        assert!(model.predict_proba(&[0.0, 0.0]) < 0.5);
        assert!(model.predict_proba(&[5.0, 5.0]) > 0.5);
    }
}

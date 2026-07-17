//! Naive-bot separability harness — measures, not asserts.
//!
//! This does NOT claim indistinguishability from a validated real-human
//! dataset (we don't have one). What it measures is narrower and honest:
//! how well the single most common real-world chain-analysis heuristic —
//! "flag suspiciously regular timing" — separates a naive fixed-cadence bot
//! from account-cooker's log-normal agent, using the exact same
//! `sample_interval_secs` function the real agent scheduler calls.
//!
//! Reproduce: cargo run --release --bin timing_harness -- --n 5000 --seed 1

use account_cooker::timing::{coefficient_of_variation, sample_interval_secs};
use clap::Parser;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

#[derive(Parser)]
#[command(about = "Measures how detectable account-cooker's timing is vs a naive fixed-cadence bot")]
struct Args {
    /// Simulated wallets per class (naive bot vs our agent).
    #[arg(long, default_value_t = 5000)]
    n: usize,
    /// Consecutive intervals the detector observes per wallet before deciding.
    #[arg(long, default_value_t = 8)]
    window: usize,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    #[arg(long, default_value_t = 45.0)]
    mean_minutes: f64,
    #[arg(long, default_value_t = 30.0)]
    std_minutes: f64,
    /// Relative jitter (+/- fraction of mean) given to the naive bot — models
    /// a bot author who added *some* randomness but not enough.
    #[arg(long, default_value_t = 0.05)]
    bot_jitter_frac: f64,
    /// CV below this is flagged "suspiciously regular" by the detector.
    /// 0.15 is a conservative, documented heuristic — not tuned to this data.
    #[arg(long, default_value_t = 0.15)]
    threshold: f64,
}

fn naive_bot_window(rng: &mut impl Rng, window: usize, mean_minutes: f64, jitter_frac: f64) -> Vec<f64> {
    let mean_secs = mean_minutes * 60.0;
    (0..window)
        .map(|_| {
            let jitter = rng.gen_range(-jitter_frac..=jitter_frac);
            mean_secs * (1.0 + jitter)
        })
        .collect()
}

fn agent_window(rng: &mut impl Rng, window: usize, mean_minutes: f64, std_minutes: f64) -> Vec<f64> {
    (0..window)
        .map(|_| sample_interval_secs(mean_minutes, std_minutes, rng) as f64)
        .collect()
}

fn main() {
    let args = Args::parse();
    let mut rng = ChaCha8Rng::seed_from_u64(args.seed);

    let mut bot_flagged = 0usize;
    let mut agent_flagged = 0usize;

    for _ in 0..args.n {
        let bot_cv = coefficient_of_variation(&naive_bot_window(
            &mut rng,
            args.window,
            args.mean_minutes,
            args.bot_jitter_frac,
        ));
        if bot_cv < args.threshold {
            bot_flagged += 1;
        }

        let agent_cv = coefficient_of_variation(&agent_window(
            &mut rng,
            args.window,
            args.mean_minutes,
            args.std_minutes,
        ));
        if agent_cv < args.threshold {
            agent_flagged += 1;
        }
    }

    let bot_detection_rate = bot_flagged as f64 / args.n as f64;
    let agent_false_flag_rate = agent_flagged as f64 / args.n as f64;

    println!("timing_harness — naive fixed-cadence detector (CV < {:.2})", args.threshold);
    println!(
        "config: mean={:.1}min std={:.1}min window={} n_per_class={} seed={} bot_jitter=±{:.0}%",
        args.mean_minutes,
        args.std_minutes,
        args.window,
        args.n,
        args.seed,
        args.bot_jitter_frac * 100.0
    );
    println!();
    println!("| Class                                    | Flagged as \"fixed-cadence bot\" |");
    println!("|-------------------------------------------|--------------------------------|");
    println!(
        "| naive bot (±{:.0}% jitter)                    | {:>6.2}%                        |",
        args.bot_jitter_frac * 100.0,
        bot_detection_rate * 100.0
    );
    println!(
        "| account-cooker agent (this config)         | {:>6.2}%                        |",
        agent_false_flag_rate * 100.0
    );
    println!();
    println!(
        "Scope: this measures resistance to ONE specific, common heuristic \
        (fixed-cadence detection via coefficient of variation) — it is a \
        modeled comparison against a naive bot, not a claim of \
        indistinguishability from a validated real-human dataset, which \
        this harness does not have access to."
    );

    if agent_false_flag_rate > 0.10 {
        eprintln!();
        eprintln!(
            "WARNING: agent false-flag rate is {:.1}% — consider raising \
            std_minutes relative to mean_minutes in cooker.toml for this persona.",
            agent_false_flag_rate * 100.0
        );
    }
}

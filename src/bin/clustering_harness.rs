//! Multi-wallet behavioral clustering harness — measures, not asserts.
//!
//! `timing_harness` answers "does ONE wallet look like a bot?" (binary).
//! This answers the bounty brief's actual central question: "explorers,
//! analytics platforms, and copy-traders can't determine which wallets
//! belong to the same entity" — a CLUSTERING question over many wallets at
//! once, not a single-wallet detection question. It uses k-means over
//! behavioral features (timing shape, active-hours signature, protocol
//! mix) extracted by `src/clustering.rs`, which itself reuses the real
//! `timing::sample_interval_secs` and `protocols::ProtocolRegistry::pick_with_rng`
//! — not a reimplementation, so this measures what actually ships.
//!
//! ## Two independent axes, five named scenarios — plus a sixth, the fix
//!
//! An operator persona has two things that can vary: how WIDE its
//! intra-operator timing spread is (tight/naive vs account-cooker's real
//! wide log-normal), and whether active-hours + protocol weights are
//! SHARED (identical) across operators or DIVERSE (independently
//! randomized per operator, as `cooker.toml` actually allows). Crossing
//! those two axes gives four scenarios, plus one fully-degenerate control:
//!
//! - **`identical_control`** — every operator uses byte-identical config
//!   in every field. No learnable operator signal exists BY CONSTRUCTION.
//!   This is a sanity check, not a privacy claim: if this scenario did NOT
//!   score near-zero ARI/NMI, it would mean this harness is unsound (e.g.
//!   picking up on RNG-stream artifacts rather than real behavioral
//!   signal), not that clustering is somehow "hard." It exists so the
//!   other numbers can be trusted.
//! - **`tight_timing + shared_persona`** — operators differ ONLY in a
//!   per-operator mean interval (operators must differ in *something* or
//!   there is nothing to cluster on at all — that fully signal-free setup
//!   is exactly `identical_control` above), with TIGHT log-normal timing
//!   (std = 5% of mean) via the same `sample_interval_secs` function every
//!   scenario uses. Active hours and protocol weights are identical across
//!   operators. This is the "naive bot farm" baseline: many wallets on
//!   close to the same schedule, the failure mode account-cooker's timing
//!   design exists to avoid.
//! - **`tight_timing + diverse_persona`** — same tight per-operator
//!   cadence as above, but active_hours and protocol weights are now
//!   independently randomized per operator too.
//! - **`wide_timing + shared_persona`** — account-cooker's real wide
//!   log-normal timing spread (std = 50-90% of mean, matching
//!   THREAT_MODEL.md's named defense), but active_hours and protocol
//!   weights held identical across operators.
//! - **`wide_timing + diverse_persona` (PRE-fix)** — full diversity on
//!   every axis AT THE OPERATOR LEVEL: wide log-normal timing,
//!   independently randomized active_hours, independently randomized
//!   protocol weights, independent per-agent RNG — but every agent within
//!   one operator still shares that operator's exact active_hours/protocol
//!   weights. This was the config family account-cooker shipped in
//!   `cooker.example.toml` before persona jitter (`src/persona.rs`).
//! - **`wide_timing + diverse_persona + agent_jitter` (POST-fix)** — same
//!   operator-level generation as the row above (same `build_operator`
//!   call, same RNG draws — see `build_operators`), but each AGENT within
//!   an operator now derives its own active_hours/protocol weights via
//!   `persona::jittered_active_hours_minutes` /
//!   `persona::jittered_protocol_weights` at `PersonaJitterConfig::default()`'s
//!   magnitude, the same defaults `cooker.example.toml` ships. Because
//!   operator generation is held identical between this row and the one
//!   above, any ARI/NMI difference between them isolates the effect of
//!   per-agent persona jitter alone.
//!
//! Splitting the axes this way exists because the first version of this
//! harness compared only `tight_timing+shared_persona` (naive) against
//! `wide_timing+diverse_persona` (real) and got a result that does NOT
//! favor account-cooker: the "real" config clustered *more* accurately
//! than the "naive" one. Collapsing two axes into one comparison couldn't
//! say why. The scenario breakdown (plus the per-feature separability
//! diagnostic printed below the main table) can — see the numbers in
//! README.md / THREAT_MODEL.md for what it actually shows and why this
//! result is reported as-is rather than reshaped until it looked better.
//!
//! ## Honest scope (see THREAT_MODEL.md for the full statement)
//!
//! This measures resistance to clustering on OBSERVABLE BEHAVIORAL
//! FEATURES ONLY — timing shape, active-hours signature, protocol mix —
//! the same class of signal `timing_harness` already measures for a single
//! wallet, now aggregated across many. It does NOT model an adversary with
//! on-chain metadata (funding graph, common-funder heuristics, address
//! reuse, gas-source correlation) — that class of attack is explicitly out
//! of scope for account-cooker per THREAT_MODEL.md ("Off-chain metadata
//! correlation", "destination-history channel") and this harness does not
//! change that scope. It reports ARI/NMI (partition-agreement metrics), as
//! specified for this measurement — it does not report ROC AUC / F1 /
//! Precision@K, which need a different task formulation (ranked pairwise
//! similarity) than partition clustering; `timing_harness`'s own
//! logistic-regression baseline already covers the single-wallet binary
//! case those would otherwise describe. This harness is ADDITIONAL to
//! `timing_harness`, not a replacement for it.
//!
//! Reproduce: cargo run --release --bin clustering_harness -- --seed 1

use account_cooker::clustering::{
    adjusted_rand_index, extract_features, feature_separability, kmeans, normalized_mutual_info,
    simulate_wallet_with_window, OperatorConfig,
};
use account_cooker::config::{PersonaJitterConfig, ProtocolConfig, TimingConfig};
use account_cooker::detectors::standardize;
use account_cooker::persona;
use account_cooker::protocols::ProtocolRegistry;
use clap::Parser;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;

const PROTOCOL_NAMES: [&str; 3] = ["jupiter_swap", "marinade_stake", "orca_lp"];
const FEATURE_NAMES: [&str; 8] = [
    "coefficient_of_variation",
    "autocorrelation_lag1",
    "skewness",
    "mean_hour_of_day",
    "actions_per_day",
    "frac_jupiter_swap",
    "frac_marinade_stake",
    "frac_orca_lp",
];

#[derive(Parser)]
#[command(
    about = "Measures whether many account-cooker wallets can be clustered back to their operator by behavior alone"
)]
struct Args {
    /// Simulated operators (distinct wallet controllers) per trial.
    #[arg(long, default_value_t = 6)]
    operators: usize,
    /// Wallets per operator.
    #[arg(long, default_value_t = 8)]
    agents_per_operator: usize,
    /// Actions observed per wallet before the adversary decides — a
    /// plausible bounded observation window, not an asymptotic sample.
    #[arg(long, default_value_t = 20)]
    actions_per_wallet: usize,
    /// Independent reseeded trials per scenario, reported as mean +/- std
    /// instead of a single-seed result.
    #[arg(long, default_value_t = 50)]
    trials: usize,
    /// k-means random restarts per trial (lowest-inertia run kept).
    #[arg(long, default_value_t = 10)]
    kmeans_restarts: usize,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Persona-jitter magnitude used by the POST-fix scenario only —
    /// defaults to exactly `PersonaJitterConfig::default()`, the same
    /// value `cooker.example.toml` ships, so the default invocation
    /// measures the real shipped fix. Overridable here so the
    /// magnitude-vs-believability tradeoff can be explored without editing
    /// code (see README.md's "3c." for what a larger value costs).
    #[arg(long, default_value_t = PersonaJitterConfig::default().active_hours_minutes)]
    jitter_active_hours_minutes: f64,
    #[arg(long, default_value_t = PersonaJitterConfig::default().protocol_weight_fraction)]
    jitter_protocol_weight_fraction: f64,
}

#[derive(Clone, Copy)]
enum TimingSpread {
    /// std = 5% of mean — a naive "added some randomness but not enough"
    /// cadence, the same failure mode timing_harness's naive-bot baseline
    /// names, expressed as a tight log-normal instead of uniform jitter so
    /// every scenario here shares one generative family.
    Tight,
    /// std = 50-90% of mean — account-cooker's actual shipped spread (the
    /// example config uses a 30/45 = 67% ratio).
    Wide,
}

#[derive(Clone, Copy)]
enum Persona {
    /// Every operator uses identical active_hours and protocol weights —
    /// only the timing parameters (mean, and std under `Wide`) vary.
    Shared,
    /// active_hours and protocol weights are independently randomized per
    /// operator, as `cooker.toml` actually allows.
    Diverse,
}

#[derive(Clone, Copy)]
enum Scenario {
    IdenticalControl,
    Combo(TimingSpread, Persona),
    /// `wide_timing + diverse_persona` (account-cooker's real operator-level
    /// config) PLUS per-agent persona jitter at `PersonaJitterConfig`'s
    /// default magnitude — the fix `THREAT_MODEL.md`'s "Multi-wallet
    /// clustering" section named but did not implement. Operator
    /// generation is byte-identical to `Combo(Wide, Diverse)` (same
    /// `build_operator` call, same RNG draws, see `build_operators`) — the
    /// ONLY difference is that each agent's active_hours/protocol weights
    /// are no longer the operator's exact values, so this scenario isolates
    /// the fix's effect rather than conflating it with a change in
    /// operator-level diversity.
    RealConfigPostFix,
}

impl Scenario {
    fn label(&self) -> &'static str {
        match self {
            Scenario::IdenticalControl => "identical_control (sanity check)",
            Scenario::Combo(TimingSpread::Tight, Persona::Shared) => {
                "tight_timing + shared_persona (naive bot-farm)"
            }
            Scenario::Combo(TimingSpread::Tight, Persona::Diverse) => {
                "tight_timing + diverse_persona"
            }
            Scenario::Combo(TimingSpread::Wide, Persona::Shared) => "wide_timing + shared_persona",
            Scenario::Combo(TimingSpread::Wide, Persona::Diverse) => {
                "wide_timing + diverse_persona (account-cooker's real config, PRE-fix)"
            }
            Scenario::RealConfigPostFix => {
                "wide_timing + diverse_persona + agent_jitter (POST-fix, default jitter)"
            }
        }
    }

    /// `Some(jitter config)` for the post-fix scenario, taken from `args`
    /// (CLI-overridable, defaults to `PersonaJitterConfig::default()` —
    /// see `Args`); `None` for every other scenario, meaning every agent
    /// within one operator shares that operator's exact persona — the
    /// pre-fix / naive-baseline behavior those scenarios exist to model.
    fn agent_jitter(&self, args: &Args) -> Option<PersonaJitterConfig> {
        match self {
            Scenario::RealConfigPostFix => Some(PersonaJitterConfig {
                active_hours_minutes: args.jitter_active_hours_minutes,
                protocol_weight_fraction: args.jitter_protocol_weight_fraction,
            }),
            _ => None,
        }
    }
}

fn default_protocol_mix(w_swap: f64, w_stake: f64, w_lp: f64) -> Vec<ProtocolConfig> {
    vec![
        ProtocolConfig {
            name: "jupiter_swap".to_string(),
            weight: w_swap,
            params: toml::Table::new(),
        },
        ProtocolConfig {
            name: "marinade_stake".to_string(),
            weight: w_stake,
            params: toml::Table::new(),
        },
        ProtocolConfig {
            name: "orca_lp".to_string(),
            weight: w_lp,
            params: toml::Table::new(),
        },
    ]
}

/// Builds one operator persona for the given (timing spread, persona
/// diversity) combination.
fn build_operator(spread: TimingSpread, persona: Persona, rng: &mut impl Rng) -> OperatorConfig {
    let mean = rng.gen_range(15.0..90.0);
    let stddev_interval_minutes = match spread {
        TimingSpread::Tight => mean * 0.05,
        TimingSpread::Wide => mean * rng.gen_range(0.5..0.9),
    };

    let (active_hours, skip_day_probability, protocols) = match persona {
        Persona::Shared => ([8u8, 23u8], 0.15, default_protocol_mix(1.0, 1.0, 1.0)),
        Persona::Diverse => {
            let start = rng.gen_range(5u8..11);
            let span = rng.gen_range(8u8..15);
            let end = (start + span).min(23);
            let skip_p = rng.gen_range(0.05..0.30);
            let w_swap = rng.gen_range(0.2..3.0);
            let w_stake = rng.gen_range(0.2..3.0);
            let w_lp = rng.gen_range(0.2..3.0);
            (
                [start, end],
                skip_p,
                default_protocol_mix(w_swap, w_stake, w_lp),
            )
        }
    };

    OperatorConfig {
        timing: TimingConfig {
            mean_interval_minutes: mean,
            stddev_interval_minutes,
            active_hours,
            skip_day_probability,
        },
        protocols,
    }
}

/// Builds `n_operators` operator personas for the given scenario. Each
/// operator's own K agents will later share this exact config, mirroring
/// "agents from one operator share the operator's cooker.toml."
fn build_operators(
    scenario: Scenario,
    n_operators: usize,
    rng: &mut impl Rng,
) -> Vec<OperatorConfig> {
    (0..n_operators)
        .map(|_| match scenario {
            Scenario::IdenticalControl => OperatorConfig {
                timing: TimingConfig {
                    mean_interval_minutes: 45.0,
                    stddev_interval_minutes: 30.0,
                    active_hours: [8, 23],
                    skip_day_probability: 0.15,
                },
                protocols: default_protocol_mix(1.0, 1.0, 1.0),
            },
            Scenario::Combo(spread, persona) => build_operator(spread, persona, rng),
            // Same operator-persona generation as Combo(Wide, Diverse) —
            // deliberately, so the two scenarios are only ever compared on
            // the one axis that differs between them (per-agent jitter).
            Scenario::RealConfigPostFix => {
                build_operator(TimingSpread::Wide, Persona::Diverse, rng)
            }
        })
        .collect()
}

struct TrialData {
    true_labels: Vec<usize>,
    feature_rows: Vec<Vec<f64>>,
}

/// Simulates N operators x K agents and extracts each wallet's feature
/// vector. Each agent gets its OWN independent RNG stream seeded off the
/// shared trial RNG — mirroring THREAT_MODEL.md's "no shared entropy
/// across agents" defense — siblings only ever share the operator CONFIG
/// (which is the thing under test), never a random stream.
///
/// Exactly ONE `rng.gen()` draw is made per agent regardless of scenario
/// (`agent_seed` below) — whether that seed is used ONLY to seed the
/// wallet's own action-generation RNG (every scenario except the post-fix
/// one) or ALSO fed through `persona::jittered_*` (post-fix scenario only)
/// doesn't change how many values are drawn from the shared trial `rng`,
/// since `persona::` re-hashes the seed through SHA-256 and spins up its
/// own fresh `ChaCha8Rng` rather than drawing further from `rng`. This is
/// what keeps the pre-existing 5 scenarios' numbers bit-for-bit unchanged
/// by this function's extension to support a 6th.
fn simulate_trial(scenario: Scenario, args: &Args, seed: u64) -> TrialData {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let operators = build_operators(scenario, args.operators, &mut rng);
    let jitter = scenario.agent_jitter(args);

    let mut true_labels = Vec::new();
    let mut feature_rows = Vec::new();

    for (op_idx, operator) in operators.iter().enumerate() {
        let base_registry = ProtocolRegistry::from_config(&operator.protocols)
            .expect("harness-constructed protocol config is always valid");
        let base_window = (
            operator.timing.active_hours[0] as u32 * 60,
            operator.timing.active_hours[1] as u32 * 60,
        );

        for _ in 0..args.agents_per_operator {
            let agent_seed: u64 = rng.gen();
            let mut agent_rng = ChaCha8Rng::seed_from_u64(agent_seed);

            let actions = match &jitter {
                None => simulate_wallet_with_window(
                    operator,
                    &base_registry,
                    args.actions_per_wallet,
                    base_window,
                    &mut agent_rng,
                ),
                Some(j) => {
                    // Synthetic harness wallets have no real Solana keypair
                    // to derive an identity from — the per-agent seed
                    // already drawn above (unique per agent, deterministic
                    // given the trial seed) stands in for one, playing the
                    // same role `wallet.pubkey().to_bytes()` plays in the
                    // real `Agent::from_config`.
                    let identity_bytes = agent_seed.to_le_bytes();

                    let jittered_weights = persona::jittered_protocol_weights(
                        &operator.protocols,
                        j.protocol_weight_fraction,
                        &identity_bytes,
                    );
                    let agent_protocols: Vec<ProtocolConfig> = operator
                        .protocols
                        .iter()
                        .zip(jittered_weights)
                        .map(|(p, weight)| ProtocolConfig {
                            name: p.name.clone(),
                            weight,
                            params: p.params.clone(),
                        })
                        .collect();
                    let agent_registry = ProtocolRegistry::from_config(&agent_protocols)
                        .expect("jittered protocol config is always valid");

                    let agent_window = persona::jittered_active_hours_minutes(
                        operator.timing.active_hours,
                        j.active_hours_minutes,
                        &identity_bytes,
                    );

                    simulate_wallet_with_window(
                        operator,
                        &agent_registry,
                        args.actions_per_wallet,
                        agent_window,
                        &mut agent_rng,
                    )
                }
            };

            feature_rows.push(extract_features(&actions, &PROTOCOL_NAMES));
            true_labels.push(op_idx);
        }
    }

    TrialData {
        true_labels,
        feature_rows,
    }
}

struct TrialResult {
    ari: f64,
    nmi: f64,
}

fn run_trial(scenario: Scenario, args: &Args, seed: u64) -> TrialResult {
    let data = simulate_trial(scenario, args, seed);

    // Standardize (z-score) before k-means — required since raw features
    // span very different scales (CV ~O(0.1-1) vs mean-hour ~O(10)).
    // Reuses the exact `detectors::standardize` the logistic-regression
    // baseline uses; passing the same set as both "train" and "test" just
    // applies its own z-score transform to itself.
    let (standardized, _, _, _) = standardize(&data.feature_rows, &data.feature_rows);

    let mut kmeans_rng = ChaCha8Rng::seed_from_u64(seed ^ 0x9E37_79B9_7F4A_7C15);
    let result = kmeans(
        &standardized,
        args.operators,
        args.kmeans_restarts,
        &mut kmeans_rng,
    );

    TrialResult {
        ari: adjusted_rand_index(&data.true_labels, &result.assignments),
        nmi: normalized_mutual_info(&data.true_labels, &result.assignments),
    }
}

fn mean_std(values: &[f64]) -> (f64, f64) {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    (mean, var.sqrt())
}

fn min_max(values: &[f64]) -> (f64, f64) {
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (min, max)
}

fn main() {
    let args = Args::parse();

    println!("clustering_harness — multi-wallet behavioral clustering (k-means, k=operators)");
    println!(
        "config: operators={} agents_per_operator={} actions_per_wallet={} trials={} kmeans_restarts={} seed={}",
        args.operators,
        args.agents_per_operator,
        args.actions_per_wallet,
        args.trials,
        args.kmeans_restarts,
        args.seed
    );
    println!(
        "wallets per trial: {} (= operators x agents_per_operator)",
        args.operators * args.agents_per_operator
    );
    println!();

    let scenarios = [
        Scenario::IdenticalControl,
        Scenario::Combo(TimingSpread::Tight, Persona::Shared),
        Scenario::Combo(TimingSpread::Tight, Persona::Diverse),
        Scenario::Combo(TimingSpread::Wide, Persona::Shared),
        Scenario::Combo(TimingSpread::Wide, Persona::Diverse),
        Scenario::RealConfigPostFix,
    ];

    println!("| Scenario | ARI (mean ± std) | NMI (mean ± std) | ARI range | NMI range |");
    println!("|---|---|---|---|---|");

    for scenario in scenarios {
        let mut aris = Vec::with_capacity(args.trials);
        let mut nmis = Vec::with_capacity(args.trials);
        for t in 0..args.trials {
            let seed = args
                .seed
                .wrapping_add((t as u64).wrapping_mul(0x0000_0100_0000_01B3));
            let r = run_trial(scenario, &args, seed);
            aris.push(r.ari);
            nmis.push(r.nmi);
        }
        let (ari_mean, ari_std) = mean_std(&aris);
        let (nmi_mean, nmi_std) = mean_std(&nmis);
        let (ari_min, ari_max) = min_max(&aris);
        let (nmi_min, nmi_max) = min_max(&nmis);

        println!(
            "| {} | {:.4} ± {:.4} | {:.4} ± {:.4} | [{:.4}, {:.4}] | [{:.4}, {:.4}] |",
            scenario.label(),
            ari_mean,
            ari_std,
            nmi_mean,
            nmi_std,
            ari_min,
            ari_max,
            nmi_min,
            nmi_max
        );
    }

    println!();
    println!(
        "--- per-feature separability diagnostic (oracle: true operator labels, \
        one representative trial at --seed, NOT k-means's own output) ---"
    );
    println!(
        "Between/within-group variance ratio per feature — shows WHICH \
        features actually carry operator signal in each scenario, \
        independent of whether k-means finds it. Higher = more separable."
    );
    println!();
    print!("| Scenario |");
    for name in FEATURE_NAMES {
        print!(" {} |", name);
    }
    println!();
    print!("|---|");
    for _ in FEATURE_NAMES {
        print!("---|");
    }
    println!();
    for scenario in scenarios {
        let data = simulate_trial(scenario, &args, args.seed);
        let scores = feature_separability(&data.feature_rows, &data.true_labels);
        print!("| {} |", scenario.label());
        for s in scores {
            if s.is_finite() {
                print!(" {:.2} |", s);
            } else {
                print!(" inf |");
            }
        }
        println!();
    }

    println!();
    println!(
        "Scope: this measures clustering resistance on OBSERVABLE BEHAVIORAL \
        FEATURES ONLY (timing shape, active-hours signature, protocol mix) — \
        the same class of signal timing_harness already measures for a \
        single wallet, now aggregated across many. It does NOT model an \
        adversary with on-chain metadata (funding graph, common-funder \
        heuristics, address reuse) — that class of attack is explicitly out \
        of scope for account-cooker (see THREAT_MODEL.md) and unaffected by \
        this number. This is additional to timing_harness, not a \
        replacement for it."
    );
    println!(
        "identical_control is a sanity check, not a privacy claim: it exists \
        to prove this harness doesn't manufacture a \"high ARI\" result out \
        of nothing when there is genuinely no operator-level signal to \
        find. Every number above is reported as measured, including any \
        that don't favor account-cooker's current defaults — see \
        THREAT_MODEL.md for how each is scoped."
    );
    println!();
    println!(
        "Persona-jitter fix check: compare the 'wide_timing + diverse_persona \
        (... PRE-fix)' row against '... + agent_jitter (POST-fix ...)' above. \
        Operator-level generation is identical between the two (same \
        build_operator call, same RNG draws — see build_operators in this \
        file), so any ARI/NMI difference isolates the effect of per-agent \
        persona jitter (src/persona.rs) alone, not a change in how operators \
        themselves are generated."
    );
}

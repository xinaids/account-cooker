//! Periodic fund consolidation/redistribution across one operator's fleet.
//!
//! The bounty brief explicitly asks for a tool that "periodically
//! consolidates and redistributes assets" across an operator's wallets —
//! not implemented anywhere else in this crate until now. This module is
//! that: on a randomized, configurable cadence (reusing the same
//! `timing::sample_interval_secs` log-normal sampler the noise-transaction
//! scheduler uses, just at hour instead of minute granularity), one wallet
//! in the fleet transfers a small, randomized fraction of its balance to
//! another randomly-chosen sibling wallet.
//!
//! Deliberately NOT the same wallet pair every time and NOT a fixed
//! interval: a consolidation pattern that always moves funds
//! wallet-1 -> wallet-2 on a fixed schedule is itself a clustering tell (a
//! predictable "hub" pattern), which would undermine the exact thing this
//! tool exists to prevent. `pick_source_destination` and
//! `compute_transfer_lamports` below are the pure, unit-tested core;
//! `run_consolidation_loop` wires them to a real RPC send, matching the
//! simulate-then-send pattern already used in `protocols/marinade.rs`.
//!
//! Opt-in, disabled by default (`enabled = false`, see
//! `config::ConsolidationConfig`) — matches the existing precedent for
//! new, less-battle-tested features (`supersonic_cast`'s `weight = 0.0`
//! default) and is the conservative choice for a feature that moves real
//! funds between an operator's own wallets.

use rand::Rng;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::Transaction;
use std::sync::Arc;
use std::time::Duration;

use crate::config::ConsolidationConfig;
use crate::timing::sample_interval_secs;

/// One fleet member consolidation can move funds between — just enough to
/// sign and label a transfer, not a full `Agent` (consolidation runs as
/// its own fleet-wide task, not per-agent, since it needs visibility into
/// every sibling wallet's pubkey).
pub struct FleetWallet {
    pub label: String,
    pub keypair: Keypair,
}

/// Picks a (source_index, destination_index) pair among `n_wallets`
/// wallets: the source drawn uniformly from `eligible` (indices with
/// enough balance to safely move funds), the destination drawn uniformly
/// from every OTHER wallet (source excluded, but not required to also be
/// "eligible" — a low-balance sibling can still receive). Returns `None`
/// if there's no eligible source or fewer than 2 wallets total, so callers
/// never have to handle an empty range.
pub fn pick_source_destination(
    eligible: &[usize],
    n_wallets: usize,
    rng: &mut impl Rng,
) -> Option<(usize, usize)> {
    if eligible.is_empty() || n_wallets < 2 {
        return None;
    }
    let source = eligible[rng.gen_range(0..eligible.len())];
    let destination = loop {
        let candidate = rng.gen_range(0..n_wallets);
        if candidate != source {
            break candidate;
        }
    };
    Some((source, destination))
}

/// Computes how many lamports to move from a source with `balance`
/// lamports: a random `fraction` of the balance ABOVE `reserve_lamports`,
/// which is always left behind. Returns `None` if there's nothing safe to
/// move (balance at or below the reserve, or the computed amount rounds
/// to zero) rather than ever moving the whole balance or panicking on a
/// pathological config. All arithmetic is checked.
pub fn compute_transfer_lamports(
    balance: u64,
    fraction: f64,
    reserve_lamports: u64,
) -> Option<u64> {
    if !(0.0..=1.0).contains(&fraction) {
        return None;
    }
    let available = balance.checked_sub(reserve_lamports)?;
    if available == 0 {
        return None;
    }
    let amount = (available as f64 * fraction) as u64;
    if amount == 0 {
        return None;
    }
    Some(amount.min(available))
}

/// Runs the consolidation loop forever: sleep a randomized interval, then
/// attempt one consolidation transfer. Never spawned unless
/// `cfg.enabled` — the scheduler checks this before spawning the task at
/// all, but this function re-checks so it fails safe if ever called
/// directly.
pub async fn run_consolidation_loop(
    wallets: Vec<FleetWallet>,
    rpc: Arc<RpcClient>,
    cfg: ConsolidationConfig,
) {
    if !cfg.enabled || wallets.len() < 2 {
        return;
    }

    loop {
        let sleep_secs = sample_interval_secs(
            cfg.mean_interval_hours * 60.0,
            cfg.stddev_interval_hours * 60.0,
            &mut rand::thread_rng(),
        );
        tokio::time::sleep(Duration::from_secs(sleep_secs)).await;

        match consolidation_tick(&wallets, &rpc, &cfg).await {
            Ok(Some(sig)) => tracing::info!("[consolidation] transfer ok, sig={sig}"),
            Ok(None) => tracing::debug!("[consolidation] skipped this tick (no eligible move)"),
            Err(e) => tracing::warn!("[consolidation] tick failed: {e}"),
        }
    }
}

/// One consolidation attempt: fetch balances, pick an eligible source and
/// a destination, compute a safe amount, sign and send. Returns `Ok(None)`
/// (not an error) when there's simply nothing safe to move this tick —
/// that's an expected steady state, not a failure.
async fn consolidation_tick(
    wallets: &[FleetWallet],
    rpc: &RpcClient,
    cfg: &ConsolidationConfig,
) -> anyhow::Result<Option<Signature>> {
    let mut balances = Vec::with_capacity(wallets.len());
    for w in wallets {
        balances.push(rpc.get_balance(&w.keypair.pubkey()).await?);
    }

    let eligible: Vec<usize> = balances
        .iter()
        .enumerate()
        .filter(|&(_, &b)| b >= cfg.min_balance_lamports)
        .map(|(i, _)| i)
        .collect();

    // Scoped so `ThreadRng` (not `Send`) is dropped before the `.await`s
    // below — otherwise the enclosing future can't be spawned with
    // `tokio::spawn`, which requires every await-crossing local to be `Send`.
    let (source_idx, dest_idx, amount) = {
        let mut rng = rand::thread_rng();
        let Some((source_idx, dest_idx)) =
            pick_source_destination(&eligible, wallets.len(), &mut rng)
        else {
            return Ok(None);
        };

        let fraction = rng.gen_range(cfg.fraction_min..=cfg.fraction_max);
        let Some(amount) =
            compute_transfer_lamports(balances[source_idx], fraction, cfg.reserve_lamports)
        else {
            return Ok(None);
        };
        (source_idx, dest_idx, amount)
    };

    let source = &wallets[source_idx];
    let destination = &wallets[dest_idx];

    let ix = solana_system_interface::instruction::transfer(
        &source.keypair.pubkey(),
        &destination.keypair.pubkey(),
        amount,
    );
    let recent_blockhash = rpc.get_latest_blockhash().await?;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&source.keypair.pubkey()),
        &[&source.keypair],
        recent_blockhash,
    );

    // Simulate first, same pattern as marinade.rs / supersonic_cast.rs:
    // surface detailed logs on failure instead of committing an unverified
    // transfer to the network first.
    let sim = rpc.simulate_transaction(&tx).await?;
    if let Some(err) = &sim.value.err {
        let logs = sim
            .value
            .logs
            .as_ref()
            .map(|l| l.join("\n"))
            .unwrap_or_default();
        anyhow::bail!("consolidation transfer simulation failed: {err:?}\nlogs:\n{logs}");
    }

    tracing::debug!(
        "[consolidation] {} -> {} amount={}",
        source.label,
        destination.label,
        amount
    );

    let sig = rpc.send_and_confirm_transaction(&tx).await?;
    Ok(Some(sig))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    #[test]
    fn pick_source_destination_never_picks_source_as_destination() {
        let mut rng = ChaCha8Rng::seed_from_u64(1);
        for _ in 0..1000 {
            let (source, dest) = pick_source_destination(&[0, 1, 2], 5, &mut rng).unwrap();
            assert_ne!(source, dest);
            assert!(source < 5);
            assert!(dest < 5);
        }
    }

    #[test]
    fn pick_source_destination_only_draws_source_from_eligible() {
        let mut rng = ChaCha8Rng::seed_from_u64(2);
        for _ in 0..500 {
            let (source, _) = pick_source_destination(&[2, 3], 6, &mut rng).unwrap();
            assert!(source == 2 || source == 3);
        }
    }

    #[test]
    fn pick_source_destination_none_when_no_eligible_wallets() {
        let mut rng = ChaCha8Rng::seed_from_u64(3);
        assert!(pick_source_destination(&[], 5, &mut rng).is_none());
    }

    #[test]
    fn pick_source_destination_none_with_fewer_than_two_wallets() {
        let mut rng = ChaCha8Rng::seed_from_u64(4);
        assert!(pick_source_destination(&[0], 1, &mut rng).is_none());
    }

    #[test]
    fn pick_source_destination_never_infinite_loops_with_one_eligible_of_two() {
        // Regression guard: with exactly 2 wallets and the eligible one as
        // source, the destination-search loop must still terminate (there
        // is exactly one valid destination).
        let mut rng = ChaCha8Rng::seed_from_u64(5);
        for _ in 0..1000 {
            let (source, dest) = pick_source_destination(&[0], 2, &mut rng).unwrap();
            assert_eq!(source, 0);
            assert_eq!(dest, 1);
        }
    }

    #[test]
    fn compute_transfer_lamports_leaves_reserve_intact() {
        let amount = compute_transfer_lamports(10_000_000, 0.5, 2_000_000).unwrap();
        // available = 8_000_000, fraction 0.5 -> 4_000_000
        assert_eq!(amount, 4_000_000);
        assert!(10_000_000 - amount >= 2_000_000);
    }

    #[test]
    fn compute_transfer_lamports_none_when_balance_at_or_below_reserve() {
        assert!(compute_transfer_lamports(1_000_000, 0.5, 1_000_000).is_none());
        assert!(compute_transfer_lamports(500_000, 0.5, 1_000_000).is_none());
    }

    #[test]
    fn compute_transfer_lamports_none_for_out_of_range_fraction() {
        assert!(compute_transfer_lamports(10_000_000, -0.1, 0).is_none());
        assert!(compute_transfer_lamports(10_000_000, 1.1, 0).is_none());
    }

    #[test]
    fn compute_transfer_lamports_none_when_fraction_rounds_to_zero() {
        // available = 10 lamports, fraction 0.01 -> 0.1 -> truncates to 0.
        assert!(compute_transfer_lamports(10, 0.01, 0).is_none());
    }

    #[test]
    fn compute_transfer_lamports_never_exceeds_available_even_near_fraction_one() {
        let amount = compute_transfer_lamports(10_000_000, 1.0, 1_000_000).unwrap();
        assert_eq!(amount, 9_000_000);
    }

    #[test]
    fn compute_transfer_lamports_never_panics_on_pathological_inputs() {
        // Reserve larger than balance, zero balance, zero reserve with
        // zero balance — none of these should panic (checked_sub guards).
        assert!(compute_transfer_lamports(0, 0.5, 1).is_none());
        assert!(compute_transfer_lamports(0, 0.5, 0).is_none());
        assert!(compute_transfer_lamports(u64::MAX, 1.0, 0).is_some());
    }
}

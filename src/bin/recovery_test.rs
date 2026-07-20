//! Crash-recovery worker used by `scripts/recovery_test.sh`.
//!
//! This intentionally does NOT touch the network or a real wallet — it
//! exercises exactly the same checkpoint save/resume logic that
//! `Agent::run_forever` uses (see `src/agent/mod.rs`, `src/state.rs`),
//! against a real OS process that gets SIGKILLed mid-run. That is the part
//! worth proving: the checkpoint file survives an unclean kill without
//! corruption, and a restarted process resumes `action_count` instead of
//! duplicating the last action or resetting to zero.
//!
//! One checkpoint, not a multi-step journal — see `src/state.rs` module
//! docs and `THREAT_MODEL.md` for why that scope is honest for this repo.

use account_cooker::state::Checkpoint;
use clap::Parser;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    state_dir: PathBuf,
    #[arg(long, default_value = "test-agent")]
    label: String,
    /// Seconds between simulated actions.
    #[arg(long, default_value_t = 2)]
    interval_secs: u64,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn main() {
    let args = Args::parse();

    let mut action_count = 0u64;

    if let Some(cp) = Checkpoint::load(&args.state_dir, &args.label) {
        action_count = cp.action_count;
        let remaining = cp.next_action_due_unix - now_unix();
        println!(
            "RESUME action_count={} remaining={}s",
            action_count, remaining
        );
        if remaining > 0 {
            std::thread::sleep(std::time::Duration::from_secs(remaining as u64));
        }
    } else {
        println!("FRESH_START");
    }

    loop {
        std::thread::sleep(std::time::Duration::from_secs(args.interval_secs));
        action_count += 1;
        let cp = Checkpoint {
            last_action_unix: now_unix(),
            last_sig: Some(format!("sim-sig-{action_count}")),
            next_action_due_unix: now_unix() + args.interval_secs as i64,
            action_count,
        };
        cp.save(&args.state_dir, &args.label)
            .expect("checkpoint save must not fail in this test harness");
        println!("ACTION {action_count}");
        use std::io::Write;
        std::io::stdout().flush().ok();
    }
}

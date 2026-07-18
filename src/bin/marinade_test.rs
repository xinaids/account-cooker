//! Standalone proof-of-work driver for the marinade_stake protocol.
//!
//! Derives the Marinade PDAs/ATAs, prints them for manual sanity-checking,
//! simulates a real deposit against mainnet, and — only if `MARINADE_SEND=1`
//! is set — sends it for real using the wallet at `~/.config/solana/id.json`.
//!
//! Usage:
//!   cargo run --release --bin marinade_test                # derive + simulate only
//!   MARINADE_SEND=1 cargo run --release --bin marinade_test # simulate + send for real
//!
//! This is a one-shot manual proof tool (mirrors how jupiter_swap's mainnet
//! proof was generated), not part of the regular test suite.

use account_cooker::protocols::{marinade::MarinadeStake, Protocol};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::{read_keypair_file, Signer};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let rpc_url = "https://api.mainnet-beta.solana.com".to_string();
    let rpc = RpcClient::new(rpc_url);

    let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME env var not set"))?;
    let keypair_path = format!("{home}/.config/solana/id.json");
    let wallet = read_keypair_file(&keypair_path)
        .map_err(|e| anyhow::anyhow!("failed to read keypair at {keypair_path}: {e}"))?;

    println!("wallet: {}", wallet.pubkey());
    let balance = rpc.get_balance(&wallet.pubkey()).await?;
    println!(
        "balance: {} lamports ({} SOL)",
        balance,
        balance as f64 / 1e9
    );

    // Small, config-driven amount: max_balance_fraction picks ~1-2% of
    // balance, min_stake_lamports floor is low enough not to block it.
    let params: toml::Table = toml::from_str(
        r#"
        max_balance_fraction = 0.20
        min_stake_lamports = 1000000
        "#,
    )?;
    let protocol = MarinadeStake::from_params(&params)?;

    if std::env::var("MARINADE_SEND").as_deref() == Ok("1") {
        println!("MARINADE_SEND=1 set — simulating then sending real deposit...");
        let sig = protocol.execute(&rpc, &wallet).await?;
        println!("CONFIRMED: https://solscan.io/tx/{sig}");
    } else {
        println!(
            "MARINADE_SEND not set — skipping. Set MARINADE_SEND=1 to simulate+send for real."
        );
        println!(
            "(execute() always simulates before sending, and bails with logs if simulation fails.)"
        );
    }

    Ok(())
}

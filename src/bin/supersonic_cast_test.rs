//! Standalone proof-of-work driver for the supersonic_cast protocol.
//!
//! Runs `SupersonicCast::execute` — the exact code path the real agent loop calls
//! — against devnet, through the account-cooker `Protocol` trait, and confirms the
//! resulting bundle transaction on-chain. Mirrors how jupiter_swap and
//! marinade_stake's proofs were generated (see marinade_test.rs).
//!
//! Usage:
//!   cargo run --release --bin supersonic_cast_test -- <keypair-path>
//!
//! The keypair must be funded on devnet (`solana airdrop 1 <pubkey> --url devnet`).

use account_cooker::protocols::{supersonic_cast::SupersonicCast, Protocol};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::{read_keypair_file, Signer};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let keypair_path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: supersonic_cast_test <keypair-path>"))?;

    let rpc = RpcClient::new("https://api.devnet.solana.com".to_string());
    let wallet = read_keypair_file(&keypair_path)
        .map_err(|e| anyhow::anyhow!("failed to read keypair at {keypair_path}: {e}"))?;

    println!("wallet: {}", wallet.pubkey());
    let balance = rpc.get_balance(&wallet.pubkey()).await?;
    println!("balance: {balance} lamports ({} SOL)", balance as f64 / 1e9);

    let params: toml::Table = toml::from_str(
        r#"
        max_balance_fraction = 0.05
        min_cast_lamports = 1000000
        k = 8
        "#,
    )?;
    let protocol = SupersonicCast::from_params(&params)?;

    println!("casting a bundle through supersonic-tx (real send + confirm)...");
    let sig = protocol.execute(&rpc, &wallet).await?;
    println!("CONFIRMED: https://explorer.solana.com/tx/{sig}?cluster=devnet");

    Ok(())
}

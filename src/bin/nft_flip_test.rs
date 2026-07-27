//! Standalone proof-of-work driver for the nft_flip protocol.
//!
//! Runs `NftFlip::execute_returning_mint` — the same shared `build_and_simulate`
//! code path the real agent loop's `Protocol::execute` calls — against whatever
//! cluster/RPC you point it at, minting a fresh 1/1 NFT and (by default) flipping
//! it to a sibling address derived from the wallet. Mirrors how
//! dao_vote_test.rs / marinade_test.rs generate their proofs.
//!
//! Usage:
//!   cargo run --release --bin nft_flip_test -- <keypair-path> <rpc-url> [flip_to_sibling]
//!
//! `flip_to_sibling` defaults to "true" (true|false accepted). Without
//! `NFT_FLIP_SEND=1` this only simulates (via `NftFlip::simulate`, never sends)
//! and reports the mint address a real call would create. Set `NFT_FLIP_SEND=1`
//! to simulate then actually send.

use account_cooker::protocols::nft_flip::NftFlip;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::{read_keypair_file, Signer};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let keypair_path = args.next().ok_or_else(|| {
        anyhow::anyhow!("usage: nft_flip_test <keypair-path> <rpc-url> [flip_to_sibling]")
    })?;
    let rpc_url = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing <rpc-url>"))?;
    let flip_to_sibling = args.next().unwrap_or_else(|| "true".to_string());

    let rpc = RpcClient::new(rpc_url.clone());
    let wallet = read_keypair_file(&keypair_path)
        .map_err(|e| anyhow::anyhow!("failed to read keypair at {keypair_path}: {e}"))?;

    println!("cluster: {rpc_url}");
    println!("wallet: {}", wallet.pubkey());
    let balance = rpc.get_balance(&wallet.pubkey()).await?;
    println!("balance: {balance} lamports ({} SOL)", balance as f64 / 1e9);
    println!("flip_to_sibling: {flip_to_sibling}");

    let params: toml::Table = toml::from_str(&format!(
        r#"
        flip_to_sibling = {flip_to_sibling}
        "#
    ))?;
    let protocol = NftFlip::from_params(&params)?;

    if std::env::var("NFT_FLIP_SEND").as_deref() == Ok("1") {
        println!("NFT_FLIP_SEND=1 set — simulating then sending real mint...");
        let (sig, mint) = protocol.execute_returning_mint(&rpc, &wallet).await?;
        println!("CONFIRMED: signature {sig}");
        println!("  mint: {mint}");
        println!("  mainnet tx:   https://solscan.io/tx/{sig}");
        println!("  devnet tx:    https://explorer.solana.com/tx/{sig}?cluster=devnet");
        println!("  mainnet mint: https://solscan.io/token/{mint}");
        println!("  devnet mint:  https://explorer.solana.com/address/{mint}?cluster=devnet");
    } else {
        println!(
            "NFT_FLIP_SEND not set — simulating only (never sends). Set NFT_FLIP_SEND=1 to send for real."
        );
        let mint = protocol.simulate(&rpc, &wallet).await?;
        println!("SIMULATION CLEAN — no error, no transaction sent.");
        println!("  hypothetical mint (not created, simulation only): {mint}");
    }

    Ok(())
}

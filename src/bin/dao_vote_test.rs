//! Standalone proof-of-work driver for the dao_vote protocol.
//!
//! Runs `DaoVote::execute` — the exact code path the real agent loop calls —
//! against a real SPL Governance (Realms) proposal on whatever cluster/RPC
//! you point it at, and confirms the resulting transaction on-chain. Mirrors
//! how marinade_test.rs / supersonic_cast_test.rs generate their proofs.
//!
//! Usage:
//!   cargo run --release --bin dao_vote_test -- <keypair-path> <rpc-url> <proposal-pubkey> [vote-choice]
//!
//! `vote-choice` defaults to "abstain" (approve|deny|abstain|veto accepted).
//! Without `DAO_VOTE_SEND=1` this only simulates (via `DaoVote::simulate`,
//! never sends) and reports the result — unlike marinade_test.rs, which does
//! nothing at all in this branch, dry-run here is a real, useful check on its
//! own since this binary can be pointed at mainnet. Set `DAO_VOTE_SEND=1` to
//! simulate then actually send.

use account_cooker::protocols::{dao_vote::DaoVote, Protocol};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::{read_keypair_file, Signer};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let keypair_path = args.next().ok_or_else(|| {
        anyhow::anyhow!(
            "usage: dao_vote_test <keypair-path> <rpc-url> <proposal-pubkey> [vote-choice]"
        )
    })?;
    let rpc_url = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing <rpc-url>"))?;
    let proposal_pubkey = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing <proposal-pubkey>"))?;
    let vote_choice = args.next().unwrap_or_else(|| "abstain".to_string());

    let rpc = RpcClient::new(rpc_url.clone());
    let wallet = read_keypair_file(&keypair_path)
        .map_err(|e| anyhow::anyhow!("failed to read keypair at {keypair_path}: {e}"))?;

    println!("cluster: {rpc_url}");
    println!("wallet: {}", wallet.pubkey());
    let balance = rpc.get_balance(&wallet.pubkey()).await?;
    println!("balance: {balance} lamports ({} SOL)", balance as f64 / 1e9);
    println!("proposal: {proposal_pubkey}");
    println!("vote_choice: {vote_choice}");

    let params: toml::Table = toml::from_str(&format!(
        r#"
        proposal_pubkey = "{proposal_pubkey}"
        vote_choice = "{vote_choice}"
        "#
    ))?;
    let protocol = DaoVote::from_params(&params)?;

    if std::env::var("DAO_VOTE_SEND").as_deref() == Ok("1") {
        println!("DAO_VOTE_SEND=1 set — simulating then sending real vote...");
        let sig = protocol.execute(&rpc, &wallet).await?;
        println!("CONFIRMED: signature {sig}");
        println!("  mainnet: https://solscan.io/tx/{sig}");
        println!("  devnet:  https://explorer.solana.com/tx/{sig}?cluster=devnet");
    } else {
        println!("DAO_VOTE_SEND not set — simulating only (never sends). Set DAO_VOTE_SEND=1 to send for real.");
        protocol.simulate(&rpc, &wallet).await?;
        println!("SIMULATION CLEAN — no error, no transaction sent.");
    }

    Ok(())
}

//! Send a single Solana transaction via Nozomi QUIC.
//!
//! Env vars:
//!   NOZOMI_API_KEY  - Nozomi API key
//!   RPC_URL         - Solana RPC endpoint
//!   FEE_PAYER       - Path to keypair JSON file
//!   NOZOMI_HOST     - (optional) default: edge.nozomi.temporal.xyz
//!
//! cargo run --example send_single

use nozomi_quic_client::{NozomiConfig, NozomiQuicClient};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    signature::Keypair,
    signer::Signer,
    message::Message,
    transaction::Transaction,
};
use solana_system_interface::instruction as system_instruction;
use std::env;
use std::time::Duration;

fn load_keypair(path: &str) -> Keypair {
    let file = std::fs::read_to_string(path).expect("read keypair file");
    let bytes: Vec<u8> = serde_json::from_str(&file).expect("parse keypair JSON");
    Keypair::try_from(bytes.as_slice()).expect("valid keypair")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let api_key = env::var("NOZOMI_API_KEY").expect("NOZOMI_API_KEY not set");
    let host = env::var("NOZOMI_HOST").unwrap_or("edge.nozomi.temporal.xyz".into());
    let rpc_url = env::var("RPC_URL").expect("RPC_URL not set");
    let fee_payer_path = env::var("FEE_PAYER").expect("FEE_PAYER not set");

    let fee_payer = load_keypair(&fee_payer_path);
    let rpc = RpcClient::new(rpc_url);
    println!("fee payer: {}", fee_payer.pubkey());

    let blockhash = rpc.get_latest_blockhash().await?;

    // Build a simple self-transfer (replace with your tx logic)
    let ix = system_instruction::transfer(&fee_payer.pubkey(), &fee_payer.pubkey(), 1);
    let msg = Message::new(&[ix], Some(&fee_payer.pubkey()));
    let tx = Transaction::new(&[&fee_payer], msg, blockhash);
    let signature = tx.signatures[0];
    let tx_bytes = wincode::serialize(&tx)?;

    // Connect and send
    let mut client = NozomiQuicClient::new(NozomiConfig::new(host, api_key))?;
    client.warmup().await?;
    client.send_batch(&[&tx_bytes]).await?;
    println!("sent {signature} ({} bytes)", tx_bytes.len());

    // Wait for confirmation
    println!("confirming ...");
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let statuses = rpc.get_signature_statuses(&[signature]).await?.value;
        if let Some(Some(status)) = statuses.first() {
            if let Some(ref err) = status.err {
                println!("transaction failed: {err}");
                return Ok(());
            }
            println!("confirmed (status: {:?})", status.confirmation_status);
            return Ok(());
        }
    }
    println!("timed out waiting for confirmation");

    Ok(())
}

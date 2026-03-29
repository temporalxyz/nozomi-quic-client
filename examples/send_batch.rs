//! Send a batch of Solana transactions via Nozomi QUIC.
//!
//! Env vars:
//!   NOZOMI_API_KEY  - Nozomi API key
//!   RPC_URL         - Solana RPC endpoint
//!   FEE_PAYER       - Path to keypair JSON file
//!   NOZOMI_HOST     - (optional) default: edge.nozomi.temporal.xyz
//!
//! cargo run --example send_batch

use nozomi_quic_client::{NozomiConfig, NozomiQuicClient};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    signature::{Keypair, Signature},
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

async fn confirm_signatures(rpc: &RpcClient, signatures: &[Signature]) {
    for sig in signatures {
        println!("confirming {sig} ...");
    }

    let mut confirmed = vec![false; signatures.len()];
    for _ in 0..60 {
        if confirmed.iter().all(|c| *c) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;

        let statuses = match rpc.get_signature_statuses(signatures).await {
            Ok(resp) => resp.value,
            Err(_) => continue,
        };

        for (i, status) in statuses.iter().enumerate() {
            if confirmed[i] {
                continue;
            }
            if let Some(status) = status {
                if let Some(ref err) = status.err {
                    println!("  {} failed: {err}", signatures[i]);
                    confirmed[i] = true;
                } else {
                    println!("  {} confirmed ({:?})", signatures[i], status.confirmation_status);
                    confirmed[i] = true;
                }
            }
        }
    }

    let pending = confirmed.iter().filter(|c| !**c).count();
    if pending > 0 {
        println!("{pending} transaction(s) timed out waiting for confirmation");
    }
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

    // Build 4 transactions with different amounts (replace with your tx logic)
    let mut signatures = Vec::new();
    let transactions: Vec<Vec<u8>> = (1..=4u64)
        .map(|lamports| {
            let ix =
                system_instruction::transfer(&fee_payer.pubkey(), &fee_payer.pubkey(), lamports);
            let msg = Message::new(&[ix], Some(&fee_payer.pubkey()));
            let tx = Transaction::new(&[&fee_payer], msg, blockhash);
            signatures.push(tx.signatures[0]);
            wincode::serialize(&tx).expect("serialize tx")
        })
        .collect();

    let tx_refs: Vec<&[u8]> = transactions.iter().map(|t| t.as_slice()).collect();

    // Connect and send batch
    let mut client = NozomiQuicClient::new(NozomiConfig::new(host, api_key))?;
    client.warmup().await?;
    client.send_batch(&tx_refs).await?;
    println!("batch sent ({} transactions)", transactions.len());

    // Wait for confirmations
    confirm_signatures(&rpc, &signatures).await;

    Ok(())
}

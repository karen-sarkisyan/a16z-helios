// src/ethereum.rs

use crate::harness::TestHarness;
use alloy::primitives::U64;
use alloy::providers::{Provider, ProviderBuilder};
use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use url::Url;

// Helper struct for deserializing the JSON-RPC response for eth_chainId
#[derive(Deserialize, Debug)]
struct JsonRpcResponse {
    result: String, // Chain ID as hex string e.g., "0x1"
}

// Basic test to check if Helios starts and RPC is reachable,
// and if it reports the same chain ID as the EL client.
#[tokio::test]
// No longer ignored: #[ignore] // Ignored until harness setup is implemented
async fn test_chain_id() -> Result<()> {
    // Initialize logging (optional, good for debugging)
    // let _ = tracing_subscriber::fmt::try_init();

    // 1. Setup the harness for Ethereum
    let harness = TestHarness::setup(/* Some config */).await?;
    println!("Harness setup complete");

    // 2. Get chain ID from the Execution Layer (EL) client directly
    let el_rpc_url = Url::parse(&harness.el_url)?;
    let el_provider = ProviderBuilder::new().on_http(el_rpc_url);
    let el_chain_id_u64 = el_provider.get_chain_id().await?;
    let el_chain_id: u64 = el_chain_id_u64.into(); // Convert U64 to u64

    // 3. Get chain ID from Helios
    let helios_rpc_url_str = harness.helios_rpc_url();
    let helios_rpc_url = Url::parse(helios_rpc_url_str)?;
    let helios_provider = ProviderBuilder::new().on_http(helios_rpc_url);
    let helios_chain_id_u64 = helios_provider.get_chain_id().await?;
    let helios_chain_id: u64 = helios_chain_id_u64.into(); // Convert U64 to u64

    // 4. Assert the results
    assert_eq!(
        el_chain_id, helios_chain_id,
        "Chain ID mismatch: EL client returned {}, Helios returned {}",
        el_chain_id, helios_chain_id
    );

    println!(
        "Successfully fetched chain ID from EL client ({}) and Helios ({}). They match!",
        el_chain_id, helios_chain_id
    );

    Ok(())
}

// Add more Ethereum-specific E2E tests here...

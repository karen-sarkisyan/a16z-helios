// src/harness/mod.rs

// This module will contain the logic for setting up and tearing down
// the test environment (Kurtosis, Helios instance, etc.).

use kurtosis_sdk::{
    enclave_api::{api_container_service_client::ApiContainerServiceClient, GetServicesArgs},
    engine_api::engine_service_client::EngineServiceClient,
};

use anyhow::Result;
use duct::{cmd, Handle, ReaderHandle};
use figment::value::{Dict, Value};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use toml;

/// Represents the running test environment.
pub struct TestHarness {
    pub cl_url: String,
    pub el_url: String,
    pub helios_url: String,
    helios_handle: Option<ReaderHandle>,
    path_to_config: PathBuf,
}

impl TestHarness {
    /// Sets up the test environment for a specific network.
    pub async fn setup(/* config: NetworkConfig */) -> Result<Self> {
        // 1. Start Kurtosis environment and run an ethereum-package enclave
        let args = vec![
            "run",
            "--enclave",
            "helios-testnet",
            "github.com/ethpandaops/ethereum-package",
        ];
        let kurtosis_engine_start = cmd("kurtosis", args)
            .stdout_capture() // Suppress stdout
            .stderr_to_stdout()
            .run()?;
        println!("Kurtosis engine started");
        let (cl_url, el_url) = get_node_urls().await?;
        // 5. Prepare Helios config (temp data dir, pass network details) with data from step 4
        // 6. Fetch checkpoint from `/eth/v1/beacon/blocks/finalized/root`
        let path_to_config = generate_config_file(&cl_url, &el_url).await?;

        // 7. Build Helios CLI
        let build_helios = cmd(
            "cargo",
            vec![
                "build",
                // "--release", // TODO: uncomment when time comes
                "--manifest-path",
                "../Cargo.toml", // Path to workspace root Cargo.toml
            ],
        )
        .run()?;
        println!("Built Helios: {:?}", build_helios);

        // Get the absolute path to the built binary
        let helios_binary = std::env::current_dir()?
            .parent() // go up to workspace root
            .unwrap()
            // .join("target/release/helios"); // path to release binary
            .join("target/debug/helios"); // path to dev binary
        let helios_args = vec![
            "ethereum",
            "--config-path",
            path_to_config.to_str().unwrap(),
        ];

        // Give Kurtosis some time before starting Helios
        // TODO remove
        println!("Sleeping for 25 seconds to give testnet some time");
        tokio::time::sleep(Duration::from_secs(25)).await;

        // Start Helios process and capture output
        let helios_reader_handle = cmd(helios_binary, helios_args)
            .stderr_to_stdout() // Merge stderr into stdout
            .reader()?; // Get a reader instead of capturing

        // Wait for Helios to sync
        wait_for_sync(&helios_reader_handle)?;

        println!("Helios is now synced and ready");
        Ok(Self {
            cl_url,
            el_url,
            helios_handle: Some(helios_reader_handle),
            helios_url: "http://127.0.0.1:8545".to_string(), // TODO: get from Helios output or hardcode to config
            path_to_config,
        })
    }

    /// Returns the RPC URL for the running Helios instance.
    pub fn helios_rpc_url(&self) -> &str {
        // Replace with actual URL
        "http://127.0.0.1:8545"
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        println!("Dropping test harness");
        if self.path_to_config.exists() {
            println!("Removing generated config file: {:?}", self.path_to_config);
            if let Err(e) = fs::remove_file(&self.path_to_config) {
                eprintln!("Failed to remove config file: {}", e);
            }
        }

        let args = vec!["enclave", "stop", "helios-testnet"];
        let kurtosis_engine_stop = cmd("kurtosis", args).run().unwrap();
        println!("Kurtosis engine stop: {:?}", kurtosis_engine_stop);
        let args = vec!["clean"];
        let kurtosis_clean = cmd("kurtosis", args).run().unwrap();
        println!("Kurtosis clean: {:?}", kurtosis_clean);
        let args = vec!["engine", "stop"];
        let kurtosis_engine_stop = cmd("kurtosis", args).run().unwrap();
        println!("Kurtosis engine stop: {:?}", kurtosis_engine_stop);
        self.helios_handle.take().unwrap().kill().unwrap();
        println!("Helios killed");
    }
}

/**
 * Using Kurtosis SDK to connect to Kurtosis enclave and get CL and EL RPC URLs
 * for the test harness.
*/
async fn get_node_urls() -> Result<(String, String)> {
    let mut engine = EngineServiceClient::connect("https://[::1]:9710").await?;
    let mut enclaves = engine.get_enclaves(()).await?;
    // println!("Enclaves: {:?}", enclaves);
    let enclave_response = enclaves.into_inner();
    let enclave_info = enclave_response
        .enclave_info
        .values()
        .find(|info| info.name == "helios-testnet") // TODO change hardcoded
        .expect("Helios testnet enclave not found");

    let enclave_port = enclave_info
        .api_container_host_machine_info
        .as_ref()
        .expect("API container host machine info must be present")
        .grpc_port_on_host_machine;

    // println!("FOUND Enclave port: {:?}", enclave_port);
    let mut enclave =
        ApiContainerServiceClient::connect(format!("https://[::1]:{}", enclave_port)).await?;
    let services = enclave
        .get_services(GetServicesArgs {
            service_identifiers: HashMap::new(),
        })
        .await?;
    // println!("Services: {:?}", services);
    let service_response = services.into_inner();
    // get the port numbers for the first service whose name starts with "cl-"

    let mut cl_rpc_url = String::new();
    let mut el_rpc_url = String::new();

    for (name, service_info) in service_response.service_info.iter() {
        if name.starts_with("cl-") {
            if let Some(port_info) = service_info.maybe_public_ports.get("http") {
                cl_rpc_url = format!("http://127.0.0.1:{}", port_info.number);
            }
        } else if name.starts_with("el-") {
            if let Some(port_info) = service_info.maybe_public_ports.get("rpc") {
                el_rpc_url = format!("http://127.0.0.1:{}", port_info.number);
            }
        }
    }

    if cl_rpc_url.is_empty() || el_rpc_url.is_empty() {
        anyhow::bail!("Could not find CL or EL RPC URLs from Kurtosis services");
    }

    println!("CL RPC URL: {}", cl_rpc_url);
    println!("EL RPC URL: {}", el_rpc_url);
    Ok((cl_rpc_url, el_rpc_url))
}

// Structs for deserializing API responses (needed again)
#[derive(Deserialize, Debug)]
struct ApiData<T> {
    data: T,
}

#[derive(Deserialize, Debug)]
struct ConfigSpecResponse {
    #[serde(rename = "DEPOSIT_CHAIN_ID")]
    deposit_chain_id: String,
    #[serde(rename = "GENESIS_FORK_VERSION")]
    genesis_fork_version: String,
    #[serde(rename = "ALTAIR_FORK_VERSION")]
    altair_fork_version: String,
    #[serde(rename = "ALTAIR_FORK_EPOCH")]
    altair_fork_epoch: String,
    #[serde(rename = "BELLATRIX_FORK_VERSION")]
    bellatrix_fork_version: String,
    #[serde(rename = "BELLATRIX_FORK_EPOCH")]
    bellatrix_fork_epoch: String,
    #[serde(rename = "CAPELLA_FORK_VERSION")]
    capella_fork_version: String,
    #[serde(rename = "CAPELLA_FORK_EPOCH")]
    capella_fork_epoch: String,
    #[serde(rename = "DENEB_FORK_VERSION")]
    deneb_fork_version: String,
    #[serde(rename = "DENEB_FORK_EPOCH")]
    deneb_fork_epoch: String,
    // Add ELECTRA if needed
}

#[derive(Deserialize, Debug)]
struct GenesisResponse {
    genesis_time: String,
    genesis_validators_root: String,
}

#[derive(Deserialize, Debug)]
struct BlockRootResponse {
    root: String,
}

/**
 * Collect relevant data about consensus layer of the network to let Helios
 * bootstrap with trusted checkpoint, genesis time, and fork schedule.
 * TODO: rework
 */
async fn generate_config_file(cl_url: &str, el_url: &str) -> Result<PathBuf> {
    let client = reqwest::Client::new();

    // 1. Fetch spec
    let spec_url = format!("{}/eth/v1/config/spec", cl_url);
    let spec_data: ApiData<ConfigSpecResponse> = client
        .get(&spec_url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    // 2. Fetch genesis
    let genesis_url = format!("{}/eth/v1/beacon/genesis", cl_url);
    let genesis_data: ApiData<GenesisResponse> = client
        .get(&genesis_url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    // 3. Fetch finalized block root to use as a checkpoint
    let checkpoint_url = format!("{}/eth/v1/beacon/blocks/finalized/root", cl_url);
    let checkpoint_data: ApiData<BlockRootResponse> = client
        .get(&checkpoint_url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    // Parse data
    let genesis_time: u64 = genesis_data.data.genesis_time.parse()?;
    let chain_id: u64 = spec_data.data.deposit_chain_id.parse()?;
    let genesis_root = genesis_data.data.genesis_validators_root;
    let checkpoint = checkpoint_data.data.root;

    // Build config using figment::Dict
    let mut chain_config = Dict::new();
    chain_config.insert("genesis_time".into(), Value::from(genesis_time));
    chain_config.insert("chain_id".into(), Value::from(chain_id));
    chain_config.insert("genesis_root".into(), Value::from(genesis_root));

    let mut forks_config = Dict::new();
    let mut genesis_fork = Dict::new();
    genesis_fork.insert("epoch".into(), Value::from(0u64));
    genesis_fork.insert(
        "fork_version".into(),
        Value::from(spec_data.data.genesis_fork_version),
    );
    forks_config.insert("genesis".into(), Value::from(genesis_fork));

    let mut altair_fork = Dict::new();
    altair_fork.insert(
        "epoch".into(),
        Value::from(spec_data.data.altair_fork_epoch.parse::<u64>()?),
    );
    altair_fork.insert(
        "fork_version".into(),
        Value::from(spec_data.data.altair_fork_version),
    );
    forks_config.insert("altair".into(), Value::from(altair_fork));

    let mut bellatrix_fork = Dict::new();
    bellatrix_fork.insert(
        "epoch".into(),
        Value::from(spec_data.data.bellatrix_fork_epoch.parse::<u64>()?),
    );
    bellatrix_fork.insert(
        "fork_version".into(),
        Value::from(spec_data.data.bellatrix_fork_version),
    );
    forks_config.insert("bellatrix".into(), Value::from(bellatrix_fork));

    let mut capella_fork = Dict::new();
    capella_fork.insert(
        "epoch".into(),
        Value::from(spec_data.data.capella_fork_epoch.parse::<u64>()?),
    );
    capella_fork.insert(
        "fork_version".into(),
        Value::from(spec_data.data.capella_fork_version),
    );
    forks_config.insert("capella".into(), Value::from(capella_fork));

    let mut deneb_fork = Dict::new();
    deneb_fork.insert(
        "epoch".into(),
        Value::from(spec_data.data.deneb_fork_epoch.parse::<u64>()?),
    );
    deneb_fork.insert(
        "fork_version".into(),
        Value::from(spec_data.data.deneb_fork_version),
    );
    forks_config.insert("deneb".into(), Value::from(deneb_fork));
    // Add electra if needed

    let mut mainnet_config = Dict::new();
    mainnet_config.insert("consensus_rpc".into(), Value::from(cl_url));
    mainnet_config.insert("execution_rpc".into(), Value::from(el_url));
    mainnet_config.insert("checkpoint".into(), Value::from(checkpoint));
    mainnet_config.insert("chain".into(), Value::from(chain_config));
    mainnet_config.insert("forks".into(), Value::from(forks_config));

    let mut helios_config = Dict::new();
    helios_config.insert("mainnet".into(), Value::from(mainnet_config));

    // Serialize to TOML string using the `toml` crate (figment Dict implements Serialize)
    let toml_string = toml::to_string_pretty(&helios_config)?;

    // Define the file path (e.g., in the e2e-tests directory)
    // Using relative path from workspace root
    let config_path = PathBuf::from("helios-e2e-temp-config.toml");

    // Write the TOML string to the file using std::fs::write
    fs::write(&config_path, toml_string)?;
    println!("Generated Helios config file at: {:?}", config_path);

    Ok(config_path)
}

/// Waits for Helios to sync by monitoring its output.
/// Returns an error if sync doesn't complete within the timeout period.
fn wait_for_sync(reader_handle: &ReaderHandle) -> Result<()> {
    const SYNC_TIMEOUT: Duration = Duration::from_secs(20); // Adjust timeout as needed
    const SYNC_MESSAGE: &str = "consensus client in sync with checkpoint:";

    let start_time = Instant::now();
    let reader = BufReader::new(reader_handle);

    for line in reader.lines() {
        let line = line?;
        println!("Helios message: {:#?}", line);

        if line.contains(SYNC_MESSAGE) {
            return Ok(());
        }

        if start_time.elapsed() > SYNC_TIMEOUT {
            anyhow::bail!("Timeout waiting for Helios to sync");
        }
    }

    anyhow::bail!("Reader closed before sync message was found")
}

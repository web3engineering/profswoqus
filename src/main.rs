use anyhow::Result;
use base64::{engine::general_purpose, Engine as _};
use bincode;
use bs58;
use clap::Parser;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer,
    system_instruction,
    transaction::{Transaction, TransactionError, VersionedTransaction},
};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

// Jito imports
use jito_sdk_rust::{
    block_engine::{BlockEngineClient, Bundle},
    jito_rpc::JitoRpcClient,
};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, value_delimiter = ',')]
    addresses: Vec<String>,

    #[arg(short, long, default_value = "8080")]
    port: u16,

    /// Jito block engine server address
    #[arg(long, default_value = "https://mainnet.block-engine.jito.wtf")]
    jito_server: String,

    /// Path to Jito signer private key file (base58 encoded)
    #[arg(long)]
    jito_signer_key: Option<String>,

    /// Enable Jito forwarding for high-tip transactions
    #[arg(long)]
    enable_jito: bool,
}

#[derive(Deserialize, Serialize)]
struct RpcRequest {
    method: String,
    params: Vec<Value>,
    id: Value,
}

#[derive(Serialize)]
struct RpcResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
    id: Value,
    jsonrpc: String,
}

type ConnectionPool = Arc<RwLock<HashMap<String, reqwest::Client>>>;

const TARGET_PUBKEY: &str = "GxkB4oYYLsoeAoxAdXjDEBSrP7JGCy3re7mqozFYyiYW";
const TARGET_LAMPORTS: u64 = 100_000;
const JITO_TIP_THRESHOLD: u64 = 1_000_000; // 1M lamports threshold for Jito forwarding
const JITO_TIP_REDUCTION: u64 = 100_000; // 100K lamports to reduce from tip

/// Shared application state containing Jito client and configuration
#[derive(Clone)]
struct AppState {
    connections: ConnectionPool,
    jito_client: Option<Arc<JitoRpcClient>>,
    jito_keypair: Option<Arc<Keypair>>,
    jito_enabled: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing subscriber for logging
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    // Ensure at least one RPC address is provided
    if args.addresses.is_empty() {
        error!("No addresses provided. Please specify at least one RPC address using -a or --addresses.");
        std::process::exit(1);
    }

    // Create a shared connection pool for reqwest clients
    let connections = Arc::new(RwLock::new(HashMap::new()));

    // Initialize reqwest clients for each provided RPC address
    for addr in &args.addresses {
        let client = reqwest::Client::new(); // reqwest::Client is cheap to clone and can be reused
        connections.write().await.insert(addr.clone(), client);
        info!("Added connection to RPC: {}", addr);
    }

    // Initialize Jito components if enabled
    let (jito_client, jito_keypair) = if args.enable_jito {
        let keypair = if let Some(key_path) = &args.jito_signer_key {
            load_keypair_from_file(key_path)?
        } else {
            error!("Jito enabled but no signer key provided. Use --jito-signer-key");
            std::process::exit(1);
        };

        let jito_client = JitoRpcClient::new(&args.jito_server)?;
        info!("Initialized Jito client with server: {}", args.jito_server);
        info!("Jito signer pubkey: {}", keypair.pubkey());
        
        (Some(Arc::new(jito_client)), Some(Arc::new(keypair)))
    } else {
        info!("Jito forwarding disabled");
        (None, None)
    };

    // Create application state
    let app_state = AppState {
        connections,
        jito_client,
        jito_keypair,
        jito_enabled: args.enable_jito,
    };

    // Define the server address to bind to
    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));

    // Create a service for handling incoming HTTP requests
    let make_svc = make_service_fn(move |_conn| {
        let app_state = app_state.clone(); // Clone state for each service instance
        async move {
            // For each request, create a service_fn closure
            Ok::<_, Infallible>(service_fn(move |req| {
                // Clone state for each request handler
                handle_request(req, app_state.clone())
            }))
        }
    });

    // Create and run the Hyper server
    let server = Server::bind(&addr).serve(make_svc);

    info!("Proxy server listening on http://{}", addr);

    // Await the server, handling potential errors
    if let Err(e) = server.await {
        error!("Server error: {}", e);
    }

    Ok(())
}

/// Handles incoming HTTP requests, acting as a proxy for Solana RPC calls.
async fn handle_request(
    req: Request<Body>,
    app_state: AppState,
) -> Result<Response<Body>, Infallible> {
    // Only allow POST requests
    if req.method() != Method::POST {
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Body::from("Method not allowed"))
            .unwrap());
    }

    // Read the request body bytes
    let body_bytes = match hyper::body::to_bytes(req.into_body()).await {
        Ok(bytes) => bytes,
        Err(e) => {
            error!("Failed to read request body: {}", e);
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("Failed to read request body"))
                .unwrap());
        }
    };

    // Deserialize the request body into an RpcRequest struct
    let rpc_request: RpcRequest = match serde_json::from_slice(&body_bytes) {
        Ok(req) => req,
        Err(e) => {
            error!("Invalid JSON in request body: {}", e);
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("Invalid JSON"))
                .unwrap());
        }
    };

    // Check if the RPC method is "sendTransaction"
    if rpc_request.method == "sendTransaction" {
        // Extract the transaction data (Base64 string) from the parameters
        if let Some(tx_data) = rpc_request.params.get(0).and_then(|p| p.as_str()) {
            match process_transaction(tx_data, &body_bytes, &app_state).await {
                Ok(signature_opt) => {
                    let response = if let Some(signature) = signature_opt {
                        // If transaction contained target transfer and was forwarded
                        RpcResponse {
                            jsonrpc: "2.0".to_string(),
                            result: Some(Value::String(signature)),
                            error: None,
                            id: rpc_request.id,
                        }
                    } else {
                        // If transaction did not contain target transfer
                        RpcResponse {
                            jsonrpc: "2.0".to_string(),
                            result: None,
                            error: Some(serde_json::json!({
                                "code": -32603, // Standard JSON-RPC error code for internal error
                                "message": "Transaction does not contain target transfer",
                                "data": {
                                  "err": "Missing Transfer",
                                }
                            })),
                            id: rpc_request.id,
                        }
                    };

                    // Send back the JSON-RPC response
                    Ok(Response::builder()
                        .status(StatusCode::OK)
                        .header("Content-Type", "application/json")
                        .body(Body::from(serde_json::to_string(&response).unwrap())) // Unwrapping is okay here for simple struct
                        .unwrap())
                }
                Err(e) => {
                    // Handle errors during transaction processing
                    error!("Error processing transaction: {}", e);
                    let response = RpcResponse {
                        jsonrpc: "2.0".to_string(),
                        result: None,
                        error: Some(Value::String(format!(
                            "Error processing transaction: {}",
                            e
                        ))),
                        id: rpc_request.id,
                    };

                    Ok(Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .header("Content-Type", "application/json")
                        .body(Body::from(serde_json::to_string(&response).unwrap())) // Unwrapping is okay here for simple struct
                        .unwrap())
                }
            }
        } else {
            // Missing transaction data in parameters
            warn!("sendTransaction request missing transaction data parameter.");
            let response = RpcResponse {
                jsonrpc: "2.0".to_string(),
                result: None,
                error: Some(Value::String(
                    "Missing transaction data parameter for sendTransaction".to_string(),
                )),
                id: rpc_request.id,
            };

            Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&response).unwrap()))
                .unwrap())
        }
    } else {
        // Method not supported by this proxy
        warn!("Unsupported RPC method received: {}", rpc_request.method);
        let response = RpcResponse {
            jsonrpc: "2.0".to_string(),
            result: None,
            error: Some(Value::String(format!(
                "Method not supported by proxy: {}",
                rpc_request.method
            ))),
            id: rpc_request.id,
        };

        Ok(Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_string(&response).unwrap()))
            .unwrap())
    }
}

/// Decodes a Base64 transaction, checks for a specific transfer, and forwards it if found.
/// For high-tip transactions (>= 1M lamports), also forwards to Jito Block Engine.
async fn process_transaction(
    tx_data: &str,
    original_body: &hyper::body::Bytes,
    app_state: &AppState,
) -> Result<Option<String>> {
    debug!(
        "Attempting to decode base64 transaction data, length: {}",
        tx_data.len()
    );
    let tx_bytes = general_purpose::STANDARD.decode(tx_data)?;
    debug!(
        "Decoded {} bytes from base64, first 20 bytes: {:02x?}",
        tx_bytes.len(),
        &tx_bytes[..tx_bytes.len().min(20)]
    );

    // FIX: Use bincode to deserialize the transaction from its wire format.
    let transaction: VersionedTransaction = bincode::deserialize(&tx_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to deserialize transaction with bincode: {}", e))?;
    debug!("Successfully deserialized Solana transaction.");

    let target_pubkey = TARGET_PUBKEY.parse::<Pubkey>()?;

    // Check if transaction contains a transfer of TARGET_LAMPORTS to target_pubkey
    if let Some(tip_amount) = get_target_transfer_amount(&transaction, &target_pubkey) {
        info!(
            "Found target transfer to {} with {} lamports. Forwarding transaction.",
            TARGET_PUBKEY, tip_amount
        );
        
        // Always forward to regular forwarders first
        forward_transaction(original_body, &app_state.connections).await?;

        // If tip is high enough and Jito is enabled, also create and send Jito bundle
        if tip_amount >= JITO_TIP_THRESHOLD && app_state.jito_enabled {
            if let (Some(jito_client), Some(jito_keypair)) = (&app_state.jito_client, &app_state.jito_keypair) {
                info!("High tip transaction ({} lamports >= {}), forwarding to Jito Block Engine", tip_amount, JITO_TIP_THRESHOLD);
                
                match create_and_send_jito_bundle(&transaction, tip_amount, jito_client, jito_keypair).await {
                    Ok(_) => info!("Successfully sent Jito bundle"),
                    Err(e) => warn!("Failed to send Jito bundle: {}", e),
                }
            } else {
                warn!("Jito enabled but client or keypair not available");
            }
        }

        // Return the transaction signature (usually the first signature from the fee payer)
        let signature = transaction
            .signatures
            .first()
            .map(|sig| sig.to_string())
            .unwrap_or_else(|| {
                warn!("Transaction has no signatures, returning empty string.");
                "".to_string()
            });

        Ok(Some(signature))
    } else {
        info!(
            "No target transfer found to {} with at least {} lamports. Not forwarding.",
            TARGET_PUBKEY, TARGET_LAMPORTS
        );
        Ok(None)
    }
}

/// Checks if a transaction contains a System Program transfer instruction
/// to the `target_pubkey` with at least `TARGET_LAMPORTS`.
/// Returns the tip amount if found, None otherwise.
fn get_target_transfer_amount(transaction: &VersionedTransaction, target_pubkey: &Pubkey) -> Option<u64> {
    // Iterate through all instructions in the transaction's message
    for instruction in transaction.message.instructions() {
        // Resolve the program ID for the current instruction
        if let Some(program_id) = transaction
            .message
            .static_account_keys()
            .get(instruction.program_id_index as usize)
        {
            // Check if the instruction is for the System Program
            if *program_id == solana_sdk::system_program::id() {
                debug!(
                    "Found System Program instruction. Program ID: {}",
                    program_id
                );

                // FIX: Use bincode::deserialize for the instruction data, just like for the transaction.
                if let Ok(
                    system_instruction @ system_instruction::SystemInstruction::Transfer { .. },
                ) = bincode::deserialize(&instruction.data)
                {
                    debug!("Parsed System Instruction: {:?}", system_instruction);
                    // Check if it's a Transfer instruction
                    if let system_instruction::SystemInstruction::Transfer { lamports } =
                        system_instruction
                    {
                        debug!(
                            "Detected SystemProgram Transfer instruction with {} lamports.",
                            lamports
                        );
                        // Check if the transfer amount meets the target
                        if lamports >= TARGET_LAMPORTS {
                            // Ensure the instruction has enough accounts for a transfer (from, to)
                            if let Some(to_idx) = instruction.accounts.get(1) {
                                // Destination account index is the second account
                                // Resolve the destination public key from the transaction's account keys
                                if let Some(to_pubkey) = transaction
                                    .message
                                    .static_account_keys()
                                    .get(*to_idx as usize)
                                {
                                    debug!("Transfer destination: {}", to_pubkey);
                                    // Check if the destination public key matches the target
                                    if to_pubkey == target_pubkey {
                                        info!("Match found: SystemProgram Transfer to {} with {} lamports (target: >= {})",
                                              to_pubkey, lamports, TARGET_LAMPORTS);
                                        return Some(lamports); // Return the tip amount
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    None // No target transfer found in any instruction
}

/// Forwards the original, unmodified request body to all configured RPC addresses.
async fn forward_transaction(
    original_body: &hyper::body::Bytes, // <-- Parameter changed
    connections: &ConnectionPool,
) -> Result<()> {
    let connections_guard = connections.read().await;

    // Iterate through all connections and send the original body
    for (addr, client) in connections_guard.iter() {
        match client
            .post(addr)
            .header("Content-Type", "application/json")
            .body(original_body.clone()) // <-- Use the original body directly
            .send()
            .await
        {
            Ok(response) => {
                info!(
                    "Forwarded transaction to {}, status: {}",
                    addr,
                    response.status()
                );
                // The debug log for the response body is still useful here
                // if let Ok(text) = response.text().await {
                //     debug!("Response from {}: {}", addr, text);
                // }
            }
            Err(e) => {
                warn!("Failed to forward transaction to {}: {}", addr, e);
            }
        }
    }

    Ok(())
}

/// Loads a keypair from a file containing a base58-encoded private key
fn load_keypair_from_file(file_path: &str) -> Result<Keypair> {
    let key_data = std::fs::read_to_string(file_path)
        .map_err(|e| anyhow::anyhow!("Failed to read keypair file '{}': {}", file_path, e))?;
    
    let key_bytes = bs58::decode(key_data.trim())
        .into_vec()
        .map_err(|e| anyhow::anyhow!("Failed to decode base58 keypair: {}", e))?;
    
    Keypair::from_bytes(&key_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to create keypair from bytes: {}", e))
}

/// Creates and sends a Jito bundle with the transaction and a tip transaction
async fn create_and_send_jito_bundle(
    original_tx: &VersionedTransaction,
    original_tip: u64,
    jito_client: &Arc<JitoRpcClient>,
    jito_keypair: &Arc<Keypair>,
) -> Result<String> {
    // Calculate the adjusted tip amount (original tip - 100k lamports)
    let bundle_tip = if original_tip > JITO_TIP_REDUCTION {
        original_tip - JITO_TIP_REDUCTION
    } else {
        1000 // Minimum tip of 1000 lamports
    };

    info!("Creating Jito bundle with tip: {} lamports (original: {})", bundle_tip, original_tip);

    // Get tip accounts from Jito
    let tip_accounts = jito_client.get_tip_accounts().await
        .map_err(|e| anyhow::anyhow!("Failed to get tip accounts: {}", e))?;
    
    if tip_accounts.is_empty() {
        return Err(anyhow::anyhow!("No tip accounts available"));
    }

    // Use the first tip account
    let tip_account = tip_accounts[0];
    info!("Using tip account: {}", tip_account);

    // Create a tip transaction
    let tip_instruction = system_instruction::transfer(
        &jito_keypair.pubkey(),
        &tip_account,
        bundle_tip,
    );

    // For now, we'll create a simple transaction with just the tip
    // In a real implementation, you might want to get recent blockhash from RPC
    let recent_blockhash = solana_sdk::hash::Hash::default(); // This should be replaced with actual recent blockhash
    
    let tip_transaction = Transaction::new_signed_with_payer(
        &[tip_instruction],
        Some(&jito_keypair.pubkey()),
        &[jito_keypair.as_ref()],
        recent_blockhash,
    );

    // Convert the tip transaction to VersionedTransaction
    let tip_versioned = VersionedTransaction::from(tip_transaction);

    // Create bundle with original transaction and tip transaction
    let bundle_txs = vec![original_tx.clone(), tip_versioned];
    
    // Send the bundle
    match jito_client.send_bundle(&bundle_txs).await {
        Ok(bundle_id) => {
            info!("Successfully sent bundle to Jito Block Engine: {}", bundle_id);
            Ok(bundle_id)
        }
        Err(e) => {
            error!("Failed to send bundle to Jito: {}", e);
            Err(anyhow::anyhow!("Failed to send Jito bundle: {}", e))
        }
    }
}

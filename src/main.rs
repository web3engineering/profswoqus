use anyhow::Result;
use base64::{engine::general_purpose, Engine as _};
use bincode;
use clap::Parser;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::system_instruction;
use solana_sdk::transaction::{TransactionError, VersionedTransaction}; // Added TransactionError for deserialize result
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, value_delimiter = ',')]
    addresses: Vec<String>,

    #[arg(short, long, default_value = "8080")]
    port: u16,
}

#[derive(Deserialize, Serialize)]
struct RpcRequest {
    method: String,
    params: Vec<Value>,
    id: Value,
}

#[derive(Serialize)]
struct RpcResponse {
    result: Option<Value>,
    error: Option<Value>,
    id: Value,
}

type ConnectionPool = Arc<RwLock<HashMap<String, reqwest::Client>>>;

const TARGET_PUBKEY: &str = "GxkB4oYYLsoeAoxAdXjDEBSrP7JGCy3re7mqozFYyiYW";
const TARGET_LAMPORTS: u64 = 100_000;

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

    // Define the server address to bind to
    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));

    // Create a service for handling incoming HTTP requests
    let make_svc = make_service_fn(move |_conn| {
        let connections = connections.clone(); // Clone Arc for each service instance
        async move {
            // For each request, create a service_fn closure
            Ok::<_, Infallible>(service_fn(move |req| {
                // Clone Arc for each request handler
                handle_request(req, connections.clone())
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
    connections: ConnectionPool,
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
            match process_transaction(tx_data, &connections).await {
                Ok(signature_opt) => {
                    let response = if let Some(signature) = signature_opt {
                        // If transaction contained target transfer and was forwarded
                        RpcResponse {
                            result: Some(Value::String(signature)),
                            error: None,
                            id: rpc_request.id,
                        }
                    } else {
                        // If transaction did not contain target transfer
                        RpcResponse {
                            result: None,
                            error: Some(serde_json::json!({
                                "code": -32603, // Standard JSON-RPC error code for internal error
                                "message": "Transaction does not contain target transfer"
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
/// Decodes a Base64 transaction, checks for a specific transfer, and forwards it if found.
async fn process_transaction(
    tx_data: &str,
    connections: &ConnectionPool,
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
    if has_target_transfer(&transaction, &target_pubkey) {
        info!(
            "Found target transfer to {} with at least {} lamports. Forwarding transaction.",
            TARGET_PUBKEY, TARGET_LAMPORTS
        );
        // NOTE: The original code forwarded the transaction but didn't wait for the RPC response
        // to confirm the signature. The signature is derived locally from the tx data.
        forward_transaction(tx_data, connections).await?;

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
/// Checks if a transaction contains a System Program transfer instruction
/// to the `target_pubkey` with at least `TARGET_LAMPORTS`.
fn has_target_transfer(transaction: &VersionedTransaction, target_pubkey: &Pubkey) -> bool {
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
                                        return true; // Found the target transfer
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    false // No target transfer found in any instruction
}

/// Forwards the Base64 encoded transaction to all configured RPC addresses.
async fn forward_transaction(tx_data: &str, connections: &ConnectionPool) -> Result<()> {
    let connections_guard = connections.read().await; // Acquire a read lock on the connection pool

    // Construct the RPC request body for forwarding
    let rpc_request = RpcRequest {
        method: "sendTransaction".to_string(),
        params: vec![Value::String(tx_data.to_string())], // Send the original Base64 string
        id: Value::Number(serde_json::Number::from(1)),   // Use a dummy ID or pass through original
    };

    let request_body = serde_json::to_string(&rpc_request)?;

    // Iterate through all connections and send the transaction
    for (addr, client) in connections_guard.iter() {
        match client
            .post(addr)
            .header("Content-Type", "application/json")
            .body(request_body.clone()) // Clone the body for each request
            .send()
            .await
        {
            Ok(response) => {
                info!(
                    "Forwarded transaction to {}, status: {}",
                    addr,
                    response.status()
                );
                // Optionally read and log the response body from the RPC
                // let response_body = response.text().await.unwrap_or_else(|_| "Failed to read response body".to_string());
                // debug!("Response from {}: {}", addr, response_body);
            }
            Err(e) => {
                warn!("Failed to forward transaction to {}: {}", addr, e);
            }
        }
    }

    Ok(())
}

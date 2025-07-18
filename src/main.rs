use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::Transaction;
use solana_sdk::system_instruction;
use clap::Parser;
use anyhow::Result;
use tracing::{info, warn, error};
use base64::{Engine as _, engine::general_purpose};

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
    tracing_subscriber::fmt::init();
    
    let args = Args::parse();
    
    if args.addresses.is_empty() {
        error!("No addresses provided");
        std::process::exit(1);
    }
    
    let connections = Arc::new(RwLock::new(HashMap::new()));
    
    // Initialize connections
    for addr in &args.addresses {
        let client = reqwest::Client::new();
        connections.write().await.insert(addr.clone(), client);
        info!("Added connection to: {}", addr);
    }
    
    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    
    let make_svc = make_service_fn(move |_conn| {
        let connections = connections.clone();
        async move {
            Ok::<_, Infallible>(service_fn(move |req| {
                handle_request(req, connections.clone())
            }))
        }
    });
    
    let server = Server::bind(&addr).serve(make_svc);
    
    info!("Server running on {}", addr);
    
    if let Err(e) = server.await {
        error!("Server error: {}", e);
    }
    
    Ok(())
}

async fn handle_request(
    req: Request<Body>,
    connections: ConnectionPool,
) -> Result<Response<Body>, Infallible> {
    if req.method() != Method::POST {
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Body::from("Method not allowed"))
            .unwrap());
    }
    
    let body_bytes = match hyper::body::to_bytes(req.into_body()).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("Failed to read request body"))
                .unwrap());
        }
    };
    
    let rpc_request: RpcRequest = match serde_json::from_slice(&body_bytes) {
        Ok(req) => req,
        Err(_) => {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from("Invalid JSON"))
                .unwrap());
        }
    };
    
    if rpc_request.method == "sendTransaction" {
        if let Some(tx_data) = rpc_request.params.get(0).and_then(|p| p.as_str()) {
            match process_transaction(tx_data, &connections).await {
                Ok(signature_opt) => {
                    let response = if let Some(signature) = signature_opt {
                        RpcResponse {
                            result: Some(Value::String(signature)),
                            error: None,
                            id: rpc_request.id,
                        }
                    } else {
                        RpcResponse {
                            result: None,
                            error: Some(serde_json::json!({
                                "code": -32603,
                                "message": "Transaction does not contain target transfer"
                            })),
                            id: rpc_request.id,
                        }
                    };
                    
                    Ok(Response::builder()
                        .status(StatusCode::OK)
                        .header("Content-Type", "application/json")
                        .body(Body::from(serde_json::to_string(&response).unwrap()))
                        .unwrap())
                }
                Err(e) => {
                    let response = RpcResponse {
                        result: None,
                        error: Some(Value::String(format!("Error: {}", e))),
                        id: rpc_request.id,
                    };
                    
                    Ok(Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .header("Content-Type", "application/json")
                        .body(Body::from(serde_json::to_string(&response).unwrap()))
                        .unwrap())
                }
            }
        } else {
            let response = RpcResponse {
                result: None,
                error: Some(Value::String("Missing transaction data".to_string())),
                id: rpc_request.id,
            };
            
            Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&response).unwrap()))
                .unwrap())
        }
    } else {
        let response = RpcResponse {
            result: None,
            error: Some(Value::String("Method not supported".to_string())),
            id: rpc_request.id,
        };
        
        Ok(Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_string(&response).unwrap()))
            .unwrap())
    }
}

async fn process_transaction(
    tx_data: &str,
    connections: &ConnectionPool,
) -> Result<Option<String>> {
    tracing::info!("Decoding base64 transaction data, length: {}", tx_data.len());
    let tx_bytes = general_purpose::STANDARD.decode(tx_data)?;
    let transaction: Transaction = bincode::deserialize(&tx_bytes)?;
    
    let target_pubkey = TARGET_PUBKEY.parse::<Pubkey>()?;
    
    // Check if transaction contains a transfer of TARGET_LAMPORTS to target_pubkey
    if has_target_transfer(&transaction, &target_pubkey) {
        info!("Found target transfer, forwarding transaction");
        forward_transaction(tx_data, connections).await?;
        
        // Return the transaction signature
        let signature = transaction.signatures.first()
            .map(|sig| sig.to_string())
            .unwrap_or_else(|| "".to_string());
        
        Ok(Some(signature))
    } else {
        info!("No target transfer found, not forwarding");
        Ok(None)
    }
}

fn has_target_transfer(transaction: &Transaction, target_pubkey: &Pubkey) -> bool {
    for instruction in &transaction.message.instructions {
        if let Some(program_id) = transaction.message.account_keys.get(instruction.program_id_index as usize) {
            if *program_id == solana_sdk::system_program::id() {
                if let Ok(system_instruction) = bincode::deserialize::<system_instruction::SystemInstruction>(&instruction.data) {
                    if let system_instruction::SystemInstruction::Transfer { lamports } = system_instruction {
                        if lamports >= TARGET_LAMPORTS {
                            if let (Some(from_idx), Some(to_idx)) = (
                                instruction.accounts.get(0),
                                instruction.accounts.get(1),
                            ) {
                                if let Some(to_pubkey) = transaction.message.account_keys.get(*to_idx as usize) {
                                    if to_pubkey == target_pubkey {
                                        return true;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    false
}

async fn forward_transaction(
    tx_data: &str,
    connections: &ConnectionPool,
) -> Result<()> {
    let connections_guard = connections.read().await;
    
    let rpc_request = RpcRequest {
        method: "sendTransaction".to_string(),
        params: vec![Value::String(tx_data.to_string())],
        id: Value::Number(serde_json::Number::from(1)),
    };
    
    let request_body = serde_json::to_string(&rpc_request)?;
    
    for (addr, client) in connections_guard.iter() {
        match client
            .post(addr)
            .header("Content-Type", "application/json")
            .body(request_body.clone())
            .send()
            .await
        {
            Ok(response) => {
                info!("Forwarded transaction to {}, status: {}", addr, response.status());
            }
            Err(e) => {
                warn!("Failed to forward transaction to {}: {}", addr, e);
            }
        }
    }
    
    Ok(())
}

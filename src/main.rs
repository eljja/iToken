use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use futures::StreamExt;
use clap::Parser;
use tracing::{info, error, warn, debug};
use axum::{
    routing::{get, post},
    Router, Json, response::IntoResponse,
    extract::ConnectInfo,
    http::StatusCode,
};
use std::net::SocketAddr;
use std::collections::HashMap;
use std::time::Instant;
use axum::response::sse::{Event as SseEvent, Sse};
use tower_http::cors::CorsLayer;
use serde_json::json;

use itoken_core::types::{InferenceRequest, InferenceReceipt, NANO_PER_ITOKEN, format_itokens};
use itoken_core::crypto::{
    load_or_generate_keypair, pubkey_to_hex,
    sign_receipt_as_node, sign_receipt_as_client, sha256_hash,
};
use itoken_inference::detector::PortDetector;
use itoken_inference::proxy::InferenceProxy;
use itoken_harness::reputation::ReputationDb;
use itoken_harness::routing::HarnessRouter;
use itoken_harness::network_stats::NetworkStats;
use itoken_ledger::LocalLedger;
use itoken_network::{P2PNode, P2PEvent, P2PInferenceRequest, P2PRequest, P2PResponse};

// ─── CLI Arguments ─────────────────────────────────────────────────────────────

#[derive(Parser, Debug, Clone)]
#[command(name = "itoken-node", version = "0.2.0", about = "iToken Network — Decentralized AI Inference Node")]
struct Args {
    /// Data directory for keys, ledger, and reputation state
    #[arg(long, default_value = ".itoken")]
    data_dir: PathBuf,

    /// Custom LLM backend URL (overrides auto-detection)
    #[arg(long)]
    backend: Option<String>,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Run the demo flow (single mock client→node→payment cycle) and exit
    #[arg(long)]
    demo: bool,

    /// Initial network median TPS for reward calculation (will be dynamic in production)
    #[arg(long, default_value = "25.0")]
    median_tps: f64,

    /// P2P listen address (e.g. /ip4/0.0.0.0/tcp/4001)
    #[arg(long)]
    listen_p2p: Option<String>,

    /// Bootstrap peer addresses (can be specified multiple times)
    #[arg(long)]
    bootstrap: Vec<String>,

    /// Local HTTP API port for client applications (default: 8420)
    #[arg(long, default_value = "8420")]
    api_port: u16,

    /// Initial client balance seed amount in iTokens
    #[arg(long, default_value = "100.0")]
    seed_amount: f64,
}

// ─── Rate Limiter ──────────────────────────────────────────────────────────────

struct RateLimiter {
    visits: HashMap<std::net::IpAddr, Vec<Instant>>,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            visits: HashMap::new(),
        }
    }

    fn check_limit(&mut self, ip: std::net::IpAddr, limit: usize, window: Duration) -> bool {
        let now = Instant::now();
        let times = self.visits.entry(ip).or_default();
        times.retain(|&t| now.duration_since(t) < window);
        if times.len() < limit {
            times.push(now);
            true
        } else {
            false
        }
    }
}

// ─── HTTP API Shared State ──────────────────────────────────────────────────────

struct AppState {
    ledger: Arc<LocalLedger>,
    reputation_db: Arc<ReputationDb>,
    network_stats: Arc<NetworkStats>,
    p2p_node: Arc<P2PNode>,
    detected_engines: Arc<parking_lot::Mutex<Vec<itoken_inference::detector::DetectedEngine>>>,
    rate_limiter: Arc<parking_lot::Mutex<RateLimiter>>,
    node_priv: ed25519_dalek::SigningKey,
    client_priv: ed25519_dalek::SigningKey,
    client_hex: String,
    node_hex: String,
    _args: Args,
}

// ─── Main Entry Point ──────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Initialize structured logging
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&args.log_level));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .init();

    println!("==================================================");
    println!("    iToken Network v0.2.0 — Production Hardened   ");
    println!("==================================================");

    // Ensure data directory exists
    std::fs::create_dir_all(&args.data_dir)
        .map_err(|e| format!("Failed to create data directory: {}", e))?;

    // Single-instance lock file check
    let lock_file_path = args.data_dir.join("itoken.lock");
    
    #[cfg(target_os = "windows")]
    let mut open_options = std::fs::OpenOptions::new();
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::OpenOptionsExt;
        open_options.share_mode(0); // Exclusive access: no other processes can read/write/delete
    }
    #[cfg(not(target_os = "windows"))]
    let mut open_options = std::fs::OpenOptions::new();

    let _lock_file = open_options
        .write(true)
        .create(true)
        .open(&lock_file_path)
        .map_err(|e| format!(
            "Failed to lock data directory '{}': is another instance of iToken running? Error: {}",
            args.data_dir.display(),
            e
        ))?;

    // Initialize persistent key storage
    let node_key_path = args.data_dir.join("node_key.dat");
    let client_key_path = args.data_dir.join("client_key.dat");

    let (node_priv, node_pub) = load_or_generate_keypair(&node_key_path)?;
    let node_hex = pubkey_to_hex(&node_pub);

    let (client_priv, client_pub) = load_or_generate_keypair(&client_key_path)?;
    let client_hex = pubkey_to_hex(&client_pub);

    info!(node_pubkey = %node_hex, "Node identity loaded");
    info!(client_pubkey = %client_hex, "Client identity loaded");

    // Initialize ledger, reputation, and network stats
    let ledger_path = args.data_dir.join("ledger.json");
    let reputation_path = args.data_dir.join("reputation.json");

    let ledger = Arc::new(LocalLedger::new(ledger_path.to_str().unwrap_or("ledger.json"))?);
    let reputation_db = Arc::new(ReputationDb::new(reputation_path.to_str().unwrap_or("reputation.json"))?);
    let network_stats = Arc::new(NetworkStats::new());

    // Register accounts and seed initial balances (configurable seed amount)
    let seed_nano = (args.seed_amount * NANO_PER_ITOKEN as f64) as u64;
    ledger.register_account(&client_hex, seed_nano);
    ledger.register_account(&node_hex, 0);

    info!(
        client_balance = %format_itokens(ledger.get_balance(&client_hex)),
        node_balance = %format_itokens(ledger.get_balance(&node_hex)),
        "Ledger balances loaded"
    );

    if args.demo {
        // Run single-shot client -> node local demo loop
        run_demo_loop(args, ledger, reputation_db, node_priv, client_priv, node_hex, client_hex).await?;
        return Ok(());
    }

    // ─── Daemon Mode ────────────────────────────────────────────────────────────
    info!("Starting persistent P2P Daemon mode...");

    // 1. Boot P2P Node
    let p2p_node = Arc::new(P2PNode::new()?);
    let p2p_addr_str = args.listen_p2p.clone().unwrap_or_else(|| "/ip4/0.0.0.0/tcp/4001".to_string());
    
    match p2p_addr_str.parse() {
        Ok(addr) => {
            if let Err(e) = p2p_node.start_listening(addr).await {
                warn!(error = %e, "Port 4001 busy, falling back to random P2P port");
                p2p_node.start_listening("/ip4/0.0.0.0/tcp/0".parse()?).await?;
            }
        }
        Err(e) => {
            error!(error = %e, "Invalid listen address configuration, exiting");
            return Err(e.into());
        }
    }

    // Dial bootstrap peers if specified
    for peer_addr in &args.bootstrap {
        if let Ok(addr) = peer_addr.parse() {
            info!(addr = %peer_addr, "Dialing bootstrap peer");
            let _ = p2p_node.dial(addr).await;
        }
    }

    // 2. Discover local LLM models and advertise them
    let detector = PortDetector::new();
    let initial_detected = if let Some(ref custom_url) = args.backend {
        detector.probe_custom(custom_url).await.map(|e| vec![e]).unwrap_or_default()
    } else {
        detector.scan_all().await
    };

    let detected_engines = Arc::new(parking_lot::Mutex::new(initial_detected.clone()));
    let rate_limiter = Arc::new(parking_lot::Mutex::new(RateLimiter::new()));

    let mut hosted_models = Vec::new();
    for engine in &initial_detected {
        for model in &engine.active_models {
            hosted_models.push(model.name.clone());
            info!(model = %model.name, "Advertising local model on DHT");
            let _ = p2p_node.advertise_model(&model.name).await;
        }
    }

    if hosted_models.is_empty() {
        hosted_models.push("mock-llama3-8b".to_string());
        info!("No local LLM engines detected — advertising mock model 'mock-llama3-8b'");
        let _ = p2p_node.advertise_model("mock-llama3-8b").await;
    }

    // 3. Initialize AppState
    let state = Arc::new(AppState {
        ledger: ledger.clone(),
        reputation_db: reputation_db.clone(),
        network_stats: network_stats.clone(),
        p2p_node: p2p_node.clone(),
        detected_engines: detected_engines.clone(),
        rate_limiter,
        node_priv,
        client_priv,
        client_hex,
        node_hex,
        _args: args.clone(),
    });

    // 4. Spawn P2P event listener task
    let p2p_state = state.clone();
    let p2p_node_loop = p2p_node.clone();
    tokio::spawn(async move {
        let loop_node = p2p_node_loop;
        info!("P2P event loop started");
        while let Some(event) = loop_node.recv_event().await {
            match event {
                P2PEvent::Request { peer_id, request, channel } => {
                    match request {
                        P2PRequest::Inference(p2p_req) => {
                            let req = p2p_req.req;
                            let client_pubkey = p2p_req.client_pubkey;
                            info!(request_id = %req.request_id, model = %req.model, client = %client_pubkey, "Processing inbound P2P query");

                            // Check validation
                            if let Err(e) = req.validate() {
                                let _ = loop_node.send_response(channel, P2PResponse::Error(format!("Validation failed: {}", e))).await;
                                continue;
                            }

                            // Process query locally (read from cache to avoid port scanning latency)
                            let (backend_url, tqw_nano) = {
                                let detected = p2p_state.detected_engines.lock();
                                if let Some(engine) = detected.first() {
                                    let tqw = engine.active_models.first().map(|m| m.tqw_nano).unwrap_or(NANO_PER_ITOKEN / 100);
                                    (engine.url.clone(), tqw)
                                } else {
                                    ("http://localhost:9999".to_string(), NANO_PER_ITOKEN / 100)
                                }
                            };

                            let start = std::time::Instant::now();
                            let actual_tokens;
                            let mut text = String::new();

                            if backend_url == "http://localhost:9999" {
                                text = "This response is served over the decentralised iToken P2P network!".to_string();
                                actual_tokens = text.split_whitespace().count();
                                tokio::time::sleep(Duration::from_millis(30)).await;
                            } else {
                                let proxy = InferenceProxy::new(backend_url);
                                match proxy.proxy_query(req.clone()).await {
                                    Ok((mut stream, get_metrics)) => {
                                        while let Some(chunk) = stream.next().await {
                                            if let Ok(token) = chunk {
                                                text.push_str(&token);
                                            }
                                        }
                                        let (tokens, _) = get_metrics();
                                        actual_tokens = tokens;
                                    }
                                    Err(e) => {
                                        error!(error = %e, "Local inference proxy failed for P2P query");
                                        let _ = loop_node.send_response(channel, P2PResponse::Error(e)).await;
                                        continue;
                                    }
                                }
                            }

                            let elapsed = start.elapsed().as_secs_f64();
                            let tps = if elapsed > 0.0 { actual_tokens as f64 / elapsed } else { 25.0 };
                            
                            // Record local inference performance
                            p2p_state.network_stats.record_local_inference(tps);

                            let median_tps = p2p_state.network_stats.get_median_tps();
                            let query_hash = sha256_hash(&req.prompt);
                            let mut receipt = InferenceReceipt {
                                receipt_id: uuid::Uuid::new_v4().to_string(),
                                client_pubkey,
                                node_pubkey: p2p_state.node_hex.clone(),
                                query_hash,
                                tokens_generated: actual_tokens,
                                tps,
                                network_median_tps: median_tps,
                                tqw_nano,
                                amount_nano: 0,
                                timestamp: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
                                node_signature: None,
                                client_signature: None,
                            };

                            receipt.amount_nano = receipt.compute_amount();
                            sign_receipt_as_node(&p2p_state.node_priv, &mut receipt);

                            let response = P2PResponse::InferenceSuccess { text, receipt };
                            if let Err(e) = loop_node.send_response(channel, response).await {
                                error!(error = %e, "Failed to send P2P response back over channel");
                            }
                        }
                        P2PRequest::SyncLedger => {
                            info!(peer = %peer_id, "Received ledger synchronization request");
                            let (balances, claimed_receipts) = p2p_state.ledger.export_state();
                            let response = P2PResponse::LedgerSync { balances, claimed_receipts };
                            if let Err(e) = loop_node.send_response(channel, response).await {
                                error!(error = %e, "Failed to send ledger sync response");
                            }
                        }
                    }
                }
                P2PEvent::LedgerReceiptReceived { receipt } => {
                    info!(
                        receipt_id = %receipt.receipt_id,
                        amount = %format_itokens(receipt.amount_nano),
                        client = %receipt.client_pubkey,
                        node = %receipt.node_pubkey,
                        "Received double-signed receipt from Gossipsub; claiming locally"
                    );
                    if let Err(e) = p2p_state.ledger.claim_gossip_receipt(&receipt) {
                        warn!(receipt_id = %receipt.receipt_id, error = %e, "Sync receipt claim skipped/failed");
                    }
                }
                P2PEvent::HealthReceived { peer_id, tps_avg, models } => {
                    debug!(peer = %peer_id, tps_avg = tps_avg, models = ?models, "Gossipsub health heartbeat received");
                    p2p_state.network_stats.feed_heartbeat(&peer_id, tps_avg);
                }
            }
        }
    });

    // 5. Spawn health broadcast loop
    let health_state = state.clone();
    let health_node = p2p_node.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;

            // Prune claimed receipts older than 7 days
            health_state.ledger.prune_old_receipts(7 * 24 * 3600);
            
            // Build health broadcast payload (broadcast actual local measured average TPS)
            let payload = json!({
                "peer_id": health_node.peer_id().to_string(),
                "models": hosted_models,
                "tps_avg": health_state.network_stats.get_local_avg_tps(),
                "timestamp": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
            });

            if let Ok(bytes) = serde_json::to_vec(&payload) {
                let _ = health_node.publish_health(bytes).await;
            }

            let median_tps_str = format!("{:.2}", health_state.network_stats.get_median_tps());
            info!(
                median_tps = %median_tps_str,
                "Live network status updated"
            );
        }
    });

    // 6. Spawn ledger boot synchronization task with retry logic
    let sync_state = state.clone();
    let sync_node = p2p_node.clone();
    tokio::spawn(async move {
        info!("Ledger cold-start synchronization task initialized");
        let mut attempts = 0;
        let max_attempts = 5;
        let retry_interval = Duration::from_secs(2);

        loop {
            tokio::time::sleep(retry_interval).await;
            attempts += 1;

            if sync_node.is_closed() {
                info!("P2P node shutdown detected, stopping ledger sync task");
                break;
            }

            let peers = sync_node.get_connected_peers().await;
            if let Some(&peer_id) = peers.first() {
                info!(peer = %peer_id, attempt = attempts, "Bootstrap peer detected, initiating ledger synchronization");
                match sync_node.send_ledger_sync(peer_id).await {
                    Ok(P2PResponse::LedgerSync { balances, claimed_receipts }) => {
                        sync_state.ledger.import_state(balances, claimed_receipts);
                        info!("Ledger synchronized successfully on boot");
                        break;
                    }
                    Ok(other) => {
                        warn!(response = ?other, "Unexpected response during ledger sync, retrying");
                    }
                    Err(e) => {
                        warn!(error = %e, "Ledger sync request failed, retrying");
                    }
                }
            } else {
                debug!(attempt = attempts, "No connected bootstrap peers found yet");
            }

            if attempts >= max_attempts {
                info!("No bootstrap peers found after {} attempts; starting with local ledger state", max_attempts);
                break;
            }
        }
    });

    // 7. Spawn engine periodic detection task
    let scan_state = state.clone();
    let scan_args = args.clone();
    tokio::spawn(async move {
        let detector = PortDetector::new();
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        // Skip the first tick since we already scanned once on startup
        interval.tick().await; 
        loop {
            interval.tick().await;
            if scan_state.p2p_node.is_closed() {
                break;
            }
            debug!("Starting periodic local LLM engine scan...");
            let detected = if let Some(ref custom_url) = scan_args.backend {
                detector.probe_custom(custom_url).await.map(|e| vec![e]).unwrap_or_default()
            } else {
                detector.scan_all().await
            };
            
            {
                let mut engines = scan_state.detected_engines.lock();
                if *engines != detected {
                    info!(
                        old_count = engines.len(),
                        new_count = detected.len(),
                        "Local LLM engine status changed"
                    );
                    *engines = detected;
                }
            }
        }
    });

    // 8. Spawn HTTP Server Gateway (Axum Router)
    let app = Router::new()
        .route("/v1/chat/completions", post(handle_chat_completions))
        .route("/v1/balance", get(handle_balance))
        .route("/v1/reputation", get(handle_reputation))
        .route("/v1/stats", get(handle_stats))
        .layer(axum::middleware::from_fn_with_state(state.clone(), rate_limit_middleware))
        .layer(CorsLayer::permissive())
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", args.api_port)).await?;
    info!(port = args.api_port, "OpenAI-compatible HTTP Gateway listening");
    
    // Setup clean graceful shutdown hook
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            info!("Shutting down HTTP Gateway and P2P Node cleanly...");
            p2p_node.shutdown().await;
        })
        .await?;

    Ok(())
}

// ─── Axum HTTP Server Handlers ──────────────────────────────────────────────────

async fn handle_chat_completions(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    let prompt = payload.get("messages")
        .and_then(|m| m.as_array())
        .and_then(|m| m.last())
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();

    let model = payload.get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("mock-llama3-8b")
        .to_string();

    let max_tokens = payload.get("max_tokens")
        .and_then(|t| t.as_u64())
        .map(|t| t as usize);

    let temperature = payload.get("temperature")
        .and_then(|t| t.as_f64())
        .map(|t| t as f32)
        .unwrap_or(0.0);

    let stream = payload.get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);

    let req = InferenceRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        prompt,
        model: model.clone(),
        max_tokens,
        temperature,
    };

    if let Err(e) = req.validate() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("Validation failed: {}", e) }))
        ).into_response();
    }

    // Find candidates over DHT
    let candidates = state.p2p_node.search_model(&req.model).await;
    let candidate_strings: Vec<String> = candidates.iter().map(|p| p.to_string()).collect();

    let router = HarnessRouter::new(state.reputation_db.clone());
    let mut use_p2p = false;
    let mut selected_peer = None;

    if !candidate_strings.is_empty() {
        if let Ok((primary, _backups)) = router.resolve_routing(&req.model, candidate_strings) {
            if let Some(peer) = candidates.iter().find(|p| p.to_string() == primary) {
                selected_peer = Some(*peer);
                use_p2p = true;
            }
        }
    }

    let text;
    let receipt;

    if let (true, Some(peer_id)) = (use_p2p, selected_peer) {
        info!(peer = %peer_id, model = %req.model, "Routing request to remote peer over P2P");

        let p2p_req = P2PInferenceRequest {
            req: req.clone(),
            client_pubkey: state.client_hex.clone(),
        };

        match state.p2p_node.send_inference(peer_id, p2p_req).await {
            Ok(P2PResponse::InferenceSuccess { text: p2p_text, receipt: mut p2p_receipt }) => {
                text = p2p_text;

                // Client must verify the node's pricing is within tolerance before signing.
                let local_median_tps = state.network_stats.get_median_tps();
                let lower_bound = local_median_tps * 0.8;
                let upper_bound = local_median_tps * 1.2;

                if p2p_receipt.network_median_tps < lower_bound || p2p_receipt.network_median_tps > upper_bound {
                    error!(
                        receipt_id = %p2p_receipt.receipt_id,
                        receipt_tps = p2p_receipt.network_median_tps,
                        local_median_tps = local_median_tps,
                        "Receipt network_median_tps is out of client's local tolerance (±20%)"
                    );
                    state.reputation_db.record_failure(&peer_id.to_string());
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        Json(json!({ "error": "Receipt pricing/TPS is outside local client tolerance" }))
                    ).into_response();
                }

                // Verify that the receipt's claimed amount_nano is mathematically correct.
                let expected_amount = p2p_receipt.compute_amount();
                if p2p_receipt.amount_nano != expected_amount {
                    error!(
                        receipt_id = %p2p_receipt.receipt_id,
                        claimed = p2p_receipt.amount_nano,
                        expected = expected_amount,
                        "Receipt claimed amount is mathematically incorrect"
                    );
                    state.reputation_db.record_failure(&peer_id.to_string());
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        Json(json!({ "error": "Receipt claimed amount is mathematically incorrect" }))
                    ).into_response();
                }

                // Client signs the receipt to authorize payout
                sign_receipt_as_client(&state.client_priv, &mut p2p_receipt);

                match state.ledger.claim_receipt(&p2p_receipt) {
                    Ok(_) => {
                        state.reputation_db.record_success(&peer_id.to_string(), 1.0, p2p_receipt.tokens_generated as u32);
                        receipt = Some(p2p_receipt.clone());

                        // Broadcast claimed receipt to P2P network
                        let p2p_node = state.p2p_node.clone();
                        let receipt_clone = p2p_receipt.clone();
                        tokio::spawn(async move {
                            let _ = p2p_node.publish_receipt(receipt_clone).await;
                        });
                    }
                    Err(e) => {
                        error!(error = %e, "Local ledger failed to claim P2P receipt");
                        state.reputation_db.record_failure(&peer_id.to_string());
                        return (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": format!("Ledger verification error: {}", e) }))
                        ).into_response();
                    }
                }
            }
            Ok(P2PResponse::LedgerSync { .. }) => {
                state.reputation_db.record_failure(&peer_id.to_string());
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "Unexpected LedgerSync response" }))
                ).into_response();
            }
            Ok(P2PResponse::Error(err)) => {
                state.reputation_db.record_failure(&peer_id.to_string());
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("Remote peer error: {}", err) }))
                ).into_response();
            }
            Err(e) => {
                state.reputation_db.record_failure(&peer_id.to_string());
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("P2P routing error: {}", e) }))
                ).into_response();
            }
        }
    } else {
        // Local execution fallback
        let (backend_url, tqw_nano) = {
            let detected = state.detected_engines.lock();
            if let Some(engine) = detected.first() {
                let tqw = engine.active_models.first().map(|m| m.tqw_nano).unwrap_or(NANO_PER_ITOKEN / 100);
                (engine.url.clone(), tqw)
            } else {
                ("http://localhost:9999".to_string(), NANO_PER_ITOKEN / 100)
            }
        };

        let start = std::time::Instant::now();
        let actual_tokens;
        let mut temp_text = String::new();

        if backend_url == "http://localhost:9999" {
            temp_text = "Rayleigh scattering is the scattering of light by particles much smaller than the wavelength of the light, explaining why the sky appears blue.".to_string();
            actual_tokens = temp_text.split_whitespace().count();
        } else {
            let proxy = InferenceProxy::new(backend_url);
            match proxy.proxy_query(req.clone()).await {
                Ok((mut stream, get_metrics)) => {
                    while let Some(chunk) = stream.next().await {
                        if let Ok(token) = chunk {
                            temp_text.push_str(&token);
                        }
                    }
                    let (tokens, _) = get_metrics();
                    actual_tokens = tokens;
                }
                Err(e) => {
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("Local inference failed: {}", e) }))
                    ).into_response();
                }
            }
        }

        let elapsed = start.elapsed().as_secs_f64();
        let tps = if elapsed > 0.0 { actual_tokens as f64 / elapsed } else { 25.0 };

        // Record local inference performance
        state.network_stats.record_local_inference(tps);

        let median_tps = state.network_stats.get_median_tps();
        let query_hash = sha256_hash(&req.prompt);
        let mut local_receipt = InferenceReceipt {
            receipt_id: uuid::Uuid::new_v4().to_string(),
            client_pubkey: state.client_hex.clone(),
            node_pubkey: state.node_hex.clone(),
            query_hash,
            tokens_generated: actual_tokens,
            tps,
            network_median_tps: median_tps,
            tqw_nano,
            amount_nano: 0,
            timestamp: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
            node_signature: None,
            client_signature: None,
        };

        local_receipt.amount_nano = local_receipt.compute_amount();
        
        sign_receipt_as_node(&state.node_priv, &mut local_receipt);
        sign_receipt_as_client(&state.client_priv, &mut local_receipt);

        let _ = state.ledger.claim_receipt(&local_receipt);
        state.reputation_db.record_success(&state.node_hex, elapsed, actual_tokens as u32);

        // Broadcast local claimed receipt too
        let p2p_node = state.p2p_node.clone();
        let receipt_clone = local_receipt.clone();
        tokio::spawn(async move {
            let _ = p2p_node.publish_receipt(receipt_clone).await;
        });
        
        text = temp_text;
        receipt = Some(local_receipt);
    }

    let created = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();

    if stream {
        let stream = simulate_sse_stream(text, model, req.request_id);
        Sse::new(stream).into_response()
    } else {
        Json(json!({
            "id": format!("chatcmpl-{}", req.request_id),
            "object": "chat.completion",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": text
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": req.prompt.split_whitespace().count(),
                "completion_tokens": receipt.as_ref().map(|r| r.tokens_generated).unwrap_or(0),
                "total_tokens": req.prompt.split_whitespace().count() + receipt.as_ref().map(|r| r.tokens_generated).unwrap_or(0)
            }
        })).into_response()
    }
}

async fn handle_balance(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    Json(json!({
        "client_balance": format_itokens(state.ledger.get_balance(&state.client_hex)),
        "node_balance": format_itokens(state.ledger.get_balance(&state.node_hex)),
    }))
}

async fn handle_reputation(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let score = state.reputation_db.get_score(&state.node_hex);
    Json(json!({
        "node_reputation_score": score,
    }))
}

async fn handle_stats(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    Json(json!({
        "median_tps": state.network_stats.get_median_tps(),
    }))
}

// ─── SSE Helper ─────────────────────────────────────────────────────────────────

fn simulate_sse_stream(
    text: String,
    model: String,
    request_id: String,
) -> impl futures::Stream<Item = Result<SseEvent, std::convert::Infallible>> {
    async_stream::stream! {
        let words: Vec<String> = text.split_inclusive(' ').map(|s| s.to_string()).collect();
        let created = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();

        for word in words {
            let chunk = json!({
                "id": format!("chatcmpl-{}", request_id),
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {
                        "content": word
                    },
                    "finish_reason": null
                }]
            });
            yield Ok(SseEvent::default().data(chunk.to_string()));
            tokio::time::sleep(Duration::from_millis(30)).await;
        }

        let final_chunk = json!({
            "id": format!("chatcmpl-{}", request_id),
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop"
            }]
        });
        yield Ok(SseEvent::default().data(final_chunk.to_string()));
        yield Ok(SseEvent::default().data("[DONE]"));
    }
}

// ─── Legacy Demo Loop Flow ──────────────────────────────────────────────────────

async fn run_demo_loop(
    args: Args,
    ledger: Arc<LocalLedger>,
    reputation_db: Arc<ReputationDb>,
    node_priv: ed25519_dalek::SigningKey,
    client_priv: ed25519_dalek::SigningKey,
    node_hex: String,
    client_hex: String,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Running single-shot demo flow...");
    let router = HarnessRouter::new(reputation_db.clone());

    // Discover local engines
    let detector = PortDetector::new();
    let detected = detector.scan_all().await;

    let (backend_url, target_model, tqw_nano) = if let Some(engine) = detected.first() {
        if let Some(model) = engine.active_models.first().cloned() {
            info!(
                engine = %engine.name,
                url = %engine.url,
                model = %model.name,
                tqw = %format_itokens(model.tqw_nano),
                "Using detected LLM engine"
            );
            (engine.url.clone(), model.name, model.tqw_nano)
        } else {
            warn!("Detected LLM engine has no active models — using mock inference");
            (
                "http://localhost:9999".to_string(),
                "mock-llama3-8b".to_string(),
                NANO_PER_ITOKEN / 100,
            )
        }
    } else {
        warn!("No local LLM engines detected — using mock inference");
        (
            "http://localhost:9999".to_string(),
            "mock-llama3-8b".to_string(),
            NANO_PER_ITOKEN / 100, // 0.01 iToken/token
        )
    };

    let prompt = "Explain Rayleigh scattering in one simple sentence.";
    let req = InferenceRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        prompt: prompt.to_string(),
        model: target_model.clone(),
        max_tokens: Some(50),
        temperature: 0.0,
    };

    req.validate()?;

    info!(request_id = %req.request_id, model = %req.model, "Routing inference request");
    let candidates = vec![node_hex.clone()];
    let (primary_node, _) = router.resolve_routing(&req.model, candidates)?;
    info!(primary_node = %primary_node, "Query routed");

    println!("--------------------------------------------------");
    println!("[Query] \"{}\"", prompt);
    print!("[Response] ");

    let start_instant = std::time::Instant::now();
    let mut actual_tokens = 0;
    let actual_tps;

    if backend_url == "http://localhost:9999" {
        let mock_text = "Rayleigh scattering is the scattering of light by particles much smaller than the wavelength of the light, explaining why the sky appears blue.";
        let words: Vec<&str> = mock_text.split_whitespace().collect();
        for word in &words {
            print!("{} ", word);
            std::io::Write::flush(&mut std::io::stdout())?;
            tokio::time::sleep(Duration::from_millis(50)).await;
            actual_tokens += 1;
        }
        println!();
        let elapsed = start_instant.elapsed().as_secs_f64();
        actual_tps = actual_tokens as f64 / elapsed;
    } else {
        let proxy = InferenceProxy::new(backend_url);
        match proxy.proxy_query(req.clone()).await {
            Ok((mut stream, get_metrics)) => {
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(token) => {
                            print!("{}", token);
                            std::io::Write::flush(&mut std::io::stdout())?;
                        }
                        Err(e) => {
                            error!(error = %e, "Stream error");
                            reputation_db.record_failure(&primary_node);
                            return Ok(());
                        }
                    }
                }
                println!();
                let (tokens, tps) = get_metrics();
                actual_tokens = tokens;
                actual_tps = tps;
            }
            Err(e) => {
                error!(error = %e, "Failed to proxy inference request");
                reputation_db.record_failure(&primary_node);
                return Ok(());
            }
        }
    }

    let duration_secs = start_instant.elapsed().as_secs_f64();

    // Build and sign receipt
    let query_hash = sha256_hash(prompt);
    let mut receipt = InferenceReceipt {
        receipt_id: uuid::Uuid::new_v4().to_string(),
        client_pubkey: client_hex.clone(),
        node_pubkey: node_hex.clone(),
        query_hash,
        tokens_generated: actual_tokens,
        tps: actual_tps,
        network_median_tps: args.median_tps,
        tqw_nano,
        amount_nano: 0,
        timestamp: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs(),
        node_signature: None,
        client_signature: None,
    };

    receipt.amount_nano = receipt.compute_amount();

    sign_receipt_as_node(&node_priv, &mut receipt);
    sign_receipt_as_client(&client_priv, &mut receipt);

    match ledger.claim_receipt(&receipt) {
        Ok(_) => {
            println!("--------------------------------------------------");
            println!(
                "[Ledger] Payout: {} iTokens → Node",
                format_itokens(receipt.amount_nano)
            );
            println!(
                "[Ledger] Client Balance: {} iTokens",
                format_itokens(ledger.get_balance(&client_hex))
            );
            println!(
                "[Ledger] Node Balance:   {} iTokens",
                format_itokens(ledger.get_balance(&node_hex))
            );

            reputation_db.record_success(&primary_node, duration_secs, actual_tokens as u32);
            println!(
                "[Reputation] Node Score: {:.4}",
                reputation_db.get_score(&primary_node)
            );
        }
        Err(e) => {
            error!(error = %e, "Receipt claim failed");
        }
    }

    println!("==================================================");
    Ok(())
}

// ─── Rate Limiting Middleware ───────────────────────────────────────────────────

async fn rate_limit_middleware(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> impl IntoResponse {
    let limit = 60; // 60 requests
    let window = Duration::from_secs(60); // per 1 minute
    let ip = addr.ip();
    
    let allowed = {
        let mut limiter = state.rate_limiter.lock();
        limiter.check_limit(ip, limit, window)
    };

    if allowed {
        next.run(req).await
    } else {
        (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(serde_json::json!({
                "error": "Rate limit exceeded. Maximum 60 requests per minute."
            }))
        ).into_response()
    }
}

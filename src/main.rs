use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use futures::StreamExt;
use clap::Parser;
use tracing::{info, error, warn};

use itoken_core::types::{InferenceRequest, InferenceReceipt, NANO_PER_ITOKEN, format_itokens};
use itoken_core::crypto::{
    load_or_generate_keypair, pubkey_to_hex,
    sign_receipt_as_node, sign_receipt_as_client, sha256_hash,
};
use itoken_inference::detector::PortDetector;
use itoken_inference::proxy::InferenceProxy;
use itoken_harness::reputation::ReputationDb;
use itoken_harness::routing::HarnessRouter;
use itoken_ledger::LocalLedger;

// ─── CLI Arguments ─────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
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

    /// Run the demo flow (single mock client→node→payment cycle)
    #[arg(long)]
    demo: bool,

    /// Network median TPS for reward calculation (will be dynamic in production)
    #[arg(long, default_value = "25.0")]
    median_tps: f64,
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

    // Initialize persistent key storage
    let node_key_path = args.data_dir.join("node_key.dat");
    let client_key_path = args.data_dir.join("client_key.dat");

    let (node_priv, node_pub) = load_or_generate_keypair(&node_key_path)?;
    let node_hex = pubkey_to_hex(&node_pub);

    let (client_priv, client_pub) = load_or_generate_keypair(&client_key_path)?;
    let client_hex = pubkey_to_hex(&client_pub);

    info!(node_pubkey = %node_hex, "Node identity loaded");
    info!(client_pubkey = %client_hex, "Client identity loaded");

    // Initialize ledger and reputation (with error handling, not panics)
    let ledger_path = args.data_dir.join("ledger.json");
    let reputation_path = args.data_dir.join("reputation.json");

    let ledger = Arc::new(
        LocalLedger::new(ledger_path.to_str().unwrap_or("ledger.json"))?
    );
    let reputation_db = Arc::new(
        ReputationDb::new(reputation_path.to_str().unwrap_or("reputation.json"))?
    );
    let router = HarnessRouter::new(reputation_db.clone());

    // Fund demo accounts (only if new)
    ledger.register_account(&client_hex, 100 * NANO_PER_ITOKEN);
    ledger.register_account(&node_hex, 0);

    info!(
        client_balance = %format_itokens(ledger.get_balance(&client_hex)),
        node_balance = %format_itokens(ledger.get_balance(&node_hex)),
        "Ledger balances loaded"
    );

    // Discover local LLM engines
    info!("Scanning for local LLM engines...");
    let detector = PortDetector::new();

    let detected = if let Some(ref custom_url) = args.backend {
        match detector.probe_custom(custom_url).await {
            Ok(engine) => vec![engine],
            Err(e) => {
                warn!(url = %custom_url, error = %e, "Custom backend not reachable");
                Vec::new()
            }
        }
    } else {
        detector.scan_all().await
    };

    let (backend_url, target_model, tqw_nano) = if let Some(engine) = detected.first() {
        let model = engine.active_models.first().unwrap().clone();
        info!(
            engine = %engine.name,
            url = %engine.url,
            model = %model.name,
            tqw = %format_itokens(model.tqw_nano),
            params = %model.parameters,
            "Using detected LLM engine"
        );
        (engine.url.clone(), model.name, model.tqw_nano)
    } else {
        warn!("No local LLM engines detected — using mock inference");
        (
            "http://localhost:9999".to_string(),
            "mock-llama3-8b".to_string(),
            NANO_PER_ITOKEN / 100, // 0.01 iToken/token
        )
    };

    // Execute inference demo
    let prompt = "Explain Rayleigh scattering in one simple sentence.";
    let req = InferenceRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        prompt: prompt.to_string(),
        model: target_model.clone(),
        max_tokens: Some(50),
        temperature: 0.0,
    };

    // Validate request
    req.validate()?;

    info!(request_id = %req.request_id, model = %req.model, "Routing inference request");
    let candidates = vec![node_hex.clone()];
    let (primary_node, _backups) = router.resolve_routing(&req.model, candidates)?;
    info!(primary_node = %primary_node, "Query routed");

    // Stream tokens
    println!("--------------------------------------------------");
    println!("[Query] \"{}\"", prompt);
    print!("[Response] ");

    let start_instant = std::time::Instant::now();
    let mut actual_tokens = 0;
    let actual_tps;

    if backend_url == "http://localhost:9999" {
        // Mock stream
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

    info!(
        tokens = actual_tokens,
        tps = format!("{:.2}", actual_tps),
        duration = format!("{:.2}s", duration_secs),
        "Inference completed"
    );

    // Build and sign receipt
    let query_hash = sha256_hash(prompt);
    let mut receipt = InferenceReceipt {
        receipt_id: uuid::Uuid::new_v4().to_string(),
        client_pubkey: client_hex.clone(),
        node_pubkey: node_hex.clone(),
        query_hash,
        tokens_generated: actual_tokens,
        tps: actual_tps,
        tqw_nano,
        amount_nano: 0,
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs(),
        node_signature: None,
        client_signature: None,
    };

    // Compute payment (deterministic integer arithmetic)
    receipt.amount_nano = receipt.compute_amount(args.median_tps);

    info!(
        receipt_id = %receipt.receipt_id,
        amount = %format_itokens(receipt.amount_nano),
        tqw = %format_itokens(receipt.tqw_nano),
        speed_mult = format!("{:.2}x", receipt.tps_multiplier(args.median_tps)),
        "Receipt computed"
    );

    // Dual signing
    sign_receipt_as_node(&node_priv, &mut receipt);
    sign_receipt_as_client(&client_priv, &mut receipt);
    info!("Receipt dual-signed by node and client");

    // Claim receipt in ledger
    match ledger.claim_receipt(&receipt, args.median_tps) {
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

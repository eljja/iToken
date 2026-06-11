use std::sync::Arc;
use std::time::Duration;
use futures::StreamExt;

use dpu_core::types::{InferenceRequest, InferenceReceipt};
use dpu_core::crypto::{generate_keypair, pubkey_to_hex, sign_receipt_as_node, sign_receipt_as_client, sha256_hash};
use dpu_inference::detector::PortDetector;
use dpu_inference::proxy::InferenceProxy;
use dpu_harness::reputation::ReputationDb;
use dpu_harness::routing::HarnessRouter;
use dpu_ledger::LocalLedger;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Initialize Logging
    tracing_subscriber::fmt::init();
    println!("==================================================");
    println!("        iToken Network - Phase 1 PoC Demo        ");
    println!("==================================================");

    // 2. Initialize Ledger and Reputation Databases
    let ledger = Arc::new(LocalLedger::new("ledger.json"));
    let reputation_db = Arc::new(ReputationDb::new("reputation.json"));
    let router = HarnessRouter::new(reputation_db.clone());

    // 3. Generate Cryptographic Keypairs for Client and Node
    let (client_priv, client_pub) = generate_keypair();
    let (node_priv, node_pub) = generate_keypair();

    let client_hex = pubkey_to_hex(&client_pub);
    let node_hex = pubkey_to_hex(&node_pub);

    println!("[Keys] Client PubKey: {}", client_hex);
    println!("[Keys] Node PubKey:   {}", node_hex);

    // 4. Fund Accounts in the Local Ledger
    ledger.register_account(&client_hex, 100.0); // Client starts with 100 iTokens
    ledger.register_account(&node_hex, 0.0);      // Node starts with 0 iTokens

    println!("[Ledger] Initial Client Balance: {} iTokens", ledger.get_balance(&client_hex));
    println!("[Ledger] Initial Node Balance:   {} iTokens", ledger.get_balance(&node_hex));
    println!("--------------------------------------------------");

    // 5. Scan for Local LLM Engines (Ollama, LM Studio, etc.)
    println!("[Discovery] Scanning local ports for running LLM servers...");
    let detector = PortDetector::new();
    let detected = detector.scan_all().await;

    let (backend_url, target_model, tqw) = if let Some(engine) = detected.first() {
        let model = engine.active_models.first().unwrap().clone();
        println!(
            "[Discovery] Detected active engine '{}' at {}",
            engine.name, engine.url
        );
        println!(
            "[Discovery] Using model: {} (TQW: {}, Params: {})",
            model.name, model.tqw, model.parameters
        );
        (engine.url.clone(), model.name, model.tqw)
    } else {
        println!("[Discovery] No running local LLMs found (Ollama/LM Studio offline).");
        println!("[Discovery] Starting local Mock Server for demo demonstration...");
        // Fallback mock engine config
        ("http://localhost:9999".to_string(), "mock-llama3-8b".to_string(), 0.01)
    };

    // 6. Set up the client request
    let prompt = "Explain Rayleigh scattering in one simple sentence.";
    let req = InferenceRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        prompt: prompt.to_string(),
        model: target_model.clone(),
        max_tokens: Some(50),
        temperature: 0.0, // Force greedy decoding for verification determinism
    };

    println!("[Harness] Routing request '{}'...", req.request_id);
    let candidates = vec![node_hex.clone()]; // In mock, Node is the candidate
    let (primary_node, _backups) = router.resolve_routing(&req.model, candidates)?;
    println!("[Harness] Routed query to primary node: {}", primary_node);
    println!("--------------------------------------------------");

    // 7. Execute Query & Stream Tokens
    println!("[Compute] Query: \"{}\"", prompt);
    print!("[Compute] Streaming tokens: ");

    let start_instant = std::time::Instant::now();
    let mut actual_tokens = 0;
    let actual_tps;

    if backend_url == "http://localhost:9999" {
        // Simulating mock LLM stream response
        let mock_text = "Rayleigh scattering is the scattering of light by particles much smaller than the wavelength of the light, explaining why the sky appears blue.";
        let words: Vec<&str> = mock_text.split_whitespace().collect();
        for word in words {
            print!("{} ", word);
            std::io::Write::flush(&mut std::io::stdout())?;
            tokio::time::sleep(Duration::from_millis(50)).await;
            actual_tokens += 1;
        }
        println!();
        let elapsed = start_instant.elapsed().as_secs_f64();
        actual_tps = actual_tokens as f64 / elapsed;
    } else {
        // Execute real OpenAI API Proxy stream
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
                            eprintln!("\n[Compute Error] Stream error: {}", e);
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
                eprintln!("[Compute Error] Failed to proxy request: {}", e);
                reputation_db.record_failure(&primary_node);
                return Ok(());
            }
        }
    }

    let duration_secs = start_instant.elapsed().as_secs_f64();
    println!("--------------------------------------------------");
    println!(
        "[Compute Stats] Tokens: {}, Speed: {:.2} TPS, Duration: {:.2}s",
        actual_tokens, actual_tps, duration_secs
    );

    // 8. Generate & Sign Inference Receipt (iToken Billing Ticket)
    let query_hash = sha256_hash(prompt);
    let mut receipt = InferenceReceipt {
        receipt_id: uuid::Uuid::new_v4().to_string(),
        client_pubkey: client_hex.clone(),
        node_pubkey: node_hex.clone(),
        query_hash,
        tokens_generated: actual_tokens,
        tps: actual_tps,
        tqw,
        amount_itokens: 0.0, // Calculated next
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs(),
        node_signature: None,
        client_signature: None,
    };

    // Set computed payment
    receipt.amount_itokens = receipt.compute_amount();

    println!(
        "[Billing] Receipt billing amount: {:.5} iTokens (TQW: {}, Speed Mult: {:.2}x)",
        receipt.amount_itokens, receipt.tqw, receipt.tps_multiplier()
    );

    // Node signs
    sign_receipt_as_node(&node_priv, &mut receipt);
    // Client signs
    sign_receipt_as_client(&client_priv, &mut receipt);

    println!("[Billing] Receipt signed by Node and Client.");

    // 9. Claim Receipt in the Ledger
    println!("[Ledger] Submitting receipt to local ledger...");
    match ledger.claim_receipt(&receipt) {
        Ok(_) => {
            println!("[Ledger] Receipt claimed successfully! Payout executed.");
            println!(
                "[Ledger] New Client Balance: {:.5} iTokens",
                ledger.get_balance(&client_hex)
            );
            println!(
                "[Ledger] New Node Balance:   {:.5} iTokens",
                ledger.get_balance(&node_hex)
            );

            // Record success in reputation database
            reputation_db.record_success(&primary_node, duration_secs, actual_tokens as u32);
            println!(
                "[Reputation] Updated node score for {}: {:.4}",
                primary_node,
                reputation_db.get_score(&primary_node)
            );
        }
        Err(e) => {
            eprintln!("[Ledger Error] Claim failed: {}", e);
        }
    }

    println!("==================================================");
    Ok(())
}

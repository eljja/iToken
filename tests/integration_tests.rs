use std::time::Duration;
use itoken_network::{P2PNode, P2PEvent, P2PInferenceRequest, P2PRequest, P2PResponse};
use itoken_core::types::{InferenceRequest, InferenceReceipt};

#[tokio::test]
async fn test_p2p_request_response_and_sync() {
    // 1. Initialize two nodes on local loopback with dedicated test ports
    let node1 = P2PNode::new().unwrap();
    let node2 = P2PNode::new().unwrap();

    let addr1 = "/ip4/127.0.0.1/tcp/50055".parse().unwrap();
    node1.start_listening(addr1).await.unwrap();

    let addr2 = "/ip4/127.0.0.1/tcp/50056".parse().unwrap();
    node2.start_listening(addr2).await.unwrap();

    // Dial node1 from node2
    node2.dial("/ip4/127.0.0.1/tcp/50055".parse().unwrap()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await; // Allow dial and connection upgrade to complete

    // Spawning node 1's event receiver to handle requests in the background
    let node1_peer_id = node1.peer_id();
    let mut node1_rx = node1;
    let handle = tokio::spawn(async move {
        // Loop twice: once for inference request, once for sync request
        for _ in 0..2 {
            if let Some(event) = node1_rx.recv_event().await {
                match event {
                    P2PEvent::Request { peer_id: _, request, channel } => {
                        match request {
                            P2PRequest::Inference(p2p_req) => {
                                let receipt = InferenceReceipt {
                                    receipt_id: "test-receipt-id".to_string(),
                                    client_pubkey: p2p_req.client_pubkey,
                                    node_pubkey: "node1-key".to_string(),
                                    query_hash: "hash".to_string(),
                                    tokens_generated: 10,
                                    tps: 20.0,
                                    network_median_tps: 20.0,
                                    tqw_nano: 10_000_000,
                                    amount_nano: 200_000_000,
                                    timestamp: 1700000000,
                                    node_signature: None,
                                    client_signature: None,
                                };
                                let _ = node1_rx.send_response(channel, P2PResponse::InferenceSuccess {
                                    text: "Response from node1".to_string(),
                                    receipt,
                                }).await;
                            }
                            P2PRequest::SyncLedger => {
                                let mut balances = std::collections::HashMap::new();
                                balances.insert("alice".to_string(), 1000);
                                let mut claimed_receipts = std::collections::HashSet::new();
                                claimed_receipts.insert("r1".to_string());
                                let _ = node1_rx.send_response(channel, P2PResponse::LedgerSync {
                                    balances,
                                    claimed_receipts,
                                }).await;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        node1_rx
    });

    // Send inference from node2 to node1
    let req = InferenceRequest {
        request_id: "r-1".to_string(),
        prompt: "hello".to_string(),
        model: "mock-model".to_string(),
        max_tokens: Some(50),
        temperature: 0.7,
    };
    let p2p_req = P2PInferenceRequest {
        req,
        client_pubkey: "alice-key".to_string(),
    };
    let resp = node2.send_inference(node1_peer_id, p2p_req).await.unwrap();
    match resp {
        P2PResponse::InferenceSuccess { text, receipt } => {
            assert_eq!(text, "Response from node1");
            assert_eq!(receipt.receipt_id, "test-receipt-id");
        }
        _ => panic!("Expected inference success"),
    }

    // Send ledger sync request from node2 to node1
    let sync_resp = node2.send_ledger_sync(node1_peer_id).await.unwrap();
    match sync_resp {
        P2PResponse::LedgerSync { balances, claimed_receipts } => {
            assert_eq!(balances.get("alice"), Some(&1000));
            assert!(claimed_receipts.contains("r1"));
        }
        _ => panic!("Expected ledger sync success"),
    }

    // Clean up node loops
    let node1_restored = handle.await.unwrap();
    node1_restored.shutdown().await;
    node2.shutdown().await;
}

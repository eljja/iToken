use std::collections::HashMap;
use std::error::Error;
use std::time::{Duration, Instant};
use futures::channel::oneshot;
use futures::StreamExt;
use libp2p::{
    gossipsub,
    identify,
    kad::{self, store::MemoryStore, Behaviour as KadBehaviour},
    noise,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp,
    yamux,
    Multiaddr,
    PeerId,
    Swarm,
};
use libp2p_request_response as request_response;
use tokio::sync::mpsc;
use tracing::{info, warn, debug};
use serde::{Deserialize, Serialize};

// ─── Protocol Constants ────────────────────────────────────────────────────────

const PROTOCOL_VERSION: &str = "/itoken/1.0.0";
const HEALTH_TOPIC: &str = "itoken/health/v1";
const LEDGER_TOPIC: &str = "itoken/ledger/v1";
const KAD_RECORD_TTL_SECS: u64 = 3600;     // 1 hour
const QUERY_TIMEOUT_SECS: u64 = 60;

// ─── P2P Inference Message Types ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct P2PInferenceRequest {
    pub req: itoken_core::types::InferenceRequest,
    pub client_pubkey: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum P2PRequest {
    Inference(P2PInferenceRequest),
    SyncLedger,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum P2PResponse {
    InferenceSuccess {
        text: String,
        receipt: itoken_core::types::InferenceReceipt,
    },
    LedgerSync {
        balances: std::collections::HashMap<String, u64>,
        claimed_receipts: std::collections::HashSet<String>,
    },
    Error(String),
}

// ─── P2P Commands ──────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum P2PCommand {
    StartListening {
        addr: Multiaddr,
        responder: oneshot::Sender<Result<(), String>>,
    },
    Dial {
        addr: Multiaddr,
        responder: oneshot::Sender<Result<(), String>>,
    },
    AdvertiseModel {
        model: String,
        responder: oneshot::Sender<Result<(), String>>,
    },
    SearchModel {
        model: String,
        responder: oneshot::Sender<Vec<PeerId>>,
    },
    PublishHealth {
        payload: Vec<u8>,
        responder: oneshot::Sender<Result<(), String>>,
    },
    PublishReceipt {
        receipt: itoken_core::types::InferenceReceipt,
        responder: oneshot::Sender<Result<(), String>>,
    },
    SendInference {
        peer_id: PeerId,
        request: P2PInferenceRequest,
        responder: oneshot::Sender<Result<P2PResponse, String>>,
    },
    SendLedgerSync {
        peer_id: PeerId,
        responder: oneshot::Sender<Result<P2PResponse, String>>,
    },
    SendResponse {
        channel: request_response::ResponseChannel<P2PResponse>,
        response: P2PResponse,
    },
    GetConnectedPeers {
        responder: oneshot::Sender<Vec<PeerId>>,
    },
    Shutdown,
}

// ─── P2P Events ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum P2PEvent {
    Request {
        peer_id: PeerId,
        request: P2PRequest,
        channel: request_response::ResponseChannel<P2PResponse>,
    },
    HealthReceived {
        peer_id: String,
        tps_avg: f64,
        models: Vec<String>,
    },
    LedgerReceiptReceived {
        receipt: itoken_core::types::InferenceReceipt,
    },
}

// ─── Network Behaviour ─────────────────────────────────────────────────────────

#[derive(NetworkBehaviour)]
pub struct ItokenBehaviour {
    pub kademlia: KadBehaviour<MemoryStore>,
    pub gossipsub: gossipsub::Behaviour,
    pub identify: identify::Behaviour,
    pub request_response: request_response::json::Behaviour<P2PRequest, P2PResponse>,
}

// ─── P2P Node ──────────────────────────────────────────────────────────────────

pub struct P2PNode {
    command_tx: mpsc::Sender<P2PCommand>,
    event_rx: tokio::sync::Mutex<mpsc::Receiver<P2PEvent>>,
    local_peer_id: PeerId,
}

impl P2PNode {
    pub fn new() -> Result<Self, Box<dyn Error>> {
        let local_key = libp2p::identity::Keypair::generate_ed25519();
        let local_peer_id = PeerId::from(local_key.public());
        info!(peer_id = %local_peer_id, "P2P node initializing");

        // Build swarm with TCP + Noise + Yamux
        let swarm = libp2p::SwarmBuilder::with_existing_identity(local_key)
            .with_tokio()
            .with_tcp(
                tcp::Config::default(),
                noise::Config::new,
                yamux::Config::default,
            )?
            .with_dns()?
            .with_behaviour(|key: &libp2p::identity::Keypair| {
                // Kademlia DHT
                let store = MemoryStore::new(key.public().to_peer_id());
                let mut kad_cfg = kad::Config::default();
                kad_cfg.set_record_ttl(Some(Duration::from_secs(KAD_RECORD_TTL_SECS)));
                let kademlia = KadBehaviour::with_config(
                    key.public().to_peer_id(),
                    store,
                    kad_cfg,
                );

                // Gossipsub with peer scoring
                let gossip_cfg = gossipsub::ConfigBuilder::default()
                    .heartbeat_interval(Duration::from_secs(1))
                    .validation_mode(gossipsub::ValidationMode::Strict)
                    .max_transmit_size(65536) // 64KB max message size
                    .build()
                    .map_err(std::io::Error::other)?;
                let mut gossipsub_behaviour = gossipsub::Behaviour::new(
                    gossipsub::MessageAuthenticity::Signed(key.clone()),
                    gossip_cfg,
                )?;

                // Subscribe to health topic
                let health_topic = gossipsub::IdentTopic::new(HEALTH_TOPIC);
                gossipsub_behaviour.subscribe(&health_topic)
                    .map_err(|e| std::io::Error::other(format!("{}", e)))?;

                // Subscribe to ledger topic
                let ledger_topic = gossipsub::IdentTopic::new(LEDGER_TOPIC);
                gossipsub_behaviour.subscribe(&ledger_topic)
                    .map_err(|e| std::io::Error::other(format!("{}", e)))?;

                // Identify protocol
                let identify = identify::Behaviour::new(identify::Config::new(
                    PROTOCOL_VERSION.to_string(),
                    key.public(),
                ));

                // Request-Response Behaviour
                let request_response = request_response::json::Behaviour::<P2PRequest, P2PResponse>::new(
                    [(libp2p::StreamProtocol::new(PROTOCOL_VERSION), request_response::ProtocolSupport::Full)],
                    request_response::Config::default(),
                );

                Ok(ItokenBehaviour {
                    kademlia,
                    gossipsub: gossipsub_behaviour,
                    identify,
                    request_response,
                })
            })?
            .with_swarm_config(|c: libp2p::swarm::Config| {
                c.with_idle_connection_timeout(Duration::from_secs(60))
            })
            .build();

        let (command_tx, command_rx) = mpsc::channel(256);
        let (event_tx, event_rx) = mpsc::channel(256);

        // Spawn swarm event loop
        tokio::spawn(run_swarm_loop(swarm, command_rx, event_tx));

        Ok(Self {
            command_tx,
            event_rx: tokio::sync::Mutex::new(event_rx),
            local_peer_id,
        })
    }

    pub fn peer_id(&self) -> PeerId {
        self.local_peer_id
    }

    pub async fn start_listening(&self, addr: Multiaddr) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(P2PCommand::StartListening { addr, responder: tx })
            .await
            .map_err(|e| format!("Command channel closed: {}", e))?;
        rx.await.map_err(|e| format!("Response channel closed: {}", e))?
    }

    pub async fn dial(&self, addr: Multiaddr) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(P2PCommand::Dial { addr, responder: tx })
            .await
            .map_err(|e| format!("Command channel closed: {}", e))?;
        rx.await.map_err(|e| format!("Response channel closed: {}", e))?
    }

    pub async fn advertise_model(&self, model: &str) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(P2PCommand::AdvertiseModel {
                model: model.to_string(),
                responder: tx,
            })
            .await
            .map_err(|e| format!("Command channel closed: {}", e))?;
        rx.await.map_err(|e| format!("Response channel closed: {}", e))?
    }

    pub async fn search_model(&self, model: &str) -> Vec<PeerId> {
        let (tx, rx) = oneshot::channel();
        if self
            .command_tx
            .send(P2PCommand::SearchModel {
                model: model.to_string(),
                responder: tx,
            })
            .await
            .is_ok()
        {
            rx.await.unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    pub async fn publish_health(&self, payload: Vec<u8>) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(P2PCommand::PublishHealth { payload, responder: tx })
            .await
            .map_err(|e| format!("Command channel closed: {}", e))?;
        rx.await.map_err(|e| format!("Response channel closed: {}", e))?
    }

    pub async fn publish_receipt(&self, receipt: itoken_core::types::InferenceReceipt) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(P2PCommand::PublishReceipt { receipt, responder: tx })
            .await
            .map_err(|e| format!("Command channel closed: {}", e))?;
        rx.await.map_err(|e| format!("Response channel closed: {}", e))?
    }

    pub async fn send_inference(&self, peer_id: PeerId, request: P2PInferenceRequest) -> Result<P2PResponse, String> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(P2PCommand::SendInference { peer_id, request, responder: tx })
            .await
            .map_err(|e| format!("Command channel closed: {}", e))?;
        rx.await.map_err(|e| format!("Response channel closed: {}", e))?
    }

    pub async fn send_ledger_sync(&self, peer_id: PeerId) -> Result<P2PResponse, String> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(P2PCommand::SendLedgerSync { peer_id, responder: tx })
            .await
            .map_err(|e| format!("Command channel closed: {}", e))?;
        rx.await.map_err(|e| format!("Response channel closed: {}", e))?
    }

    pub async fn recv_event(&self) -> Option<P2PEvent> {
        self.event_rx.lock().await.recv().await
    }

    pub async fn send_response(&self, channel: request_response::ResponseChannel<P2PResponse>, response: P2PResponse) -> Result<(), String> {
        self.command_tx
            .send(P2PCommand::SendResponse { channel, response })
            .await
            .map_err(|e| format!("Command channel closed: {}", e))
    }

    pub async fn get_connected_peers(&self) -> Vec<PeerId> {
        let (tx, rx) = oneshot::channel();
        if self.command_tx.send(P2PCommand::GetConnectedPeers { responder: tx }).await.is_ok() {
            rx.await.unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// Send shutdown signal to the P2P event loop.
    pub async fn shutdown(&self) {
        let _ = self.command_tx.send(P2PCommand::Shutdown).await;
    }

    /// Check if the command channel is closed (indicates shutdown).
    pub fn is_closed(&self) -> bool {
        self.command_tx.is_closed()
    }
}

// ─── Swarm Event Loop ──────────────────────────────────────────────────────────

struct PendingQuery {
    responder: oneshot::Sender<Vec<PeerId>>,
    peers: Vec<PeerId>,
    started_at: Instant,
}

async fn run_swarm_loop(
    mut swarm: Swarm<ItokenBehaviour>,
    mut command_rx: mpsc::Receiver<P2PCommand>,
    event_tx: mpsc::Sender<P2PEvent>,
) {
    let mut search_queries: HashMap<kad::QueryId, PendingQuery> = HashMap::new();
    let mut pending_inference_requests: HashMap<
        request_response::OutboundRequestId,
        oneshot::Sender<Result<P2PResponse, String>>,
    > = HashMap::new();

    let mut cleanup_interval = tokio::time::interval(Duration::from_secs(30));
    let health_topic = gossipsub::IdentTopic::new(HEALTH_TOPIC);
    let ledger_topic = gossipsub::IdentTopic::new(LEDGER_TOPIC);

    loop {
        tokio::select! {
            // Handle incoming commands
            Some(cmd) = command_rx.recv() => {
                match cmd {
                    P2PCommand::StartListening { addr, responder } => {
                        let res = swarm.listen_on(addr.clone())
                            .map(|_| {
                                info!(addr = %addr, "P2P listening started");
                            })
                            .map_err(|e| e.to_string());
                        let _ = responder.send(res);
                    }
                    P2PCommand::Dial { addr, responder } => {
                        let res = swarm.dial(addr.clone())
                            .map(|_| {
                                info!(addr = %addr, "Dialing peer");
                            })
                            .map_err(|e| e.to_string());
                        let _ = responder.send(res);
                    }
                    P2PCommand::AdvertiseModel { model, responder } => {
                        let key = kad::RecordKey::new(&model.as_bytes());
                        let record = kad::Record {
                            key: key.clone(),
                            value: swarm.local_peer_id().to_bytes(),
                            publisher: None,
                            expires: None,
                        };
                        let res = swarm.behaviour_mut().kademlia
                            .put_record(record, kad::Quorum::One)
                            .map(|_| {
                                info!(model = %model, "Model advertised on DHT");
                            })
                            .map_err(|e| e.to_string());
                        let _ = responder.send(res);
                    }
                    P2PCommand::SearchModel { model, responder } => {
                        let query_id = swarm.behaviour_mut().kademlia
                            .get_record(kad::RecordKey::new(&model.as_bytes()));
                        search_queries.insert(query_id, PendingQuery {
                            responder,
                            peers: Vec::new(),
                            started_at: Instant::now(),
                        });
                        debug!(model = %model, "DHT search initiated");
                    }
                    P2PCommand::PublishHealth { payload, responder } => {
                        let res = swarm.behaviour_mut().gossipsub
                            .publish(health_topic.clone(), payload)
                            .map(|_| {
                                debug!("Health broadcast published");
                            })
                            .map_err(|e| format!("Failed to publish health: {}", e));
                        let _ = responder.send(res);
                    }
                    P2PCommand::PublishReceipt { receipt, responder } => {
                        let res = match serde_json::to_vec(&receipt) {
                            Ok(bytes) => {
                                swarm.behaviour_mut().gossipsub
                                    .publish(ledger_topic.clone(), bytes)
                                    .map(|_| {
                                        info!(receipt_id = %receipt.receipt_id, "Receipt broadcasted over Gossipsub");
                                    })
                                    .map_err(|e| format!("Failed to publish receipt: {}", e))
                            }
                            Err(e) => Err(format!("Serialization error: {}", e)),
                        };
                        let _ = responder.send(res);
                    }
                    P2PCommand::SendInference { peer_id, request, responder } => {
                        let request_id = swarm.behaviour_mut().request_response.send_request(&peer_id, P2PRequest::Inference(request));
                        pending_inference_requests.insert(request_id, responder);
                    }
                    P2PCommand::SendLedgerSync { peer_id, responder } => {
                        let request_id = swarm.behaviour_mut().request_response.send_request(&peer_id, P2PRequest::SyncLedger);
                        pending_inference_requests.insert(request_id, responder);
                    }
                    P2PCommand::SendResponse { channel, response } => {
                        let _ = swarm.behaviour_mut().request_response.send_response(channel, response);
                    }
                    P2PCommand::GetConnectedPeers { responder } => {
                        let peers: Vec<PeerId> = swarm.connected_peers().copied().collect();
                        let _ = responder.send(peers);
                    }
                    P2PCommand::Shutdown => {
                        info!("P2P shutdown signal received");
                        break;
                    }
                }
            }
            // Handle swarm events
            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::Behaviour(ItokenBehaviourEvent::Kademlia(
                        kad::Event::OutboundQueryProgressed {
                            id,
                            result: kad::QueryResult::GetRecord(Ok(
                                kad::GetRecordOk::FoundRecord(peer_record)
                            )),
                            ..
                        }
                    )) => {
                        if let Some(pending) = search_queries.get_mut(&id) {
                            if let Ok(peer_id) = PeerId::from_bytes(&peer_record.record.value) {
                                pending.peers.push(peer_id);
                                debug!(peer = %peer_id, "Found peer for model query");
                            }
                        }
                    }
                    SwarmEvent::Behaviour(ItokenBehaviourEvent::Kademlia(
                        kad::Event::OutboundQueryProgressed {
                            id,
                            result: kad::QueryResult::GetRecord(result),
                            ..
                        }
                    )) => {
                        if let Some(pending) = search_queries.remove(&id) {
                            if pending.peers.is_empty() {
                                debug!("DHT search completed with no results: {:?}", result);
                            }
                            let _ = pending.responder.send(pending.peers);
                        }
                    }
                    SwarmEvent::Behaviour(ItokenBehaviourEvent::Gossipsub(
                        gossipsub::Event::Message { message, propagation_source, .. }
                    )) => {
                        debug!(
                            source = %propagation_source,
                            topic = %message.topic,
                            size = message.data.len(),
                            "Received Gossipsub message"
                        );
                        if message.topic == health_topic.hash() {
                            if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&message.data) {
                                if let (Some(tps_avg), Some(models_val)) = (
                                    val.get("tps_avg").and_then(|v| v.as_f64()),
                                    val.get("models").and_then(|v| v.as_array())
                                ) {
                                    let models: Vec<String> = models_val.iter()
                                        .filter_map(|m| m.as_str().map(|s| s.to_string()))
                                        .collect();
                                    
                                    let _ = event_tx.try_send(P2PEvent::HealthReceived {
                                        peer_id: propagation_source.to_string(),
                                        tps_avg,
                                        models,
                                    });
                                }
                            }
                        } else if message.topic == ledger_topic.hash() {
                            if let Ok(receipt) = serde_json::from_slice::<itoken_core::types::InferenceReceipt>(&message.data) {
                                let _ = event_tx.try_send(P2PEvent::LedgerReceiptReceived { receipt });
                            }
                        }
                    }
                    SwarmEvent::Behaviour(ItokenBehaviourEvent::RequestResponse(
                        request_response::Event::Message {
                            peer,
                            message: request_response::Message::Request {
                                request,
                                channel,
                                ..
                            },
                        }
                    )) => {
                        debug!(peer = %peer, "Received P2P request");
                        if let Err(e) = event_tx.try_send(P2PEvent::Request {
                            peer_id: peer,
                            request,
                            channel,
                        }) {
                            warn!("Event channel full, rejecting incoming request: {:?}", e);
                            if let mpsc::error::TrySendError::Full(P2PEvent::Request { channel, .. }) = e {
                                let _ = swarm.behaviour_mut().request_response.send_response(channel, P2PResponse::Error("Node is busy".to_string()));
                            }
                        }
                    }
                    SwarmEvent::Behaviour(ItokenBehaviourEvent::RequestResponse(
                        request_response::Event::Message {
                            peer: _,
                            message: request_response::Message::Response {
                                request_id,
                                response,
                            },
                        }
                    )) => {
                        debug!("Received response for request {:?}", request_id);
                        if let Some(responder) = pending_inference_requests.remove(&request_id) {
                            let _ = responder.send(Ok::<P2PResponse, String>(response));
                        }
                    }
                    SwarmEvent::Behaviour(ItokenBehaviourEvent::RequestResponse(
                        request_response::Event::OutboundFailure {
                            request_id,
                            error: err,
                            ..
                        }
                    )) => {
                        warn!("Outbound request {:?} failed: {:?}", request_id, err);
                        if let Some(responder) = pending_inference_requests.remove(&request_id) {
                            let _ = responder.send(Err::<P2PResponse, String>(format!("P2P request failed: {:?}", err)));
                        }
                    }
                    SwarmEvent::NewListenAddr { address, .. } => {
                        info!(address = %address, "Listening on new address");
                    }
                    SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                        info!(peer = %peer_id, "Peer connected");
                    }
                    SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
                        info!(peer = %peer_id, cause = ?cause, "Peer disconnected");
                    }
                    _ => {}
                }
            }
            // Periodic cleanup of stale search queries
            _ = cleanup_interval.tick() => {
                let now = Instant::now();
                let stale_ids: Vec<_> = search_queries.iter()
                    .filter(|(_, q)| now.duration_since(q.started_at).as_secs() > QUERY_TIMEOUT_SECS)
                    .map(|(id, _)| *id)
                    .collect();

                for id in stale_ids {
                    if let Some(pending) = search_queries.remove(&id) {
                        warn!("DHT search query timed out, returning partial results");
                        let _ = pending.responder.send(pending.peers);
                    }
                }
            }
        }
    }

    info!("P2P swarm event loop exited cleanly");
}

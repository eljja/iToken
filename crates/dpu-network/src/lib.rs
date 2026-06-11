use std::collections::HashMap;
use std::error::Error;
use std::time::Duration;
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
use tokio::sync::mpsc;
use tracing::info;

// Custom P2P command to interact with the Swarm actor
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
}

#[derive(NetworkBehaviour)]
pub struct DpuBehaviour {
    pub kademlia: KadBehaviour<MemoryStore>,
    pub gossipsub: gossipsub::Behaviour,
    pub identify: identify::Behaviour,
}

pub struct P2PNode {
    command_tx: mpsc::Sender<P2PCommand>,
    local_peer_id: PeerId,
}

impl P2PNode {
    pub fn new() -> Result<Self, Box<dyn Error>> {
        // Create local PeerId
        let local_key = libp2p::identity::Keypair::generate_ed25519();
        let local_peer_id = PeerId::from(local_key.public());
        info!("Local Peer ID: {:?}", local_peer_id);

        // Build TCP transport with Noise and Yamux
        let swarm = libp2p::SwarmBuilder::with_existing_identity(local_key)
            .with_tokio()
            .with_tcp(
                tcp::Config::default(),
                noise::Config::new,
                yamux::Config::default,
            )?
            .with_dns()?
            .with_behaviour(|key: &libp2p::identity::Keypair| {
                // Configure Kademlia DHT
                let store = MemoryStore::new(key.public().to_peer_id());
                let kad_cfg = kad::Config::default();
                let kademlia = KadBehaviour::with_config(key.public().to_peer_id(), store, kad_cfg);

                // Configure Gossipsub
                let gossip_cfg = gossipsub::ConfigBuilder::default()
                    .heartbeat_interval(Duration::from_secs(1))
                    .validation_mode(gossipsub::ValidationMode::Strict)
                    .build()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                let gossipsub = gossipsub::Behaviour::new(
                    gossipsub::MessageAuthenticity::Signed(key.clone()),
                    gossip_cfg,
                )?;

                // Configure Identify
                let identify = identify::Behaviour::new(identify::Config::new(
                    "/dpu/1.0.0".to_string(),
                    key.public(),
                ));

                Ok(DpuBehaviour {
                    kademlia,
                    gossipsub,
                    identify,
                })
            })?
            .with_swarm_config(|c: libp2p::swarm::Config| c.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();

        // Channels for communication
        let (command_tx, command_rx) = mpsc::channel(100);

        // Spawn Swarm Event Loop in the background
        tokio::spawn(run_swarm_loop(swarm, command_rx));

        Ok(Self {
            command_tx,
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
            .map_err(|e| e.to_string())?;
        rx.await.map_err(|e| e.to_string())?
    }

    pub async fn dial(&self, addr: Multiaddr) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(P2PCommand::Dial { addr, responder: tx })
            .await
            .map_err(|e| e.to_string())?;
        rx.await.map_err(|e| e.to_string())?
    }

    pub async fn advertise_model(&self, model: &str) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(P2PCommand::AdvertiseModel {
                model: model.to_string(),
                responder: tx,
            })
            .await
            .map_err(|e| e.to_string())?;
        rx.await.map_err(|e| e.to_string())?
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
}

async fn run_swarm_loop(
    mut swarm: Swarm<DpuBehaviour>,
    mut command_rx: mpsc::Receiver<P2PCommand>,
) {
    let mut search_queries = HashMap::new();

    loop {
        tokio::select! {
            // Handle incoming API commands
            Some(cmd) = command_rx.recv() => {
                match cmd {
                    P2PCommand::StartListening { addr, responder } => {
                        let res = swarm.listen_on(addr)
                            .map(|_| ())
                            .map_err(|e| e.to_string());
                        let _ = responder.send(res);
                    }
                    P2PCommand::Dial { addr, responder } => {
                        let res = swarm.dial(addr)
                            .map(|_| ())
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
                        let res = swarm.behaviour_mut().kademlia.put_record(record, kad::Quorum::One)
                            .map(|_| ())
                            .map_err(|e| e.to_string());
                        let _ = responder.send(res);
                    }
                    P2PCommand::SearchModel { model, responder } => {
                        let query_id = swarm.behaviour_mut().kademlia.get_record(kad::RecordKey::new(&model.as_bytes()));
                        search_queries.insert(query_id, (responder, Vec::new()));
                    }
                }
            }
            // Handle Swarm events
            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::Behaviour(DpuBehaviourEvent::Kademlia(kad::Event::OutboundQueryProgressed {
                        id,
                        result: kad::QueryResult::GetRecord(Ok(kad::GetRecordOk::FoundRecord(peer_record))),
                        ..
                    })) => {
                        if let Some((_, peers)) = search_queries.get_mut(&id) {
                            if let Ok(peer_id) = PeerId::from_bytes(&peer_record.record.value) {
                                peers.push(peer_id);
                            }
                        }
                    }
                    SwarmEvent::Behaviour(DpuBehaviourEvent::Kademlia(kad::Event::OutboundQueryProgressed {
                        id,
                        result: kad::QueryResult::GetRecord(result),
                        ..
                    })) => {
                        // Query finished
                        if let Some((responder, peers)) = search_queries.remove(&id) {
                            if peers.is_empty() {
                                // If DHT query failed or returned empty, we could do a backup discovery
                                info!("DHT search returned result: {:?}", result);
                            }
                            let _ = responder.send(peers);
                        }
                    }
                    SwarmEvent::NewListenAddr { address, .. } => {
                        info!("Listening on {:?}", address);
                    }
                    SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                        info!("Connected to peer: {:?}", peer_id);
                    }
                    SwarmEvent::ConnectionClosed { peer_id, .. } => {
                        info!("Connection closed to peer: {:?}", peer_id);
                    }
                    _ => {}
                }
            }
        }
    }
}

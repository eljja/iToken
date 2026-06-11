# iToken Network

> **Language:** [한국어로 읽기 (Korean)](README_KR.md)

Fusing BitTorrent-style P2P AI Inference Seeding with a Sovereign Compute-Backed Cryptocurrency.

iToken is a decentralized, peer-to-peer AI inference network that transforms consumer hardware (RTX GPUs, Apple Silicon Mac Minis/Pros, CPUs) into real-time digital assets. By decoupled integration with existing local LLM engines (such as LM Studio, Ollama, Kobold.cpp, or llama.cpp), users can share their idle computing power to serve queries globally and earn **iTokens** based on model quality and generation speed.

---

## 🌟 Key Features

1.  **Decoupled Local LLM API Proxy (Simple is Best)**
    *   No need to manage complex model registries or re-invent GPU accelerators.
    *   The iToken daemon acts as a lightweight P2P reverse proxy that hooks directly into standard OpenAI-compatible local APIs (`localhost:11434`, `localhost:1234`, etc.).
    *   **Zero Configuration:** Automatically scans common ports on startup to detect running models and publish capabilities to the P2P network.
2.  **Energy-Efficient "1 Query = 1 Node" Routing**
    *   Eliminates redundant double-computations during normal operations.
    *   Default queries are routed to exactly one trusted node based on Reputation and Staking, saving over 90% of electricity and compute resources compared to mesh consensus models.
3.  **Optimistic Challenger Economics**
    *   Keeps active verification overhead at 0%.
    *   If a client detects bad output quality, model forgery, or sluggish speeds, they file a dispute by staking iTokens.
    *   Validator nodes execute spot-checking (using IEEE 754 exponent/mantissa drift analysis) only on dispute. Cheating nodes have their staked iTokens slashed and are blacklisted.
4.  **Speed (TPS) & Hardware-Based Valuation**
    *   Compensation formula scales with generation speed (TPS) relative to the network's moving median:
        $$\text{Reward} = \text{Generated Tokens} \times TQW_{M, Q} \times \text{Speed Multiplier}(TPS)$$
    *   Fast GPU nodes earn high-value premiums. Slower CPU nodes bid for asynchronous batch tasks (e.g., translations) where latency is not critical.
5.  **Sovereign Independent Blockchain**
    *   Built on a Substrate Solo Chain, ensuring complete immunity to external cryptocurrency market crashes.

---

## 🏗️ Architecture Overview

```
┌──────────────────────────────────────────────────┐
│        iToken P2P Daemon (Rust Core)             │
│  ┌──────────┐  ┌─────────────┐  ┌────────────┐  │
│  │ libp2p   │  │ Gossipsub   │  │ Ledger     │  │
│  │ • QUIC   │  │ • Capability│  │ • Blocks   │  │
│  │ • NAT    │  │ • Peer Score│  │ • Tx       │  │
│  │ • DHT    │  │ • Heartbeat │  │ • Escrow   │  │
│  └──────────┘  └─────────────┘  └────────────┘  │
│  ┌─────────────────────────────────────────────┐ │
│  │   Harness Router (Consensus & Routing)      │ │
│  └─────────────────────────────────────────────┘ │
│  ┌─────────────────────────────────────────────┐ │
│  │   API Auto-Detection & Proxy (OpenAI API)   │ │
│  └─────────────────────────────────────────────┘ │
└──────────────────────────────┬───────────────────┘
                               │ Local HTTP / REST (OpenAI Compatible)
                               │ (e.g., http://localhost:11434, 1234)
┌──────────────────────────────▼───────────────────┐
│        External Local LLM Inference Engines      │
│  ┌─────────────────┐ ┌─────────────┐ ┌─────────┐  │
│  │ LM Studio       │ │ Ollama      │ │ vLLM    │  │
│  └─────────────────┘ └─────────────┘ └─────────┘  │
│  ┌─────────────────┐ ┌─────────────┐ ┌─────────┐  │
│  │ llama-server    │ │ Kobold.cpp  │ │ Others  │  │
│  └─────────────────┘ └─────────────┘ └─────────┘  │
└──────────────────────────────────────────────────┘
```

---

## 📂 Repository Structure (PoC Blueprint)

```
d:/Code/iToken/
├── Cargo.toml                    # Rust Workspace configuration
├── README.md                     # Repository Overview (English)
├── README_KR.md                  # Repository Overview (Korean)
│
├── crates/
│   ├── itoken-core/                 # Core shared types, cryptography, and transaction signatures
│   ├── itoken-network/              # rust-libp2p (QUIC transport, NAT hole punching, Kademlia DHT)
│   ├── itoken-inference/            # OpenAI API proxy, Port Scanner, and Tokenizer
│   ├── itoken-harness/              # Request routing, consensus, and node reputation metrics
│   └── itoken-ledger/               # Substrate solo chain / lightweight PoC ledger
```

---

## 📄 License

This project is licensed under the Apache License 2.0. See the `LICENSE` file for details.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelSpec {
    pub name: String,
    pub tqw: f64,
    pub parameters: String, // e.g. "8B", "70B"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceRequest {
    pub request_id: String,
    pub prompt: String,
    pub model: String,
    pub max_tokens: Option<usize>,
    pub temperature: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResponseChunk {
    pub request_id: String,
    pub text: String,
    pub is_done: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceReceipt {
    pub receipt_id: String,
    pub client_pubkey: String,
    pub node_pubkey: String,
    pub query_hash: String,
    pub tokens_generated: usize,
    pub tps: f64,
    pub tqw: f64,
    pub amount_itokens: f64,
    pub timestamp: u64,
    pub node_signature: Option<String>,
    pub client_signature: Option<String>,
}

impl InferenceReceipt {
    pub fn compute_amount(&self) -> f64 {
        (self.tokens_generated as f64) * self.tqw * self.tps_multiplier()
    }

    pub fn tps_multiplier(&self) -> f64 {
        // Speed Multiplier relative to average speed (here we assume a base threshold like 25 TPS as median)
        // In full impl, this moving median is fetched from the network state.
        let median_tps = 25.0;
        let ratio = self.tps / median_tps;
        ratio.powf(0.75).max(0.1).min(3.0) // Clamped between 0.1x and 3x
    }
}

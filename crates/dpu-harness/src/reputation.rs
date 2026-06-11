use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Mutex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeReputation {
    pub node_id: String,
    pub successes: u32,
    pub failures: u32,
    pub total_latency_secs: f64,
    pub total_tokens_generated: u32,
}

impl NodeReputation {
    pub fn new(node_id: String) -> Self {
        Self {
            node_id,
            successes: 1, // Start with 1 success to avoid division by zero or empty division
            failures: 0,
            total_latency_secs: 1.0, // Start with 1 sec default
            total_tokens_generated: 25, // Start with 25 tokens default (25 TPS average)
        }
    }

    pub fn success_rate(&self) -> f64 {
        let total = self.successes + self.failures;
        if total == 0 {
            return 1.0;
        }
        self.successes as f64 / total as f64
    }

    pub fn average_tps(&self) -> f64 {
        if self.total_latency_secs <= 0.0 {
            return 25.0;
        }
        self.total_tokens_generated as f64 / self.total_latency_secs
    }

    pub fn calculate_score(&self) -> f64 {
        // Reputation score is a mix of success rate and speed (TPS)
        // Score = (Success Rate * 0.7) + (TPS index * 0.3)
        let speed_index = (self.average_tps() / 50.0).min(1.0); // Normalised against 50 TPS
        (self.success_rate() * 0.7) + (speed_index * 0.3)
    }
}

pub struct ReputationDb {
    file_path: String,
    records: Mutex<HashMap<String, NodeReputation>>,
}

impl ReputationDb {
    pub fn new(file_path: &str) -> Self {
        let path = Path::new(file_path);
        let records = if path.exists() {
            let mut file = File::open(path).expect("Failed to open reputation file");
            let mut content = String::new();
            file.read_to_string(&mut content).expect("Failed to read reputation file");
            serde_json::from_str(&content).unwrap_or_else(|_| HashMap::new())
        } else {
            HashMap::new()
        };

        Self {
            file_path: file_path.to_string(),
            records: Mutex::new(records),
        }
    }

    pub fn get_score(&self, node_id: &str) -> f64 {
        let records = self.records.lock().unwrap();
        records.get(node_id)
            .map(|r| r.calculate_score())
            .unwrap_or(0.5) // Default middle reputation score for new nodes
    }

    pub fn record_success(&self, node_id: &str, latency_secs: f64, tokens: u32) {
        let mut records = self.records.lock().unwrap();
        let record = records.entry(node_id.to_string())
            .or_insert_with(|| NodeReputation::new(node_id.to_string()));
        
        record.successes += 1;
        record.total_latency_secs += latency_secs;
        record.total_tokens_generated += tokens;
        
        self.save_locked(&records);
    }

    pub fn record_failure(&self, node_id: &str) {
        let mut records = self.records.lock().unwrap();
        let record = records.entry(node_id.to_string())
            .or_insert_with(|| NodeReputation::new(node_id.to_string()));
        
        record.failures += 1;
        
        self.save_locked(&records);
    }

    fn save_locked(&self, records: &HashMap<String, NodeReputation>) {
        let serialized = serde_json::to_string_pretty(records).expect("Failed to serialize reputation");
        let mut file = File::create(&self.file_path).expect("Failed to write reputation file");
        file.write_all(serialized.as_bytes()).expect("Failed to write reputation content");
    }
}

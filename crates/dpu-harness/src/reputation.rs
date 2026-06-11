use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

// ─── Node Reputation ───────────────────────────────────────────────────────────

/// Decay factor per hour. After 24 hours unseen, score multiplied by ~0.79.
/// After 7 days, ~0.19. After 14 days, ~0.04.
const DECAY_LAMBDA: f64 = 0.01;

/// Default reputation score for unknown nodes
const DEFAULT_SCORE: f64 = 0.5;

/// Maximum age in seconds before a node's accumulated history is considered stale
const MAX_STALE_AGE_SECS: u64 = 7 * 24 * 3600; // 7 days

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeReputation {
    pub node_id: String,
    pub successes: u32,
    pub failures: u32,
    pub total_latency_secs: f64,
    pub total_tokens_generated: u32,
    /// Unix timestamp of last successful interaction
    pub last_seen: u64,
}

impl NodeReputation {
    pub fn new(node_id: String) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            node_id,
            successes: 0,
            failures: 0,
            total_latency_secs: 0.0,
            total_tokens_generated: 0,
            last_seen: now,
        }
    }

    pub fn success_rate(&self) -> f64 {
        let total = self.successes + self.failures;
        if total == 0 {
            return DEFAULT_SCORE;
        }
        self.successes as f64 / total as f64
    }

    pub fn average_tps(&self) -> f64 {
        if self.total_latency_secs <= 0.0 || self.total_tokens_generated == 0 {
            return 0.0;
        }
        self.total_tokens_generated as f64 / self.total_latency_secs
    }

    /// Calculate reputation score with time-based decay.
    /// Score = base_score × decay_factor
    /// base_score = (Success Rate × 0.7) + (Speed Index × 0.3)
    /// decay_factor = e^(-λ × hours_since_last_seen)
    pub fn calculate_score(&self) -> f64 {
        let total = self.successes + self.failures;
        if total == 0 {
            return DEFAULT_SCORE;
        }

        // Base score from performance
        let speed_index = (self.average_tps() / 50.0).min(1.0);
        let base_score = (self.success_rate() * 0.7) + (speed_index * 0.3);

        // Apply time decay
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let age_hours = (now.saturating_sub(self.last_seen)) as f64 / 3600.0;
        let decay = (-DECAY_LAMBDA * age_hours).exp();

        // Score decays toward DEFAULT_SCORE over time
        DEFAULT_SCORE + (base_score - DEFAULT_SCORE) * decay
    }
}

// ─── Reputation Database ───────────────────────────────────────────────────────

pub struct ReputationDb {
    file_path: String,
    records: Mutex<HashMap<String, NodeReputation>>,
}

impl ReputationDb {
    pub fn new(file_path: &str) -> Result<Self, String> {
        let path = Path::new(file_path);
        let records = if path.exists() {
            let mut file = File::open(path)
                .map_err(|e| format!("Failed to open reputation file: {}", e))?;
            let mut content = String::new();
            file.read_to_string(&mut content)
                .map_err(|e| format!("Failed to read reputation file: {}", e))?;
            serde_json::from_str(&content).unwrap_or_else(|e| {
                error!(error = %e, "Corrupted reputation file, starting fresh");
                HashMap::new()
            })
        } else {
            HashMap::new()
        };

        info!(path = file_path, nodes = records.len(), "Reputation database initialized");

        Ok(Self {
            file_path: file_path.to_string(),
            records: Mutex::new(records),
        })
    }

    pub fn get_score(&self, node_id: &str) -> f64 {
        let records = self.records.lock();
        records.get(node_id)
            .map(|r| r.calculate_score())
            .unwrap_or(DEFAULT_SCORE)
    }

    pub fn record_success(&self, node_id: &str, latency_secs: f64, tokens: u32) {
        let mut records = self.records.lock();
        let record = records.entry(node_id.to_string())
            .or_insert_with(|| NodeReputation::new(node_id.to_string()));

        record.successes += 1;
        record.total_latency_secs += latency_secs;
        record.total_tokens_generated += tokens;
        record.last_seen = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        info!(
            node_id = node_id,
            score = format!("{:.4}", record.calculate_score()),
            successes = record.successes,
            failures = record.failures,
            avg_tps = format!("{:.1}", record.average_tps()),
            "Reputation updated (success)"
        );

        self.save_atomic(&records);
    }

    pub fn record_failure(&self, node_id: &str) {
        let mut records = self.records.lock();
        let record = records.entry(node_id.to_string())
            .or_insert_with(|| NodeReputation::new(node_id.to_string()));

        record.failures += 1;

        warn!(
            node_id = node_id,
            score = format!("{:.4}", record.calculate_score()),
            failures = record.failures,
            "Reputation updated (failure)"
        );

        self.save_atomic(&records);
    }

    /// Prune nodes that haven't been seen for over MAX_STALE_AGE_SECS.
    pub fn prune_stale(&self) {
        let mut records = self.records.lock();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let before = records.len();
        records.retain(|_, v| now.saturating_sub(v.last_seen) < MAX_STALE_AGE_SECS);
        let pruned = before - records.len();

        if pruned > 0 {
            info!(pruned = pruned, remaining = records.len(), "Pruned stale reputation records");
            self.save_atomic(&records);
        }
    }

    /// Atomic file write: write to temp, fsync, then rename.
    fn save_atomic(&self, records: &HashMap<String, NodeReputation>) {
        let tmp_path = format!("{}.tmp", self.file_path);
        let serialized = match serde_json::to_string_pretty(records) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "Failed to serialize reputation state");
                return;
            }
        };

        let result = (|| -> std::io::Result<()> {
            let mut file = File::create(&tmp_path)?;
            file.write_all(serialized.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&tmp_path, &self.file_path)?;
            Ok(())
        })();

        if let Err(e) = result {
            error!(error = %e, "CRITICAL: Failed to persist reputation state");
            let _ = std::fs::remove_file(&tmp_path);
        }
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_node_default_score() {
        let node = NodeReputation::new("test".into());
        let score = node.calculate_score();
        assert!((score - DEFAULT_SCORE).abs() < 0.01, "New node should have default score");
    }

    #[test]
    fn test_score_increases_with_success() {
        let mut node = NodeReputation::new("test".into());
        node.successes = 10;
        node.failures = 0;
        node.total_latency_secs = 10.0;
        node.total_tokens_generated = 500;
        node.last_seen = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let score = node.calculate_score();
        assert!(score > DEFAULT_SCORE, "Good node should score above default");
    }

    #[test]
    fn test_score_decreases_with_failure() {
        let mut node = NodeReputation::new("test".into());
        node.successes = 1;
        node.failures = 10;
        node.total_latency_secs = 100.0;
        node.total_tokens_generated = 10;
        node.last_seen = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let score = node.calculate_score();
        assert!(score < DEFAULT_SCORE, "Bad node should score below default");
    }

    #[test]
    fn test_score_decays_over_time() {
        let mut node = NodeReputation::new("test".into());
        node.successes = 100;
        node.failures = 0;
        node.total_latency_secs = 100.0;
        node.total_tokens_generated = 5000;
        node.last_seen = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let fresh_score = node.calculate_score();

        // Simulate 7 days of inactivity
        node.last_seen -= 7 * 24 * 3600;
        let stale_score = node.calculate_score();

        assert!(
            stale_score < fresh_score,
            "Score should decay over time: fresh={}, stale={}",
            fresh_score, stale_score
        );
        assert!(
            (stale_score - DEFAULT_SCORE).abs() < (fresh_score - DEFAULT_SCORE).abs(),
            "Stale score should be closer to default"
        );
    }
}

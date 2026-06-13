use std::collections::HashMap;
use std::time::{Duration, Instant};
use parking_lot::Mutex;
use tracing::{debug, info};

struct NetworkStatsInner {
    tps_map: HashMap<String, f64>,
    last_seen: HashMap<String, Instant>,
    local_tps_history: Vec<(Instant, f64)>,
}

pub struct NetworkStats {
    inner: Mutex<NetworkStatsInner>,
    /// Stale threshold for keeping peer TPS reports (5 minutes)
    stale_duration: Duration,
}

impl Default for NetworkStats {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkStats {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(NetworkStatsInner {
                tps_map: HashMap::new(),
                last_seen: HashMap::new(),
                local_tps_history: Vec::new(),
            }),
            stale_duration: Duration::from_secs(300),
        }
    }

    pub fn with_stale_duration(duration: Duration) -> Self {
        Self {
            inner: Mutex::new(NetworkStatsInner {
                tps_map: HashMap::new(),
                last_seen: HashMap::new(),
                local_tps_history: Vec::new(),
            }),
            stale_duration: duration,
        }
    }

    /// Record a local inference run's TPS.
    pub fn record_local_inference(&self, tps: f64) {
        let mut inner = self.inner.lock();
        inner.local_tps_history.push((Instant::now(), tps));
        if inner.local_tps_history.len() > 10 {
            inner.local_tps_history.remove(0);
        }
        debug!(tps = tps, "Local inference TPS registered");
    }

    /// Get the local average TPS based on history.
    pub fn get_local_avg_tps(&self) -> f64 {
        let inner = self.inner.lock();
        if inner.local_tps_history.is_empty() {
            25.0
        } else {
            let sum: f64 = inner.local_tps_history.iter().map(|(_, tps)| *tps).sum();
            sum / inner.local_tps_history.len() as f64
        }
    }

    /// Feed an observed peer's average TPS from a Gossipsub health check broadcast.
    pub fn feed_heartbeat(&self, peer_id: &str, tps_avg: f64) {
        let now = Instant::now();
        let mut inner = self.inner.lock();
        inner.last_seen.insert(peer_id.to_string(), now);
        inner.tps_map.insert(peer_id.to_string(), tps_avg);
        debug!(peer_id = %peer_id, tps_avg = tps_avg, "TPS heartbeat registered");
    }

    /// Compute the median TPS of all active (non-stale) peers.
    /// If no active peers are found, returns the default fallback (25.0).
    pub fn get_median_tps(&self) -> f64 {
        let now = Instant::now();
        let mut inner = self.inner.lock();

        // Evict stale records
        let stale_dur = self.stale_duration;
        let mut stale_peers = Vec::new();
        for (peer, &instant) in &inner.last_seen {
            if now.duration_since(instant) >= stale_dur {
                stale_peers.push(peer.clone());
            }
        }

        for peer in &stale_peers {
            inner.last_seen.remove(peer);
            inner.tps_map.remove(peer);
            info!(peer_id = %peer, "Evicted stale node from median TPS calculation");
        }

        let mut values: Vec<f64> = inner.tps_map.values().copied().collect();
        if values.is_empty() {
            return 25.0; // Standard fallback
        }

        // Sort values to calculate median
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = values.len() / 2;
        if values.len().is_multiple_of(2) {
            (values[mid - 1] + values[mid]) / 2.0
        } else {
            values[mid]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_median() {
        let stats = NetworkStats::new();
        assert_eq!(stats.get_median_tps(), 25.0);
    }

    #[test]
    fn test_single_median() {
        let stats = NetworkStats::new();
        stats.feed_heartbeat("peer1", 50.0);
        assert_eq!(stats.get_median_tps(), 50.0);
    }

    #[test]
    fn test_odd_median() {
        let stats = NetworkStats::new();
        stats.feed_heartbeat("peer1", 10.0);
        stats.feed_heartbeat("peer2", 50.0);
        stats.feed_heartbeat("peer3", 30.0);
        assert_eq!(stats.get_median_tps(), 30.0);
    }

    #[test]
    fn test_even_median() {
        let stats = NetworkStats::new();
        stats.feed_heartbeat("peer1", 10.0);
        stats.feed_heartbeat("peer2", 20.0);
        stats.feed_heartbeat("peer3", 30.0);
        stats.feed_heartbeat("peer4", 40.0);
        // (20 + 30) / 2 = 25.0
        assert_eq!(stats.get_median_tps(), 25.0);
    }

    #[test]
    fn test_stale_eviction() {
        let stats = NetworkStats::with_stale_duration(Duration::from_millis(50));
        stats.feed_heartbeat("peer1", 50.0);
        assert_eq!(stats.get_median_tps(), 50.0);

        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(stats.get_median_tps(), 25.0); // Evicted, back to default
    }

    #[test]
    fn test_local_tps_history() {
        let stats = NetworkStats::new();
        assert_eq!(stats.get_local_avg_tps(), 25.0);
        stats.record_local_inference(10.0);
        stats.record_local_inference(20.0);
        assert_eq!(stats.get_local_avg_tps(), 15.0);
        for i in 0..15 {
            stats.record_local_inference(i as f64);
        }
        // Should keep last 10 entries: 5.0 to 14.0 -> average is 9.5
        assert_eq!(stats.get_local_avg_tps(), 9.5);
    }
}

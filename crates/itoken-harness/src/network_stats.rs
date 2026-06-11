use std::collections::HashMap;
use std::time::{Duration, Instant};
use parking_lot::Mutex;
use tracing::{debug, info};

pub struct NetworkStats {
    /// Peer ID -> Last reported average TPS
    tps_map: Mutex<HashMap<String, f64>>,
    /// Peer ID -> Time of last heartbeat received
    last_seen: Mutex<HashMap<String, Instant>>,
    /// Stale threshold for keeping peer TPS reports (5 minutes)
    stale_duration: Duration,
}

impl NetworkStats {
    pub fn new() -> Self {
        Self {
            tps_map: Mutex::new(HashMap::new()),
            last_seen: Mutex::new(HashMap::new()),
            stale_duration: Duration::from_secs(300),
        }
    }

    pub fn with_stale_duration(duration: Duration) -> Self {
        Self {
            tps_map: Mutex::new(HashMap::new()),
            last_seen: Mutex::new(HashMap::new()),
            stale_duration: duration,
        }
    }

    /// Feed an observed peer's average TPS from a Gossipsub health check broadcast.
    pub fn feed_heartbeat(&self, peer_id: &str, tps_avg: f64) {
        let now = Instant::now();
        {
            let mut last_seen = self.last_seen.lock();
            last_seen.insert(peer_id.to_string(), now);
        }
        {
            let mut tps_map = self.tps_map.lock();
            tps_map.insert(peer_id.to_string(), tps_avg);
        }
        debug!(peer_id = %peer_id, tps_avg = tps_avg, "TPS heartbeat registered");
    }

    /// Compute the median TPS of all active (non-stale) peers.
    /// If no active peers are found, returns the default fallback (25.0).
    pub fn get_median_tps(&self) -> f64 {
        let now = Instant::now();
        let mut last_seen = self.last_seen.lock();
        let mut tps_map = self.tps_map.lock();

        // Evict stale records
        let stale_dur = self.stale_duration;
        last_seen.retain(|peer, &mut instant| {
            let keep = now.duration_since(instant) < stale_dur;
            if !keep {
                tps_map.remove(peer);
                info!(peer_id = %peer, "Evicted stale node from median TPS calculation");
            }
            keep
        });

        let mut values: Vec<f64> = tps_map.values().copied().collect();
        if values.is_empty() {
            return 25.0; // Standard fallback
        }

        // Sort values to calculate median
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = values.len() / 2;
        if values.len() % 2 == 0 {
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
}

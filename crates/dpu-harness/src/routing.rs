use crate::reputation::ReputationDb;
use std::sync::Arc;

pub struct HarnessRouter {
    reputation_db: Arc<ReputationDb>,
}

impl HarnessRouter {
    pub fn new(reputation_db: Arc<ReputationDb>) -> Self {
        Self { reputation_db }
    }

    pub fn resolve_routing(
        &self,
        _model: &str,
        candidates: Vec<String>,
    ) -> Result<(String, Vec<String>), String> {
        if candidates.is_empty() {
            return Err("No active provider nodes available for this model".to_string());
        }

        let mut scored_candidates: Vec<(String, f64)> = candidates
            .into_iter()
            .map(|node_id| {
                let score = self.reputation_db.get_score(&node_id);
                (node_id, score)
            })
            .collect();

        // Sort by reputation score descending
        scored_candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let mut iter = scored_candidates.into_iter();
        let primary = iter.next().map(|(id, _)| id).unwrap();
        let backups: Vec<String> = iter.map(|(id, _)| id).collect();

        Ok((primary, backups))
    }
}

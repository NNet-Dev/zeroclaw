//! Query-time rerank machinery for recalled memory candidates.

use crate::importance::weighted_final_score;
use crate::traits::{MemoryCategory, MemoryEntry};
use chrono::{DateTime, Utc};
use std::collections::HashSet;

const DEFAULT_NEAR_DUPLICATE_THRESHOLD: f64 = 0.92;
const DEFAULT_IMPORTANCE_WEIGHT: f64 = 0.2;
const DEFAULT_RECENCY_WEIGHT: f64 = 0.1;

/// Advanced rerank strategy.
#[derive(Debug, Clone, PartialEq)]
pub enum RerankStrategy {
    None,
    Mmr { lambda: f64 },
}

/// Rerank stage configuration, materialized from canonical memory config.
#[derive(Debug, Clone)]
pub struct RerankConfig {
    pub strategy: RerankStrategy,
    pub threshold: usize,
    pub importance_weight: f64,
    pub recency_weight: f64,
    pub min_relevance_score: f64,
    pub final_limit: usize,
}

impl RerankConfig {
    pub fn disabled(final_limit: usize, min_relevance_score: f64) -> Self {
        Self {
            strategy: RerankStrategy::None,
            threshold: usize::MAX,
            importance_weight: DEFAULT_IMPORTANCE_WEIGHT,
            recency_weight: DEFAULT_RECENCY_WEIGHT,
            min_relevance_score,
            final_limit: final_limit.max(1),
        }
    }
}

/// Run blend, duplicate collapse, optional advanced rerank, threshold, and trim.
pub fn run(mut pool: Vec<MemoryEntry>, config: &RerankConfig) -> Vec<MemoryEntry> {
    for entry in &mut pool {
        let Some(hybrid_score) = entry.score else {
            continue;
        };
        let hybrid_score = hybrid_score.clamp(0.0, 1.0);
        let importance = entry.importance.unwrap_or(0.0).clamp(0.0, 1.0);
        let recency = recency_factor(entry).clamp(0.0, 1.0);
        entry.score = Some(blended_score(
            hybrid_score,
            importance,
            recency,
            config.importance_weight,
            config.recency_weight,
        ));
    }

    let mut candidates = collapse_exact_and_near_duplicates(pool, DEFAULT_NEAR_DUPLICATE_THRESHOLD);
    sort_by_score(&mut candidates);

    if candidates.len() >= config.threshold {
        candidates = match config.strategy {
            RerankStrategy::None => candidates,
            RerankStrategy::Mmr { lambda } => mmr_rerank(candidates, lambda),
        };
    }

    candidates.retain(|entry| {
        entry
            .score
            .is_none_or(|score| score >= config.min_relevance_score)
    });
    candidates.truncate(config.final_limit);
    candidates
}

/// Collapse exact and near-duplicate entries, preserving the highest score.
pub fn collapse_exact_and_near_duplicates(
    mut entries: Vec<MemoryEntry>,
    near_duplicate_threshold: f64,
) -> Vec<MemoryEntry> {
    sort_by_score(&mut entries);
    let mut kept: Vec<MemoryEntry> = Vec::new();
    let mut exact_contents: HashSet<String> = HashSet::new();

    'entry: for entry in entries {
        let normalized = normalize_content(&entry.content);
        if !exact_contents.insert(normalized.clone()) {
            continue;
        }
        for kept_entry in &kept {
            if lexical_similarity(&normalized, &normalize_content(&kept_entry.content))
                >= near_duplicate_threshold
            {
                continue 'entry;
            }
        }
        kept.push(entry);
    }

    kept
}

fn blended_score(
    hybrid_score: f64,
    importance: f64,
    recency: f64,
    importance_weight: f64,
    recency_weight: f64,
) -> f64 {
    if (importance_weight - DEFAULT_IMPORTANCE_WEIGHT).abs() < f64::EPSILON
        && (recency_weight - DEFAULT_RECENCY_WEIGHT).abs() < f64::EPSILON
    {
        return weighted_final_score(hybrid_score, importance, recency).clamp(0.0, 1.0);
    }

    let importance_weight = importance_weight.clamp(0.0, 1.0);
    let recency_weight = recency_weight.clamp(0.0, 1.0);
    let retrieval_weight = (1.0 - importance_weight - recency_weight).max(0.0);
    let total = retrieval_weight + importance_weight + recency_weight;
    if total < f64::EPSILON {
        return hybrid_score;
    }

    ((hybrid_score * retrieval_weight)
        + (importance * importance_weight)
        + (recency * recency_weight))
        / total
}

fn recency_factor(entry: &MemoryEntry) -> f64 {
    if entry.category == MemoryCategory::Core {
        return 1.0;
    }

    let Ok(timestamp) = DateTime::parse_from_rfc3339(&entry.timestamp) else {
        return 1.0;
    };
    let age_days = Utc::now()
        .signed_duration_since(timestamp.with_timezone(&Utc))
        .num_seconds()
        .max(0) as f64
        / 86_400.0;

    (-age_days / crate::decay::DEFAULT_HALF_LIFE_DAYS * std::f64::consts::LN_2).exp()
}

fn mmr_rerank(mut candidates: Vec<MemoryEntry>, lambda: f64) -> Vec<MemoryEntry> {
    if candidates.len() <= 1 {
        return candidates;
    }

    let lambda = lambda.clamp(0.0, 1.0);
    let mut selected: Vec<MemoryEntry> = Vec::with_capacity(candidates.len());

    while !candidates.is_empty() {
        let best_index = candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                let relevance = candidate.score.unwrap_or(0.0);
                let redundancy = selected
                    .iter()
                    .map(|selected| lexical_similarity(&candidate.content, &selected.content))
                    .fold(0.0_f64, f64::max);
                let mmr_score = lambda * relevance - (1.0 - lambda) * redundancy;
                (index, mmr_score)
            })
            .max_by(|(_, left), (_, right)| {
                left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(index, _)| index)
            .unwrap_or(0);

        selected.push(candidates.remove(best_index));
    }

    selected
}

fn sort_by_score(entries: &mut [MemoryEntry]) {
    entries.sort_by(|left, right| {
        right
            .score
            .unwrap_or(0.0)
            .partial_cmp(&left.score.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.key.cmp(&right.key))
    });
}

fn normalize_content(content: &str) -> String {
    content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn lexical_similarity(left: &str, right: &str) -> f64 {
    let left_tokens: HashSet<&str> = left.split_whitespace().collect();
    let right_tokens: HashSet<&str> = right.split_whitespace().collect();
    if left_tokens.is_empty() || right_tokens.is_empty() {
        return 0.0;
    }
    let intersection = left_tokens.intersection(&right_tokens).count() as f64;
    let union = left_tokens.union(&right_tokens).count() as f64;
    if union < f64::EPSILON {
        0.0
    } else {
        intersection / union
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn entry(id: &str, content: &str, score: f64, importance: f64) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            key: id.into(),
            content: content.into(),
            category: MemoryCategory::Daily,
            timestamp: Utc::now().to_rfc3339(),
            session_id: None,
            score: Some(score),
            namespace: "default".into(),
            importance: Some(importance),
            superseded_by: None,
            kind: None,
            pinned: false,
            tenant_id: None,
            agent_alias: None,
            agent_id: None,
        }
    }

    fn config(strategy: RerankStrategy) -> RerankConfig {
        RerankConfig {
            strategy,
            threshold: 1,
            importance_weight: DEFAULT_IMPORTANCE_WEIGHT,
            recency_weight: DEFAULT_RECENCY_WEIGHT,
            min_relevance_score: 0.0,
            final_limit: 10,
        }
    }

    #[test]
    fn run_blends_and_sorts_with_weighted_final_score() {
        let results = run(
            vec![
                entry("low", "lower", 0.7, 0.0),
                entry("high", "higher", 0.6, 1.0),
            ],
            &config(RerankStrategy::None),
        );

        assert_eq!(results[0].key, "high");
        let expected = weighted_final_score(0.6, 1.0, 1.0);
        assert!((results[0].score.unwrap() - expected).abs() < 0.001);
    }

    #[test]
    fn threshold_applies_after_blend() {
        let mut cfg = config(RerankStrategy::None);
        cfg.min_relevance_score = 0.8;
        let results = run(
            vec![
                entry("drop", "drop this", 0.2, 0.0),
                entry("keep", "keep this", 0.9, 1.0),
            ],
            &cfg,
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "keep");
    }

    #[test]
    fn exact_duplicate_content_collapses() {
        let results = collapse_exact_and_near_duplicates(
            vec![
                entry("a", "same content", 0.9, 0.0),
                entry("b", "same   content", 0.8, 0.0),
            ],
            DEFAULT_NEAR_DUPLICATE_THRESHOLD,
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "a");
    }

    #[test]
    fn near_duplicate_content_collapses() {
        let results = collapse_exact_and_near_duplicates(
            vec![
                entry("a", "alpha beta gamma delta epsilon", 0.9, 0.0),
                entry("b", "alpha beta gamma delta zeta", 0.8, 0.0),
            ],
            0.6,
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].key, "a");
    }

    #[test]
    fn mmr_diversifies_after_highest_relevance_item() {
        let mut cfg = config(RerankStrategy::Mmr { lambda: 0.5 });
        cfg.final_limit = 3;
        let results = run(
            vec![
                entry("a", "rust memory scoring pipeline", 1.0, 0.0),
                entry("b", "rust memory scoring pipeline duplicate", 0.98, 0.0),
                entry("c", "garden tomatoes irrigation calendar", 0.8, 0.0),
            ],
            &cfg,
        );

        assert_eq!(results[0].key, "a");
        assert_eq!(results[1].key, "c");
    }

    #[test]
    fn old_entries_get_lower_recency_signal() {
        let mut old = entry("old", "old", 1.0, 0.0);
        old.timestamp = (Utc::now() - Duration::days(7)).to_rfc3339();
        assert!(recency_factor(&old) < 0.6);
    }
}

//! Score normalization and fusion helpers for recall candidates.

use crate::vector::ScoredResult;
use std::collections::HashMap;

/// Normalize a batch of raw BM25-style scores to a bounded [0, 1] axis.
///
/// Scores are assumed to be higher-is-better by the time they reach this
/// helper. Empty batches return empty; all-zero batches return zero scores.
pub fn bm25_to_unit(raw: &[(String, f32)]) -> Vec<(String, f32)> {
    let max_score = raw.iter().map(|(_, score)| *score).fold(0.0_f32, f32::max);

    if max_score < f32::EPSILON {
        return raw.iter().map(|(id, _)| (id.clone(), 0.0)).collect();
    }

    raw.iter()
        .map(|(id, score)| (id.clone(), (*score / max_score).clamp(0.0, 1.0)))
        .collect()
}

/// Normalize, fuse, sort, and truncate vector and keyword candidate IDs.
///
/// When only one source is present, that source is normalized onto the full
/// [0, 1] axis instead of being capped by an inactive source's configured
/// weight.
pub fn normalize_and_fuse(
    vector_results: &[(String, f32)],
    keyword_results: &[(String, f32)],
    vector_weight: f32,
    keyword_weight: f32,
    limit: usize,
) -> Vec<ScoredResult> {
    let mut map: HashMap<String, ScoredResult> = HashMap::new();

    for (id, score) in vector_results {
        map.entry(id.clone())
            .and_modify(|result| result.vector_score = Some(score.clamp(0.0, 1.0)))
            .or_insert_with(|| ScoredResult {
                id: id.clone(),
                vector_score: Some(score.clamp(0.0, 1.0)),
                keyword_score: None,
                final_score: 0.0,
            });
    }

    for (id, score) in bm25_to_unit(keyword_results) {
        map.entry(id.clone())
            .and_modify(|result| result.keyword_score = Some(score))
            .or_insert_with(|| ScoredResult {
                id,
                vector_score: None,
                keyword_score: Some(score),
                final_score: 0.0,
            });
    }

    let has_vector = !vector_results.is_empty();
    let has_keyword = !keyword_results.is_empty();
    let (effective_vector_weight, effective_keyword_weight) = match (has_vector, has_keyword) {
        (true, true) => {
            let total = vector_weight + keyword_weight;
            if total > f32::EPSILON {
                (vector_weight / total, keyword_weight / total)
            } else {
                (0.0, 0.0)
            }
        }
        (true, false) if vector_weight > f32::EPSILON => (1.0, 0.0),
        (false, true) if keyword_weight > f32::EPSILON => (0.0, 1.0),
        (true, false) | (false, true) => (0.0, 0.0),
        (false, false) => (0.0, 0.0),
    };

    let mut results: Vec<ScoredResult> = map
        .into_values()
        .map(|mut result| {
            let vector_score = result.vector_score.unwrap_or(0.0);
            let keyword_score = result.keyword_score.unwrap_or(0.0);
            result.final_score = (effective_vector_weight * vector_score
                + effective_keyword_weight * keyword_score)
                .clamp(0.0, 1.0);
            result
        })
        .collect();

    results.sort_by(|a, b| {
        b.final_score
            .partial_cmp(&a.final_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    results.truncate(limit);
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bm25_to_unit_maps_best_score_to_one() {
        let normalized = bm25_to_unit(&[("a".into(), 2.0), ("b".into(), 4.0), ("c".into(), 1.0)]);

        assert_eq!(normalized[0], ("a".into(), 0.5));
        assert_eq!(normalized[1], ("b".into(), 1.0));
        assert_eq!(normalized[2], ("c".into(), 0.25));
    }

    #[test]
    fn bm25_to_unit_handles_empty_and_zero_batches() {
        assert!(bm25_to_unit(&[]).is_empty());
        assert_eq!(bm25_to_unit(&[("a".into(), 0.0)]), vec![("a".into(), 0.0)]);
    }

    #[test]
    fn normalize_and_fuse_keeps_both_source_weighting() {
        let fused = normalize_and_fuse(
            &[("a".into(), 0.5), ("b".into(), 0.9)],
            &[("a".into(), 10.0), ("c".into(), 5.0)],
            0.7,
            0.3,
            10,
        );

        let a = fused.iter().find(|result| result.id == "a").unwrap();
        assert!((a.final_score - 0.65).abs() < 0.001);
        assert_eq!(a.vector_score, Some(0.5));
        assert_eq!(a.keyword_score, Some(1.0));
    }

    #[test]
    fn normalize_and_fuse_single_source_uses_full_axis() {
        let fused = normalize_and_fuse(&[], &[("x".into(), 10.0), ("y".into(), 5.0)], 0.7, 0.3, 10);

        assert_eq!(fused[0].id, "x");
        assert_eq!(fused[0].final_score, 1.0);
        assert_eq!(fused[1].final_score, 0.5);
    }
}

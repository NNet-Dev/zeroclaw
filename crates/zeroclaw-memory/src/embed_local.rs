//! Feature-gated local embedding provider.
//!
//! This is dependency-light machinery for the S1B feature gate. It gives
//! operators a local deterministic vector path without network calls while the
//! heavier bundled-model decision remains blocked on license, size, and
//! platform validation.

use crate::embeddings::EmbeddingProvider;
use async_trait::async_trait;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

const MIN_DIMENSIONS: usize = 16;

#[derive(Debug, Clone)]
pub struct LocalEmbedding {
    dims: usize,
}

impl LocalEmbedding {
    pub fn load(dims: usize) -> anyhow::Result<Self> {
        if dims < MIN_DIMENSIONS {
            anyhow::bail!(
                "local embedding dimensions must be at least {MIN_DIMENSIONS}; got {dims}"
            );
        }

        Ok(Self { dims })
    }
}

#[async_trait]
impl EmbeddingProvider for LocalEmbedding {
    fn name(&self) -> &str {
        "local"
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| hashed_text_embedding(text, self.dims))
            .collect())
    }
}

fn hashed_text_embedding(text: &str, dims: usize) -> Vec<f32> {
    let mut vector = vec![0.0_f32; dims];
    for token in text.split_whitespace().filter(|token| !token.is_empty()) {
        let normalized = token.to_ascii_lowercase();
        add_feature(&mut vector, &normalized, 1.0);

        for window in normalized.as_bytes().windows(3) {
            if let Ok(ngram) = std::str::from_utf8(window) {
                add_feature(&mut vector, ngram, 0.35);
            }
        }
    }

    normalize_l2(&mut vector);
    vector
}

fn add_feature(vector: &mut [f32], feature: &str, weight: f32) {
    if vector.is_empty() {
        return;
    }

    let mut hasher = DefaultHasher::new();
    feature.hash(&mut hasher);
    let hash = hasher.finish();
    let index = (hash as usize) % vector.len();
    let sign = if hash & 1 == 0 { 1.0 } else { -1.0 };
    vector[index] += sign * weight;
}

fn normalize_l2(vector: &mut [f32]) {
    let norm = vector
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    if norm <= f64::EPSILON || !norm.is_finite() {
        return;
    }

    for value in vector {
        *value = (f64::from(*value) / norm) as f32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_rejects_invalid_dimensions() {
        assert!(LocalEmbedding::load(0).is_err());
        assert!(LocalEmbedding::load(MIN_DIMENSIONS - 1).is_err());
    }

    #[tokio::test]
    async fn embed_returns_deterministic_non_empty_vectors() {
        let provider = LocalEmbedding::load(64).unwrap();
        let first = provider
            .embed_one("memory recall semantic test")
            .await
            .unwrap();
        let second = provider
            .embed_one("memory recall semantic test")
            .await
            .unwrap();

        assert_eq!(first.len(), 64);
        assert_eq!(first, second);
        assert!(first.iter().any(|value| value.abs() > f32::EPSILON));
    }

    #[tokio::test]
    async fn related_texts_have_positive_similarity() {
        let provider = LocalEmbedding::load(128).unwrap();
        let first = provider.embed_one("github polling channel").await.unwrap();
        let second = provider.embed_one("polling github channel").await.unwrap();

        assert!(crate::vector::cosine_similarity(&first, &second) > 0.5);
    }
}

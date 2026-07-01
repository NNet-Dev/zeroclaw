//! Knowledge graph recall candidates for the memory recall pipeline.

use crate::kind_bridge;
use crate::knowledge_graph::{KnowledgeGraph, KnowledgeNode};
use crate::normalize;
use crate::traits::{MemoryCategory, MemoryEntry, MemoryKind};
use chrono::Utc;
use std::collections::HashMap;

const DEFAULT_KG_RELEVANCE_FLOOR: f64 = 0.05;
const KNOWLEDGE_NAMESPACE: &str = "knowledge";

/// A typed KG candidate ready to fuse through the recall pipeline.
#[derive(Debug, Clone)]
pub struct KgCandidate {
    pub id: String,
    pub content: String,
    pub similarity: f64,
    pub kind: MemoryKind,
    pub source_project: Option<String>,
    pub timestamp: String,
}

/// Query the KG for the turn's user message.
///
/// KG recall is additive. An empty or failing KG query returns no candidates
/// instead of failing the surrounding memory recall path.
pub fn query(kg: &KnowledgeGraph, user_message: &str, k: usize) -> Vec<KgCandidate> {
    if k == 0 {
        return Vec::new();
    }

    let mut candidates = match kg.query_by_similarity(user_message, k) {
        Ok(results) => results
            .into_iter()
            .filter(|result| result.score >= DEFAULT_KG_RELEVANCE_FLOOR)
            .map(|result| candidate_from_node(result.node, result.score))
            .collect::<Vec<_>>(),
        Err(e) => {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "knowledge graph recall query failed"
            );
            Vec::new()
        }
    };

    if candidates.is_empty() {
        candidates = query_by_lexical_tags(kg, user_message, k);
    }

    candidates.truncate(k);
    candidates
}

/// Fuse existing memory entries and KG candidates through the shared score
/// normalization seam, preserving source metadata on the resulting entries.
pub fn fuse_with_memory(
    memory_entries: Vec<MemoryEntry>,
    kg_candidates: Vec<KgCandidate>,
    vector_weight: f32,
    keyword_weight: f32,
    limit: usize,
) -> Vec<MemoryEntry> {
    if kg_candidates.is_empty() {
        return memory_entries;
    }

    let mut by_id: HashMap<String, MemoryEntry> = HashMap::new();
    let memory_scores = memory_entries
        .into_iter()
        .map(|entry| {
            let score = entry.score.unwrap_or(0.0) as f32;
            let id = entry.id.clone();
            by_id.insert(id.clone(), entry);
            (id, score)
        })
        .collect::<Vec<_>>();

    let kg_scores = kg_candidates
        .into_iter()
        .map(|candidate| {
            let score = candidate.similarity as f32;
            let id = candidate.id.clone();
            by_id.insert(id.clone(), candidate.into_memory_entry());
            (id, score)
        })
        .collect::<Vec<_>>();

    normalize::normalize_and_fuse(
        &memory_scores,
        &kg_scores,
        vector_weight,
        keyword_weight,
        limit.max(1),
    )
    .into_iter()
    .filter_map(|result| {
        by_id.remove(&result.id).map(|mut entry| {
            entry.score = Some(result.final_score as f64);
            entry
        })
    })
    .collect()
}

impl KgCandidate {
    fn into_memory_entry(self) -> MemoryEntry {
        MemoryEntry {
            id: self.id.clone(),
            key: self.id,
            content: self.content,
            category: MemoryCategory::Core,
            timestamp: self.timestamp,
            session_id: None,
            score: Some(self.similarity),
            namespace: KNOWLEDGE_NAMESPACE.into(),
            importance: None,
            superseded_by: None,
            kind: Some(self.kind),
            pinned: false,
            tenant_id: None,
            agent_alias: None,
            agent_id: None,
        }
    }
}

fn candidate_from_node(node: KnowledgeNode, score: f64) -> KgCandidate {
    let id = kind_bridge::node_source_key(&node.id);
    let content = match node.source_project.as_deref() {
        Some(source) if !source.trim().is_empty() => {
            format!(
                "[knowledge:{} source={}] {}: {}",
                node.node_type.as_str(),
                source,
                node.title,
                node.content
            )
        }
        _ => format!(
            "[knowledge:{}] {}: {}",
            node.node_type.as_str(),
            node.title,
            node.content
        ),
    };

    KgCandidate {
        id,
        content,
        similarity: score,
        kind: kind_bridge::node_type_to_memory_kind(node.node_type),
        source_project: node.source_project,
        timestamp: node.updated_at.to_rfc3339(),
    }
}

fn query_by_lexical_tags(kg: &KnowledgeGraph, user_message: &str, k: usize) -> Vec<KgCandidate> {
    let tags = lexical_tags(user_message);
    if tags.is_empty() {
        return Vec::new();
    }

    match kg.query_by_tags(&tags) {
        Ok(nodes) => nodes
            .into_iter()
            .take(k)
            .map(|node| candidate_from_node(node, 1.0))
            .collect(),
        Err(e) => {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "knowledge graph tag recall query failed"
            );
            Vec::new()
        }
    }
}

fn lexical_tags(text: &str) -> Vec<String> {
    let mut tags = text
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter_map(|token| {
            let token = token.trim().to_ascii_lowercase();
            (token.len() >= 3).then_some(token)
        })
        .collect::<Vec<_>>();
    tags.sort();
    tags.dedup();
    tags.truncate(3);
    tags
}

impl Default for KgCandidate {
    fn default() -> Self {
        Self {
            id: "kg:default".into(),
            content: String::new(),
            similarity: 0.0,
            kind: MemoryKind::Semantic(crate::traits::SemanticSubtype::Fact),
            source_project: None,
            timestamp: Utc::now().to_rfc3339(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge_graph::NodeType;
    use tempfile::tempdir;

    #[test]
    fn query_returns_empty_for_empty_graph() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::new(&dir.path().join("kg.db"), 100).unwrap();

        assert!(query(&kg, "missing topic", 5).is_empty());
    }

    #[test]
    fn query_returns_typed_candidates() {
        let dir = tempdir().unwrap();
        let kg = KnowledgeGraph::new(&dir.path().join("kg.db"), 100).unwrap();
        kg.add_node(
            NodeType::Decision,
            "Deploy policy",
            "Deploys require approval",
            &["deploy".into(), "approval".into()],
            Some("ops"),
        )
        .unwrap();

        let candidates = query(&kg, "deploy approval", 5);
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].kind,
            MemoryKind::Semantic(crate::traits::SemanticSubtype::Decision)
        );
        assert!(candidates[0].content.contains("source=ops"));
    }

    #[test]
    fn fuse_uses_shared_normalization_and_preserves_kg_entry() {
        let memory = MemoryEntry {
            id: "m1".into(),
            key: "m1".into(),
            content: "memory".into(),
            category: MemoryCategory::Core,
            timestamp: "now".into(),
            session_id: None,
            score: Some(0.5),
            namespace: "default".into(),
            importance: None,
            superseded_by: None,
            kind: None,
            pinned: false,
            tenant_id: None,
            agent_alias: None,
            agent_id: None,
        };
        let kg = KgCandidate {
            id: "kg:1".into(),
            content: "knowledge".into(),
            similarity: 10.0,
            ..KgCandidate::default()
        };

        let fused = fuse_with_memory(vec![memory], vec![kg], 0.7, 0.3, 10);
        assert_eq!(fused.len(), 2);
        let kg = fused.iter().find(|entry| entry.key == "kg:1").unwrap();
        assert_eq!(kg.namespace, KNOWLEDGE_NAMESPACE);
        assert!(kg.score.unwrap() > 0.0);
    }
}

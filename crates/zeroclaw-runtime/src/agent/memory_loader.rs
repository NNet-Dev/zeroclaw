use async_trait::async_trait;
use std::fmt::Write;
use std::sync::Arc;
use std::time::Instant;
use zeroclaw_memory::{
    self, MEMORY_CONTEXT_CLOSE, MEMORY_CONTEXT_OPEN, Memory, RerankConfig, decay,
    knowledge_graph::KnowledgeGraph, rerank,
};

use crate::observability::{Observer, ObserverEvent};

use super::loop_::make_query_summary;

#[async_trait]
pub trait MemoryLoader: Send + Sync {
    /// Loads a memory-context preamble for a user message.
    ///
    /// Implementations MUST emit a `ObserverEvent::MemoryRecall` event via
    /// `observer` for every recall call they perform — both on success and
    /// failure paths — so OTel/log observers can attribute per-turn memory
    /// cost. The agent runtime relies on this for end-to-end visibility
    /// of the implicit recall that runs at the start of each turn.
    async fn load_context(
        &self,
        memory: &dyn Memory,
        observer: &dyn Observer,
        user_message: &str,
        session_id: Option<&str>,
    ) -> anyhow::Result<String>;
}

pub struct DefaultMemoryLoader {
    limit: usize,
    min_relevance_score: f64,
    candidate_multiplier: usize,
    rerank_enabled: bool,
    rerank_config: RerankConfig,
    kg_graph: Option<Arc<KnowledgeGraph>>,
    fuse_kg_into_recall: bool,
    vector_weight: f32,
    keyword_weight: f32,
}

impl Default for DefaultMemoryLoader {
    fn default() -> Self {
        let limit = 5;
        let min_relevance_score = 0.4;
        Self {
            limit,
            min_relevance_score,
            candidate_multiplier: 1,
            rerank_enabled: false,
            rerank_config: RerankConfig::disabled(limit, min_relevance_score),
            kg_graph: None,
            fuse_kg_into_recall: false,
            vector_weight: 0.7,
            keyword_weight: 0.3,
        }
    }
}

impl DefaultMemoryLoader {
    pub fn new(limit: usize, min_relevance_score: f64) -> Self {
        let limit = limit.max(1);
        Self {
            limit,
            min_relevance_score,
            candidate_multiplier: 1,
            rerank_enabled: false,
            rerank_config: RerankConfig::disabled(limit, min_relevance_score),
            kg_graph: None,
            fuse_kg_into_recall: false,
            vector_weight: 0.7,
            keyword_weight: 0.3,
        }
    }

    /// Build a loader with the relevance-plane knobs resolved from memory config.
    ///
    /// When `rerank_enabled` is false this behaves exactly like [`Self::new`]:
    /// no candidate over-fetch and no rerank stage, so recall stays
    /// byte-identical to the pre-relevance loader. When enabled, the loader
    /// over-fetches `limit * candidate_multiplier` candidates and runs the
    /// blend/dedup/MMR/threshold rerank, trimming back to `limit`.
    pub fn with_relevance_config(
        limit: usize,
        min_relevance_score: f64,
        candidate_multiplier: usize,
        rerank_enabled: bool,
        rerank_config: RerankConfig,
    ) -> Self {
        Self {
            limit: limit.max(1),
            min_relevance_score,
            candidate_multiplier: candidate_multiplier.max(1),
            rerank_enabled,
            rerank_config,
            kg_graph: None,
            fuse_kg_into_recall: false,
            vector_weight: 0.7,
            keyword_weight: 0.3,
        }
    }

    /// Attach optional read-only knowledge-graph recall fusion. When
    /// `fuse_kg_into_recall` is false (default) or no graph is present, recall is
    /// unaffected; fusing an empty graph is an identity operation.
    pub fn with_kg_recall(
        mut self,
        kg_graph: Option<Arc<KnowledgeGraph>>,
        fuse_kg_into_recall: bool,
        vector_weight: f32,
        keyword_weight: f32,
    ) -> Self {
        self.kg_graph = kg_graph;
        self.fuse_kg_into_recall = fuse_kg_into_recall;
        self.vector_weight = vector_weight;
        self.keyword_weight = keyword_weight;
        self
    }
}

#[async_trait]
impl MemoryLoader for DefaultMemoryLoader {
    async fn load_context(
        &self,
        memory: &dyn Memory,
        observer: &dyn Observer,
        user_message: &str,
        session_id: Option<&str>,
    ) -> anyhow::Result<String> {
        let backend = memory.name().to_string();
        let query_summary = make_query_summary(user_message);

        let start = Instant::now();
        // Over-fetch a larger candidate pool only when reranking is enabled;
        // otherwise recall exactly `limit` so the default path is unchanged.
        let pool_limit = if self.rerank_enabled {
            self.limit
                .saturating_mul(self.candidate_multiplier)
                .max(self.limit)
        } else {
            self.limit
        };
        let recall_result = memory
            .recall(user_message, pool_limit, session_id, None, None)
            .await;
        let duration = start.elapsed();

        let mut entries = match recall_result {
            Ok(entries) => {
                observer.record_event(&ObserverEvent::MemoryRecall {
                    query_summary,
                    duration,
                    num_entries: entries.len(),
                    backend,
                    success: true,
                });
                entries
            }
            Err(e) => {
                observer.record_event(&ObserverEvent::MemoryRecall {
                    query_summary,
                    duration,
                    num_entries: 0,
                    backend,
                    success: false,
                });
                return Err(e);
            }
        };

        // Read-only knowledge-graph recall fusion (default off; fusing an empty
        // graph is an identity operation). Runs before the rerank/decay stage.
        if self.fuse_kg_into_recall
            && let Some(kg) = self.kg_graph.as_deref()
        {
            let kg_candidates = zeroclaw_memory::kg_recall::query(kg, user_message, pool_limit);
            entries = zeroclaw_memory::kg_recall::fuse_with_memory(
                entries,
                kg_candidates,
                self.vector_weight,
                self.keyword_weight,
                pool_limit,
            );
        }

        if entries.is_empty() {
            return Ok(String::new());
        }

        if self.rerank_enabled {
            // Relevance plane: blend (retrieval + importance + recency), collapse
            // near-duplicates, optionally diversify via MMR, apply the threshold,
            // and trim back to the recall limit.
            entries = rerank::run(entries, &self.rerank_config);
        } else {
            // Behaviour-neutral default path: apply time decay and let the render
            // loop below handle min-relevance filtering, identical to the
            // pre-relevance loader.
            decay::apply_time_decay(&mut entries, decay::DEFAULT_HALF_LIFE_DAYS);
        }

        let mut context = String::new();
        let mut included = false;
        for entry in entries {
            if zeroclaw_memory::is_assistant_autosave_key(&entry.key) {
                continue;
            }
            if zeroclaw_memory::is_user_autosave_key(&entry.key) {
                continue;
            }
            if zeroclaw_memory::should_skip_autosave_content(&entry.content) {
                continue;
            }
            if let Some(score) = entry.score
                && score < self.min_relevance_score
            {
                continue;
            }
            if !included {
                context.push_str(MEMORY_CONTEXT_OPEN);
                context.push('\n');
                included = true;
            }
            let _ = writeln!(context, "- {}: {}", entry.key, entry.content);
        }

        // If all entries were below threshold, return empty
        if !included {
            return Ok(String::new());
        }

        context.push_str(MEMORY_CONTEXT_CLOSE);
        context.push_str("\n\n");
        Ok(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::NoopObserver;
    use std::sync::Arc;
    use zeroclaw_memory::{
        MEMORY_CONTEXT_CLOSE, MEMORY_CONTEXT_OPEN, Memory, MemoryCategory, MemoryEntry,
        RerankConfig, RerankStrategy,
    };

    struct MockMemory;
    struct MockMemoryWithEntries {
        entries: Arc<Vec<MemoryEntry>>,
    }

    #[async_trait]
    impl Memory for MockMemory {
        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            limit: usize,
            _session_id: Option<&str>,
            _since: Option<&str>,
            _until: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            if limit == 0 {
                return Ok(vec![]);
            }
            Ok(vec![MemoryEntry {
                id: "1".into(),
                key: "k".into(),
                content: "v".into(),
                category: MemoryCategory::Conversation,
                timestamp: "now".into(),
                session_id: None,
                score: None,
                namespace: "default".into(),
                importance: None,
                superseded_by: None,
                kind: None,
                pinned: false,
                tenant_id: None,
                agent_alias: None,
                agent_id: None,
            }])
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(vec![])
        }

        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(true)
        }

        async fn forget_for_agent(&self, _key: &str, _agent_id: &str) -> anyhow::Result<bool> {
            Ok(true)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }

        async fn health_check(&self) -> bool {
            true
        }

        fn name(&self) -> &str {
            "mock"
        }

        async fn store_with_agent(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
            _namespace: Option<&str>,
            _importance: Option<f64>,
            _agent_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall_for_agents(
            &self,
            _allowed_agent_ids: &[&str],
            query: &str,
            limit: usize,
            session_id: Option<&str>,
            since: Option<&str>,
            until: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            self.recall(query, limit, session_id, since, until).await
        }
    }
    impl ::zeroclaw_api::attribution::Attributable for MockMemory {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Memory(
                ::zeroclaw_api::attribution::MemoryKind::InMemory,
            )
        }
        fn alias(&self) -> &str {
            "MockMemory"
        }
    }

    #[async_trait]
    impl Memory for MockMemoryWithEntries {
        async fn store(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
            _since: Option<&str>,
            _until: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(self.entries.as_ref().clone())
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(vec![])
        }

        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(true)
        }

        async fn forget_for_agent(&self, _key: &str, _agent_id: &str) -> anyhow::Result<bool> {
            Ok(true)
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(self.entries.len())
        }

        async fn health_check(&self) -> bool {
            true
        }

        fn name(&self) -> &str {
            "mock-with-entries"
        }

        async fn store_with_agent(
            &self,
            _key: &str,
            _content: &str,
            _category: MemoryCategory,
            _session_id: Option<&str>,
            _namespace: Option<&str>,
            _importance: Option<f64>,
            _agent_id: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn recall_for_agents(
            &self,
            _allowed_agent_ids: &[&str],
            query: &str,
            limit: usize,
            session_id: Option<&str>,
            since: Option<&str>,
            until: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            self.recall(query, limit, session_id, since, until).await
        }
    }
    impl ::zeroclaw_api::attribution::Attributable for MockMemoryWithEntries {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Memory(
                ::zeroclaw_api::attribution::MemoryKind::InMemory,
            )
        }
        fn alias(&self) -> &str {
            "MockMemoryWithEntries"
        }
    }

    #[tokio::test]
    async fn default_loader_formats_context() {
        let loader = DefaultMemoryLoader::default();
        let context = loader
            .load_context(&MockMemory, &NoopObserver, "hello", None)
            .await
            .unwrap();
        assert_eq!(
            context,
            format!("{MEMORY_CONTEXT_OPEN}\n- k: v\n{MEMORY_CONTEXT_CLOSE}\n\n")
        );
    }

    #[tokio::test]
    async fn default_loader_skips_legacy_assistant_autosave_entries() {
        let loader = DefaultMemoryLoader::new(5, 0.0);
        let memory = MockMemoryWithEntries {
            entries: Arc::new(vec![
                MemoryEntry {
                    id: "1".into(),
                    key: "assistant_resp_legacy".into(),
                    content: "fabricated detail".into(),
                    category: MemoryCategory::Daily,
                    timestamp: "now".into(),
                    session_id: None,
                    score: Some(0.95),
                    namespace: "default".into(),
                    importance: None,
                    superseded_by: None,
                    kind: None,
                    pinned: false,
                    tenant_id: None,
                    agent_alias: None,
                    agent_id: None,
                },
                MemoryEntry {
                    id: "2".into(),
                    key: "user_fact".into(),
                    content: "User prefers concise answers".into(),
                    category: MemoryCategory::Conversation,
                    timestamp: "now".into(),
                    session_id: None,
                    score: Some(0.9),
                    namespace: "default".into(),
                    importance: None,
                    superseded_by: None,
                    kind: None,
                    pinned: false,
                    tenant_id: None,
                    agent_alias: None,
                    agent_id: None,
                },
            ]),
        };

        let context = loader
            .load_context(&memory, &NoopObserver, "answer style", None)
            .await
            .unwrap();
        assert!(context.contains("user_fact"));
        assert!(!context.contains("assistant_resp_legacy"));
        assert!(!context.contains("fabricated detail"));
    }

    #[tokio::test]
    async fn default_loader_skips_user_autosave_entries() {
        let loader = DefaultMemoryLoader::new(5, 0.0);
        let memory = MockMemoryWithEntries {
            entries: Arc::new(vec![
                MemoryEntry {
                    id: "1".into(),
                    key: "user_msg_e5f6g7h8".into(),
                    content: "User message embedding prior context verbatim".into(),
                    category: MemoryCategory::Conversation,
                    timestamp: "now".into(),
                    session_id: None,
                    score: Some(0.95),
                    namespace: "default".into(),
                    importance: None,
                    superseded_by: None,
                    kind: None,
                    pinned: false,
                    tenant_id: None,
                    agent_alias: None,
                    agent_id: None,
                },
                MemoryEntry {
                    id: "2".into(),
                    key: "user_fact".into(),
                    content: "User prefers concise answers".into(),
                    category: MemoryCategory::Conversation,
                    timestamp: "now".into(),
                    session_id: None,
                    score: Some(0.9),
                    namespace: "default".into(),
                    importance: None,
                    superseded_by: None,
                    kind: None,
                    pinned: false,
                    tenant_id: None,
                    agent_alias: None,
                    agent_id: None,
                },
            ]),
        };

        let context = loader
            .load_context(&memory, &NoopObserver, "answer style", None)
            .await
            .unwrap();
        assert!(context.contains("user_fact"));
        assert!(!context.contains("user_msg_e5f6g7h8"));
        assert!(!context.contains("embedding prior context"));
    }

    fn scored_entry(key: &str, content: &str, score: f64) -> MemoryEntry {
        MemoryEntry {
            id: key.into(),
            key: key.into(),
            content: content.into(),
            category: MemoryCategory::Conversation,
            timestamp: "now".into(),
            session_id: None,
            score: Some(score),
            namespace: "default".into(),
            importance: None,
            superseded_by: None,
            kind: None,
            pinned: false,
            tenant_id: None,
            agent_alias: None,
            agent_id: None,
        }
    }

    /// With reranking disabled (the default), the loader must not re-score,
    /// reorder, or de-duplicate recall results: every entry passes through, so
    /// even exact-duplicate content is rendered for each key. This is the
    /// multi-entry byte-identity guard the single-entry tests above cannot give.
    #[tokio::test]
    async fn rerank_disabled_passes_entries_through_without_dedup() {
        let memory = MockMemoryWithEntries {
            entries: Arc::new(vec![
                scored_entry("fact_a", "the office is in Denver", 0.9),
                scored_entry("fact_b", "the office is in Denver", 0.85),
                scored_entry("fact_c", "cats are mammals", 0.8),
            ]),
        };
        // new(..) leaves rerank disabled and candidate_multiplier at 1.
        let loader = DefaultMemoryLoader::new(5, 0.0);
        let context = loader
            .load_context(&memory, &NoopObserver, "q", None)
            .await
            .unwrap();
        assert!(context.contains("fact_a"));
        // Exact-duplicate content is kept when rerank is off.
        assert!(context.contains("fact_b"));
        assert!(context.contains("fact_c"));
    }

    /// With reranking enabled, `rerank::run` collapses duplicate content
    /// (keeping the first/highest-scored entry) before rendering, so the
    /// duplicate drops out. This proves the gate actually engages.
    #[tokio::test]
    async fn rerank_enabled_collapses_duplicate_content() {
        let memory = MockMemoryWithEntries {
            entries: Arc::new(vec![
                scored_entry("fact_a", "the office is in Denver", 0.9),
                scored_entry("fact_b", "the office is in Denver", 0.85),
                scored_entry("fact_c", "cats are mammals", 0.8),
            ]),
        };
        let rerank_config = RerankConfig {
            strategy: RerankStrategy::Mmr { lambda: 0.7 },
            threshold: 1,
            importance_weight: 0.2,
            recency_weight: 0.1,
            min_relevance_score: 0.0,
            final_limit: 5,
        };
        let loader = DefaultMemoryLoader::with_relevance_config(5, 0.0, 1, true, rerank_config);
        let context = loader
            .load_context(&memory, &NoopObserver, "q", None)
            .await
            .unwrap();
        assert!(context.contains("fact_a"));
        assert!(context.contains("fact_c"));
        // Duplicate content collapsed by the rerank stage.
        assert!(!context.contains("fact_b"));
    }

    /// Read-only KG recall fusion is safe by construction: fusing an EMPTY
    /// knowledge graph yields no candidates, so recall is returned unchanged.
    /// This is the end-to-end identity proof behind the (default-off) KG flag.
    #[tokio::test]
    async fn kg_fusion_with_empty_graph_is_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let kg = Arc::new(
            zeroclaw_memory::knowledge_graph::KnowledgeGraph::new(&tmp.path().join("kg.db"), 1000)
                .unwrap(),
        );
        let memory = MockMemoryWithEntries {
            entries: Arc::new(vec![
                scored_entry("fact_a", "the office is in Denver", 0.9),
                scored_entry("fact_c", "cats are mammals", 0.8),
            ]),
        };
        let with_kg = DefaultMemoryLoader::new(5, 0.0).with_kg_recall(Some(kg), true, 0.7, 0.3);
        let without_kg = DefaultMemoryLoader::new(5, 0.0);
        let fused = with_kg
            .load_context(&memory, &NoopObserver, "office", None)
            .await
            .unwrap();
        let plain = without_kg
            .load_context(&memory, &NoopObserver, "office", None)
            .await
            .unwrap();
        assert_eq!(fused, plain);
    }

    /// PR-C ships fully neutral: kind assignment, KG fusion, typed-fact
    /// extraction, and the consolidation sweep are all off by default.
    #[test]
    fn memory_types_and_consolidation_flags_default_off() {
        let cfg = zeroclaw_config::schema::MemoryConfig::default();
        assert!(!cfg.types.enabled);
        assert!(!cfg.types.fuse_kg_into_recall);
        assert!(!cfg.consolidation_extract_facts);
        assert!(!cfg.consolidation_sweep_enabled);
    }
}

//! Multi-stage retrieval pipeline.
//!
//! Wraps a `Memory` trait object with staged retrieval:
//! - **Stage 1 (Hot cache):** In-memory LRU of recent recall results.
//! - **Stage 2 (FTS):** FTS5 keyword search with optional early-return.
//! - **Stage 3 (Vector):** Vector similarity search + hybrid merge.
//!
//! Configurable via `[memory]` settings: `retrieval_stages`, `fts_early_return_score`.

use super::traits::{ExportFilter, Memory, MemoryCategory, MemoryEntry, MemoryStats, StoreOptions};
use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A cached recall result.
struct CachedResult {
    entries: Vec<MemoryEntry>,
    created_at: Instant,
}

/// Cache identity for a recall request.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RetrievalCacheKey {
    query: String,
    limit: usize,
    session_id: Option<String>,
    namespace: Option<String>,
    since: Option<String>,
    until: Option<String>,
}

impl RetrievalCacheKey {
    fn new(
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        namespace: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> Self {
        Self {
            query: query.to_string(),
            limit,
            session_id: session_id.map(str::to_string),
            namespace: namespace.map(str::to_string),
            since: since.map(str::to_string),
            until: until.map(str::to_string),
        }
    }
}

/// Multi-stage retrieval pipeline configuration.
#[derive(Debug, Clone)]
pub struct RetrievalConfig {
    /// Ordered list of stages: "cache", "fts", "vector".
    pub stages: Vec<String>,
    /// FTS score above which to early-return without vector stage.
    pub fts_early_return_score: f64,
    /// Max entries in the hot cache.
    pub cache_max_entries: usize,
    /// TTL for cached results.
    pub cache_ttl: Duration,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            stages: vec!["cache".into(), "fts".into(), "vector".into()],
            fts_early_return_score: 0.85,
            cache_max_entries: 256,
            cache_ttl: Duration::from_secs(300),
        }
    }
}

/// Multi-stage retrieval pipeline wrapping a `Memory` backend.
pub struct RetrievalPipeline {
    memory: Arc<dyn Memory>,
    config: RetrievalConfig,
    hot_cache: Mutex<HashMap<RetrievalCacheKey, CachedResult>>,
}

impl ::zeroclaw_api::attribution::Attributable for RetrievalPipeline {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        self.memory.role()
    }

    fn alias(&self) -> &str {
        self.memory.alias()
    }
}

impl RetrievalPipeline {
    pub fn new(memory: Arc<dyn Memory>, config: RetrievalConfig) -> Self {
        Self {
            memory,
            config,
            hot_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Build a cache key from query parameters.
    fn cache_key(
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        namespace: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> RetrievalCacheKey {
        RetrievalCacheKey::new(query, limit, session_id, namespace, since, until)
    }

    /// Check the hot cache for a previous result.
    fn check_cache(&self, key: &RetrievalCacheKey) -> Option<Vec<MemoryEntry>> {
        let cache = self.hot_cache.lock();
        if let Some(cached) = cache.get(key)
            && cached.created_at.elapsed() < self.config.cache_ttl
        {
            return Some(cached.entries.clone());
        }
        None
    }

    /// Store a result in the hot cache with LRU eviction.
    fn store_in_cache(&self, key: RetrievalCacheKey, entries: Vec<MemoryEntry>) {
        let mut cache = self.hot_cache.lock();

        // LRU eviction: remove oldest entries if at capacity
        if cache.len() >= self.config.cache_max_entries {
            let oldest_key = cache
                .iter()
                .min_by_key(|(_, v)| v.created_at)
                .map(|(k, _)| k.clone());
            if let Some(k) = oldest_key {
                cache.remove(&k);
            }
        }

        cache.insert(
            key,
            CachedResult {
                entries,
                created_at: Instant::now(),
            },
        );
    }

    /// Execute the multi-stage retrieval pipeline.
    pub async fn recall(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        namespace: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let ck = Self::cache_key(query, limit, session_id, namespace, since, until);

        for stage in &self.config.stages {
            match stage.as_str() {
                "cache" => {
                    if let Some(cached) = self.check_cache(&ck) {
                        ::zeroclaw_log::record!(
                            DEBUG,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_attrs(::serde_json::json!({"query": query})),
                            "retrieval pipeline: cache hit for ''"
                        );
                        return Ok(cached);
                    }
                }
                "fts" | "vector" => {
                    // Both FTS and vector are handled by the backend's recall method
                    // which already does hybrid merge. We delegate to it.
                    let results = if let Some(ns) = namespace {
                        self.memory
                            .recall_namespaced(ns, query, limit, session_id, since, until)
                            .await?
                    } else {
                        self.memory
                            .recall(query, limit, session_id, since, until)
                            .await?
                    };

                    if !results.is_empty() {
                        // Check for FTS early-return: if top score exceeds threshold
                        // and we're in the FTS stage, we can skip further stages
                        if stage == "fts"
                            && let Some(top_score) = results.first().and_then(|e| e.score)
                            && top_score >= self.config.fts_early_return_score
                        {
                            ::zeroclaw_log::record!(
                                DEBUG,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_attrs(::serde_json::json!({"top_score": top_score})),
                                "retrieval pipeline: FTS early return (score=)"
                            );
                            self.store_in_cache(ck, results.clone());
                            return Ok(results);
                        }

                        self.store_in_cache(ck, results.clone());
                        return Ok(results);
                    }
                }
                other => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"other": other})),
                        "retrieval pipeline: unknown stage '', skipping"
                    );
                }
            }
        }

        // No results from any stage
        Ok(Vec::new())
    }

    /// Invalidate the hot cache (e.g. after a store operation).
    pub fn invalidate_cache(&self) {
        self.hot_cache.lock().clear();
    }

    /// Get the number of entries in the hot cache.
    pub fn cache_size(&self) -> usize {
        self.hot_cache.lock().len()
    }
}

#[async_trait]
impl Memory for RetrievalPipeline {
    fn name(&self) -> &str {
        self.memory.name()
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.memory
            .store(key, content, category, session_id)
            .await?;
        self.invalidate_cache();
        Ok(())
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        RetrievalPipeline::recall(self, query, limit, session_id, None, since, until).await
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        self.memory.get(key).await
    }

    async fn get_for_agent(
        &self,
        key: &str,
        agent_id: &str,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        self.memory.get_for_agent(key, agent_id).await
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.memory.list(category, session_id).await
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        let removed = self.memory.forget(key).await?;
        if removed {
            self.invalidate_cache();
        }
        Ok(removed)
    }

    async fn forget_for_agent(&self, key: &str, agent_id: &str) -> anyhow::Result<bool> {
        let removed = self.memory.forget_for_agent(key, agent_id).await?;
        if removed {
            self.invalidate_cache();
        }
        Ok(removed)
    }

    async fn purge_namespace(&self, namespace: &str) -> anyhow::Result<usize> {
        let removed = self.memory.purge_namespace(namespace).await?;
        if removed > 0 {
            self.invalidate_cache();
        }
        Ok(removed)
    }

    async fn purge_session(&self, session_id: &str) -> anyhow::Result<usize> {
        let removed = self.memory.purge_session(session_id).await?;
        if removed > 0 {
            self.invalidate_cache();
        }
        Ok(removed)
    }

    async fn purge_session_for_agent(
        &self,
        session_id: &str,
        agent_id: &str,
    ) -> anyhow::Result<usize> {
        let removed = self
            .memory
            .purge_session_for_agent(session_id, agent_id)
            .await?;
        if removed > 0 {
            self.invalidate_cache();
        }
        Ok(removed)
    }

    async fn purge_agent(&self, agent_alias: &str) -> anyhow::Result<usize> {
        let removed = self.memory.purge_agent(agent_alias).await?;
        if removed > 0 {
            self.invalidate_cache();
        }
        Ok(removed)
    }

    async fn export_agent(&self, agent_alias: &str) -> anyhow::Result<Vec<MemoryEntry>> {
        self.memory.export_agent(agent_alias).await
    }

    async fn rename_agent(&self, from: &str, to: &str) -> anyhow::Result<usize> {
        let renamed = self.memory.rename_agent(from, to).await?;
        if renamed > 0 {
            self.invalidate_cache();
        }
        Ok(renamed)
    }

    async fn count_agent(&self, agent_alias: &str) -> anyhow::Result<usize> {
        self.memory.count_agent(agent_alias).await
    }

    async fn count(&self) -> anyhow::Result<usize> {
        self.memory.count().await
    }

    async fn health_check(&self) -> bool {
        self.memory.health_check().await
    }

    async fn supersede(&self, superseded_ids: &[String], new_id: &str) -> anyhow::Result<()> {
        self.memory.supersede(superseded_ids, new_id).await?;
        if !superseded_ids.is_empty() {
            self.invalidate_cache();
        }
        Ok(())
    }

    async fn count_in_scope(
        &self,
        namespace: Option<&str>,
        category: Option<&MemoryCategory>,
    ) -> anyhow::Result<u64> {
        self.memory.count_in_scope(namespace, category).await
    }

    async fn stats(&self) -> anyhow::Result<MemoryStats> {
        self.memory.stats().await
    }

    async fn reindex(&self) -> anyhow::Result<usize> {
        let reembedded = self.memory.reindex().await?;
        self.invalidate_cache();
        Ok(reembedded)
    }

    async fn recall_namespaced(
        &self,
        namespace: &str,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        RetrievalPipeline::recall(
            self,
            query,
            limit,
            session_id,
            Some(namespace),
            since,
            until,
        )
        .await
    }

    async fn export(&self, filter: &ExportFilter) -> anyhow::Result<Vec<MemoryEntry>> {
        self.memory.export(filter).await
    }

    async fn store_with_metadata(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        namespace: Option<&str>,
        importance: Option<f64>,
    ) -> anyhow::Result<()> {
        self.memory
            .store_with_metadata(key, content, category, session_id, namespace, importance)
            .await?;
        self.invalidate_cache();
        Ok(())
    }

    async fn store_with_options(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        options: StoreOptions,
    ) -> anyhow::Result<()> {
        self.memory
            .store_with_options(key, content, category, session_id, options)
            .await?;
        self.invalidate_cache();
        Ok(())
    }

    async fn store_with_agent(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        namespace: Option<&str>,
        importance: Option<f64>,
        agent_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.memory
            .store_with_agent(
                key, content, category, session_id, namespace, importance, agent_id,
            )
            .await?;
        self.invalidate_cache();
        Ok(())
    }

    async fn recall_for_agents(
        &self,
        allowed_agent_ids: &[&str],
        query: &str,
        limit: usize,
        session_id: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.memory
            .recall_for_agents(allowed_agent_ids, query, limit, session_id, since, until)
            .await
    }

    async fn ensure_agent_uuid(&self, alias: &str) -> anyhow::Result<String> {
        self.memory.ensure_agent_uuid(alias).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::none::NoneMemory;
    use crate::traits::MemoryCategory;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct StatefulMemory {
        entries: Mutex<Vec<MemoryEntry>>,
        recalls: AtomicUsize,
    }

    impl StatefulMemory {
        fn new(entries: Vec<MemoryEntry>) -> Self {
            Self {
                entries: Mutex::new(entries),
                recalls: AtomicUsize::new(0),
            }
        }

        fn recalls(&self) -> usize {
            self.recalls.load(Ordering::SeqCst)
        }
    }

    impl ::zeroclaw_api::attribution::Attributable for StatefulMemory {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Memory(
                ::zeroclaw_api::attribution::MemoryKind::InMemory,
            )
        }

        fn alias(&self) -> &str {
            "stateful-memory"
        }
    }

    #[async_trait]
    impl Memory for StatefulMemory {
        fn name(&self) -> &str {
            "stateful"
        }

        async fn store(
            &self,
            key: &str,
            content: &str,
            category: MemoryCategory,
            session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            *self.entries.lock() = vec![entry(key, content, 1.0, category, session_id)];
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
            self.recalls.fetch_add(1, Ordering::SeqCst);
            Ok(self.entries.lock().iter().take(limit).cloned().collect())
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(self.entries.lock().clone())
        }

        async fn forget(&self, key: &str) -> anyhow::Result<bool> {
            let mut entries = self.entries.lock();
            let before = entries.len();
            entries.retain(|entry| entry.key != key);
            Ok(entries.len() != before)
        }

        async fn forget_for_agent(&self, key: &str, _agent_id: &str) -> anyhow::Result<bool> {
            self.forget(key).await
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(self.entries.lock().len())
        }

        async fn health_check(&self) -> bool {
            true
        }

        async fn store_with_agent(
            &self,
            key: &str,
            content: &str,
            category: MemoryCategory,
            session_id: Option<&str>,
            _namespace: Option<&str>,
            _importance: Option<f64>,
            _agent_id: Option<&str>,
        ) -> anyhow::Result<()> {
            self.store(key, content, category, session_id).await
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

    fn entry(
        key: &str,
        content: &str,
        score: f64,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> MemoryEntry {
        MemoryEntry {
            id: key.into(),
            key: key.into(),
            content: content.into(),
            category,
            timestamp: "2026-06-30T00:00:00Z".into(),
            session_id: session_id.map(str::to_string),
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

    #[tokio::test]
    async fn pipeline_returns_empty_from_none_backend() {
        let memory = Arc::new(NoneMemory::new("none"));
        let pipeline = RetrievalPipeline::new(memory, RetrievalConfig::default());

        let results = pipeline
            .recall("test", 10, None, None, None, None)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn pipeline_cache_invalidation() {
        let memory = Arc::new(NoneMemory::new("none"));
        let pipeline = RetrievalPipeline::new(memory, RetrievalConfig::default());

        // Force a cache entry
        let ck = RetrievalPipeline::cache_key("test", 10, None, None, None, None);
        pipeline.store_in_cache(ck, vec![]);

        assert_eq!(pipeline.cache_size(), 1);
        pipeline.invalidate_cache();
        assert_eq!(pipeline.cache_size(), 0);
    }

    #[test]
    fn cache_key_includes_all_params() {
        let base = RetrievalPipeline::cache_key(
            "hello",
            10,
            Some("sess-a"),
            Some("ns1"),
            Some("2026-06-01T00:00:00Z"),
            Some("2026-06-02T00:00:00Z"),
        );
        let different_query = RetrievalPipeline::cache_key(
            "goodbye",
            10,
            Some("sess-a"),
            Some("ns1"),
            Some("2026-06-01T00:00:00Z"),
            Some("2026-06-02T00:00:00Z"),
        );
        let different_limit = RetrievalPipeline::cache_key(
            "hello",
            11,
            Some("sess-a"),
            Some("ns1"),
            Some("2026-06-01T00:00:00Z"),
            Some("2026-06-02T00:00:00Z"),
        );
        let different_session = RetrievalPipeline::cache_key(
            "hello",
            10,
            Some("sess-b"),
            Some("ns1"),
            Some("2026-06-01T00:00:00Z"),
            Some("2026-06-02T00:00:00Z"),
        );
        let different_namespace = RetrievalPipeline::cache_key(
            "hello",
            10,
            Some("sess-a"),
            Some("ns2"),
            Some("2026-06-01T00:00:00Z"),
            Some("2026-06-02T00:00:00Z"),
        );
        let different_since = RetrievalPipeline::cache_key(
            "hello",
            10,
            Some("sess-a"),
            Some("ns1"),
            Some("2026-06-03T00:00:00Z"),
            Some("2026-06-02T00:00:00Z"),
        );
        let different_until = RetrievalPipeline::cache_key(
            "hello",
            10,
            Some("sess-a"),
            Some("ns1"),
            Some("2026-06-01T00:00:00Z"),
            Some("2026-06-04T00:00:00Z"),
        );
        let absent_since = RetrievalPipeline::cache_key(
            "hello",
            10,
            Some("sess-a"),
            Some("ns1"),
            None,
            Some("2026-06-02T00:00:00Z"),
        );
        let empty_since = RetrievalPipeline::cache_key(
            "hello",
            10,
            Some("sess-a"),
            Some("ns1"),
            Some(""),
            Some("2026-06-02T00:00:00Z"),
        );
        let delimiter_in_query =
            RetrievalPipeline::cache_key("hello:10", 20, None, None, None, None);
        let delimiter_in_limit_shape =
            RetrievalPipeline::cache_key("hello", 10, Some("20"), None, None, None);

        assert_ne!(base, different_query);
        assert_ne!(base, different_limit);
        assert_ne!(base, different_session);
        assert_ne!(base, different_namespace);
        assert_ne!(base, different_since);
        assert_ne!(base, different_until);
        assert_ne!(absent_since, empty_since);
        assert_ne!(delimiter_in_query, delimiter_in_limit_shape);
    }

    #[tokio::test]
    async fn retrieval_cache_distinguishes_time_windows() {
        let memory = Arc::new(NoneMemory::new("none"));
        let pipeline = RetrievalPipeline::new(memory, RetrievalConfig::default());
        let cached_entry = MemoryEntry {
            id: "cached-window".into(),
            key: "project".into(),
            content: "cached content".into(),
            category: crate::traits::MemoryCategory::Core,
            timestamp: "2026-06-01T00:00:00Z".into(),
            session_id: Some("session-a".into()),
            score: Some(0.9),
            namespace: "default".into(),
            importance: None,
            superseded_by: None,
            kind: None,
            pinned: false,
            tenant_id: None,
            agent_alias: None,
            agent_id: None,
        };
        let first_window_key = RetrievalPipeline::cache_key(
            "project",
            10,
            Some("session-a"),
            None,
            Some("2026-06-01T00:00:00Z"),
            Some("2026-06-02T00:00:00Z"),
        );
        pipeline.store_in_cache(first_window_key, vec![cached_entry]);

        let first = pipeline
            .recall(
                "project",
                10,
                Some("session-a"),
                None,
                Some("2026-06-01T00:00:00Z"),
                Some("2026-06-02T00:00:00Z"),
            )
            .await
            .unwrap();
        let second = pipeline
            .recall(
                "project",
                10,
                Some("session-a"),
                None,
                Some("2026-06-03T00:00:00Z"),
                Some("2026-06-04T00:00:00Z"),
            )
            .await
            .unwrap();

        assert_eq!(first[0].content, "cached content");
        assert!(
            second.is_empty(),
            "a different time window must not reuse a cached result"
        );
    }

    #[tokio::test]
    async fn pipeline_caches_results() {
        let memory = Arc::new(NoneMemory::new("none"));
        let config = RetrievalConfig {
            stages: vec!["cache".into()],
            ..Default::default()
        };
        let pipeline = RetrievalPipeline::new(memory, config);

        // First call: cache miss, no results
        let results = pipeline
            .recall("test", 10, None, None, None, None)
            .await
            .unwrap();
        assert!(results.is_empty());

        // Manually insert a cache entry
        let ck = RetrievalPipeline::cache_key("cached_query", 5, None, None, None, None);
        let fake_entry = MemoryEntry {
            id: "1".into(),
            key: "k".into(),
            content: "cached content".into(),
            category: crate::traits::MemoryCategory::Core,
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
        };
        pipeline.store_in_cache(ck, vec![fake_entry]);

        // Cache hit
        let results = pipeline
            .recall("cached_query", 5, None, None, None, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "cached content");
    }

    #[tokio::test]
    async fn pipeline_checks_cache_before_backend_stage() {
        let memory = Arc::new(StatefulMemory::new(vec![entry(
            "k1",
            "first backend result",
            0.9,
            MemoryCategory::Core,
            None,
        )]));
        let pipeline = RetrievalPipeline::new(memory.clone(), RetrievalConfig::default());

        let first = pipeline
            .recall("query", 5, None, None, None, None)
            .await
            .unwrap();
        let second = pipeline
            .recall("query", 5, None, None, None, None)
            .await
            .unwrap();

        assert_eq!(first[0].content, "first backend result");
        assert_eq!(second[0].content, "first backend result");
        assert_eq!(memory.recalls(), 1, "second recall should hit hot cache");
    }

    #[tokio::test]
    async fn pipeline_store_invalidates_cached_recall() {
        let memory = Arc::new(StatefulMemory::new(vec![entry(
            "old",
            "stale cached result",
            0.9,
            MemoryCategory::Core,
            None,
        )]));
        let pipeline = RetrievalPipeline::new(memory.clone(), RetrievalConfig::default());

        let first = pipeline
            .recall("query", 5, None, None, None, None)
            .await
            .unwrap();
        assert_eq!(first[0].content, "stale cached result");
        assert_eq!(pipeline.cache_size(), 1);

        pipeline
            .store("new", "fresh written result", MemoryCategory::Core, None)
            .await
            .unwrap();
        assert_eq!(pipeline.cache_size(), 0);

        let second = pipeline
            .recall("query", 5, None, None, None, None)
            .await
            .unwrap();
        assert_eq!(second[0].content, "fresh written result");
        assert_eq!(memory.recalls(), 2);
    }

    #[tokio::test]
    async fn pipeline_preserves_backend_sorted_scores() {
        let memory = Arc::new(StatefulMemory::new(vec![
            entry(
                "high",
                "normalized fused high score",
                1.0,
                MemoryCategory::Core,
                None,
            ),
            entry(
                "mid",
                "normalized fused medium score",
                0.6,
                MemoryCategory::Core,
                None,
            ),
            entry(
                "low",
                "normalized fused low score",
                0.2,
                MemoryCategory::Core,
                None,
            ),
        ]));
        let pipeline = RetrievalPipeline::new(memory, RetrievalConfig::default());

        let results = pipeline
            .recall("query", 3, None, None, None, None)
            .await
            .unwrap();

        assert_eq!(
            results
                .iter()
                .map(|entry| entry.key.as_str())
                .collect::<Vec<_>>(),
            vec!["high", "mid", "low"]
        );
        assert!(results.iter().all(|entry| {
            entry
                .score
                .is_some_and(|score| (0.0..=1.0).contains(&score))
        }));
    }
}

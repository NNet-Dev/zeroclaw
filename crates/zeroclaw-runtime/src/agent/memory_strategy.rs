use std::sync::Arc;
use zeroclaw_api::memory_traits::{Memory, MemoryStrategy};
use zeroclaw_api::model_provider::ModelProvider;
use zeroclaw_api::observability_traits::Observer;
use zeroclaw_memory::{RerankConfig, RerankStrategy, RetrievalConfig, RetrievalPipeline};

use crate::agent::memory_loader::{DefaultMemoryLoader, MemoryLoader};

/// Default memory strategy that delegates to existing implementations.
///
/// It wraps the backend in the staged [`RetrievalPipeline`] (cache + delegated
/// recall) and drives `DefaultMemoryLoader`, `consolidation::consolidate_turn`,
/// and `hygiene::run_if_due`. With the relevance flags at their defaults the
/// recall path is byte-identical to the pre-relevance loader.
pub struct DefaultMemoryStrategy {
    pipeline: Arc<RetrievalPipeline>,
    limit: usize,
    min_relevance_score: f64,
    memory_config: zeroclaw_config::schema::MemoryConfig,
    workspace_dir: std::path::PathBuf,
}

impl DefaultMemoryStrategy {
    pub fn new(
        memory: Arc<dyn Memory>,
        memory_config: zeroclaw_config::schema::MemoryConfig,
        workspace_dir: impl Into<std::path::PathBuf>,
    ) -> Self {
        // Wrap the backend in the staged retrieval pipeline. It caches recall
        // results and invalidates the cache on every write, so it is
        // behaviour-neutral for the default recall path.
        let pipeline = Arc::new(RetrievalPipeline::new(
            memory,
            build_retrieval_config(&memory_config),
        ));
        Self {
            pipeline,
            limit: 5,
            min_relevance_score: memory_config.min_relevance_score,
            memory_config,
            workspace_dir: workspace_dir.into(),
        }
    }

    /// Convenience constructor that takes the live `MemoryConfig` so
    /// `run_governance` uses the operator's actual settings (archive
    /// windows, hygiene toggle, etc.) rather than hardcoded defaults.
    pub fn with_config(
        memory: Arc<dyn Memory>,
        memory_config: zeroclaw_config::schema::MemoryConfig,
        workspace_dir: impl Into<std::path::PathBuf>,
    ) -> Self {
        Self::new(memory, memory_config, workspace_dir)
    }

    /// Build a strategy using the effective per-agent recall limit resolved by
    /// the caller while preserving the rest of the live memory configuration.
    pub fn with_config_and_limit(
        memory: Arc<dyn Memory>,
        memory_config: zeroclaw_config::schema::MemoryConfig,
        workspace_dir: impl Into<std::path::PathBuf>,
        limit: usize,
    ) -> Self {
        let mut strategy = Self::new(memory, memory_config, workspace_dir);
        strategy.limit = limit.max(1);
        strategy
    }
}

#[async_trait::async_trait]
impl MemoryStrategy for DefaultMemoryStrategy {
    async fn load_context(
        &self,
        observer: &dyn Observer,
        query: &str,
        session_id: Option<&str>,
    ) -> anyhow::Result<String> {
        let loader = DefaultMemoryLoader::with_relevance_config(
            self.limit,
            self.min_relevance_score,
            self.memory_config.candidate_multiplier,
            self.memory_config.rerank_enabled,
            build_rerank_config(&self.memory_config, self.limit, self.min_relevance_score),
        );
        loader
            .load_context(self.pipeline.as_ref(), observer, query, session_id)
            .await
    }

    async fn consolidate_turn(
        &self,
        user_message: &str,
        assistant_response: &str,
        provider: &dyn ModelProvider,
        model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<()> {
        zeroclaw_memory::consolidation::consolidate_turn(
            provider,
            model,
            temperature,
            self.pipeline.as_ref(),
            &self.memory_config,
            user_message,
            assistant_response,
        )
        .await
    }

    async fn run_governance(&self) -> anyhow::Result<()> {
        // Delegate to the existing hygiene routine.
        // Phase 1: `hygiene::run_if_due` returns `Result<()>`.
        // A structured report will be wired in a follow-up when hygiene
        // exposes per-action counters.
        zeroclaw_memory::hygiene::run_if_due(&self.memory_config, &self.workspace_dir)
    }
}

fn build_retrieval_config(
    memory_config: &zeroclaw_config::schema::MemoryConfig,
) -> RetrievalConfig {
    RetrievalConfig {
        stages: memory_config.retrieval_stages.clone(),
        fts_early_return_score: memory_config.fts_early_return_score,
        ..RetrievalConfig::default()
    }
}

/// Materialize the rerank stage config. When `rerank_enabled` is false the
/// strategy is `None` and the loader skips the rerank stage entirely, keeping
/// recall byte-identical to the pre-relevance path. The LLM-judge strategy is
/// not yet implemented (tracked as a follow-up); requesting it warns and falls
/// back to no advanced rerank.
fn build_rerank_config(
    memory_config: &zeroclaw_config::schema::MemoryConfig,
    final_limit: usize,
    min_relevance_score: f64,
) -> RerankConfig {
    let strategy = if !memory_config.rerank_enabled {
        RerankStrategy::None
    } else {
        match memory_config
            .rerank_strategy
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "mmr" => RerankStrategy::Mmr {
                lambda: memory_config.mmr_lambda,
            },
            "none" | "" => RerankStrategy::None,
            other => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "rerank_strategy": other,
                        })),
                    "unknown or unsupported memory rerank_strategy; falling back to no advanced rerank"
                );
                RerankStrategy::None
            }
        }
    };

    RerankConfig {
        strategy,
        threshold: memory_config.rerank_threshold,
        importance_weight: memory_config.importance_weight,
        recency_weight: memory_config.recency_weight,
        min_relevance_score,
        final_limit: final_limit.max(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::NoopObserver;
    use zeroclaw_config::schema::MemoryConfig;
    use zeroclaw_memory::{MemoryCategory, SqliteMemory};

    /// End-to-end proof that the strategy recalls through the wrapped
    /// `RetrievalPipeline`: a fact stored on the backend is surfaced by
    /// `load_context`. With default config the relevance flags are off, so this
    /// also exercises the behaviour-neutral recall path.
    #[tokio::test]
    async fn strategy_recall_routes_through_pipeline() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = Arc::new(SqliteMemory::new("test", tmp.path()).unwrap());
        memory
            .store(
                "greeting",
                "the sky is blue today",
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();

        let strategy =
            DefaultMemoryStrategy::with_config(memory, MemoryConfig::default(), tmp.path());
        // "*" is the recent-recall query, so the result does not depend on FTS
        // scoring; the stored fact must reach the rendered context.
        let context = strategy
            .load_context(&NoopObserver, "*", None)
            .await
            .unwrap();
        assert!(context.contains("the sky is blue today"));
    }
}

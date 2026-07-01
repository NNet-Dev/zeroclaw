//! Periodic cross-turn consolidation sweep machinery.

use crate::conflict;
use crate::knowledge_graph::KnowledgeGraph;
use crate::traits::{Memory, MemoryCategory, MemoryKind, SemanticSubtype, StoreOptions};
use serde::Deserialize;
use zeroclaw_api::model_provider::ModelProvider;
use zeroclaw_config::schema::MemoryConfig;
use zeroclaw_providers::ProviderDispatch;

const SWEEP_MERGE_PROMPT: &str = r#"You are a memory sweep engine. Given recent daily memory entries, extract only durable facts that should survive across turns.

Return ONLY valid JSON: {"facts":["..."],"trend":"..." or null}
Rules:
- Prefer stable facts, preferences, and decisions.
- Do not include procedures or how-to workflows; those belong in skills.
- Do not repeat facts that are already equivalent.
- Return at most the requested number of facts."#;

/// Per-session sweep trigger counter.
#[derive(Debug, Default)]
pub struct SweepCounter {
    turns_since_sweep: u32,
}

impl SweepCounter {
    /// Returns true and resets when `interval` turns have elapsed.
    pub fn tick(&mut self, interval: u32) -> bool {
        if interval == 0 {
            return false;
        }
        self.turns_since_sweep = self.turns_since_sweep.saturating_add(1);
        if self.turns_since_sweep >= interval {
            self.turns_since_sweep = 0;
            return true;
        }
        false
    }
}

/// Bounded sweep configuration derived from canonical memory config.
#[derive(Debug, Clone)]
pub struct SweepConfig {
    pub window: usize,
    pub max_merges: usize,
    pub conflict_threshold: f64,
}

impl SweepConfig {
    pub fn from_memory_config(config: &MemoryConfig) -> Self {
        Self {
            window: config.consolidation_sweep_window.max(1),
            max_merges: config.consolidation_sweep_max_merges.max(1),
            conflict_threshold: config.conflict_threshold,
        }
    }
}

/// Summary of a sweep pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub entries_read: usize,
    pub merged: usize,
    pub promoted: usize,
    pub superseded: usize,
}

#[derive(Debug, Deserialize)]
struct SweepExtraction {
    #[serde(default)]
    facts: Vec<String>,
    #[serde(default)]
    trend: Option<String>,
}

/// Periodic cross-turn merge/pattern pass.
pub async fn consolidate_sweep(
    memory: &dyn Memory,
    _kg: Option<&KnowledgeGraph>,
    provider: &dyn ModelProvider,
    model: &str,
    temperature: Option<f64>,
    session_id: &str,
    config: &SweepConfig,
) -> anyhow::Result<SweepReport> {
    let entries = memory
        .list(Some(&MemoryCategory::Daily), Some(session_id))
        .await?
        .into_iter()
        .rev()
        .take(config.window)
        .collect::<Vec<_>>();

    if entries.is_empty() {
        return Ok(SweepReport::default());
    }

    let prompt_input = format!(
        "Max facts: {}\nRecent entries:\n{}",
        config.max_merges,
        entries
            .iter()
            .map(|entry| format!("- {}", entry.content))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let raw = ProviderDispatch::from_ref(provider)
        .chat_with_system(Some(SWEEP_MERGE_PROMPT), &prompt_input, model, temperature)
        .await?;
    let extraction = parse_sweep_response(&raw);

    let mut report = SweepReport {
        entries_read: entries.len(),
        ..SweepReport::default()
    };

    for fact in extraction
        .facts
        .into_iter()
        .chain(extraction.trend)
        .filter_map(|fact| {
            let trimmed = fact.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .take(config.max_merges)
    {
        let key = format!("core_sweep_{}", uuid::Uuid::new_v4());
        let superseded = conflict::check_and_resolve_conflicts(
            memory,
            &key,
            &fact,
            &MemoryCategory::Core,
            config.conflict_threshold,
        )
        .await
        .unwrap_or_default();
        let superseded_count = superseded.len();
        memory
            .store_with_options(
                &key,
                &fact,
                MemoryCategory::Core,
                Some(session_id),
                StoreOptions::default().with_kind(MemoryKind::Semantic(SemanticSubtype::Fact)),
            )
            .await?;
        memory.supersede(&superseded, &key).await?;
        report.merged += 1;
        report.promoted += 1;
        report.superseded += superseded_count;
    }

    Ok(report)
}

fn parse_sweep_response(raw: &str) -> SweepExtraction {
    let cleaned = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    serde_json::from_str(cleaned).unwrap_or(SweepExtraction {
        facts: Vec::new(),
        trend: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_fires_and_resets_on_interval() {
        let mut counter = SweepCounter::default();
        assert!(!counter.tick(3));
        assert!(!counter.tick(3));
        assert!(counter.tick(3));
        assert!(!counter.tick(3));
    }

    #[test]
    fn counter_disabled_by_zero_interval() {
        let mut counter = SweepCounter::default();
        assert!(!counter.tick(0));
        assert!(!counter.tick(0));
    }

    #[test]
    fn parses_sweep_response_with_markdown_fence() {
        let parsed = parse_sweep_response(
            "```json\n{\"facts\":[\"Deploys require approval\"],\"trend\":\"Safety focus\"}\n```",
        );
        assert_eq!(parsed.facts, vec!["Deploys require approval"]);
        assert_eq!(parsed.trend.as_deref(), Some("Safety focus"));
    }
}

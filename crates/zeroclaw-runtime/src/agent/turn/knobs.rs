//! Per-caller loop behaviour knobsconsolidation).

use zeroclaw_config::coding::CodingConfig;

/// How to handle max-tool-iteration exhaustion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MaxIterationBehavior {
    /// Ask the LLM for a tools-free final summary (channel/CLI behaviour).
    #[default]
    GracefulSummary,
    /// Bail with "exceeded maximum tool iterations" (embedder control signal).
    ErrorAtCap,
}

/// Explicit knobs for per-caller loop behaviour.
#[derive(Debug, Clone)]
pub struct LoopKnobs {
    pub dedup_enabled: bool,
    pub max_iteration_behavior: MaxIterationBehavior,
    pub coding: CodingConfig,
    /// When `true` (channel paths), a response that resembles the internal
    /// tool protocol while no tools are enabled is classified as a parse
    /// issue (malformed-protocol retry, then i18n fallback) so raw protocol
    /// text never reaches end users. Embedder wrappers set `false`: their
    /// contract is to return the model text verbatim and let the embedder
    /// do its own post-processing.
    pub detect_protocol_without_tools: bool,
}

impl Default for LoopKnobs {
    fn default() -> Self {
        Self {
            dedup_enabled: true,
            max_iteration_behavior: MaxIterationBehavior::GracefulSummary,
            coding: CodingConfig::default(),
            detect_protocol_without_tools: true,
        }
    }
}

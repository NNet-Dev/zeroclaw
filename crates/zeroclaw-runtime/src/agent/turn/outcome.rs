//! Turn-loop control-flow outcomes: cancellation and model-switch errors.

use std::sync::{Arc, Mutex};

/// Callback type for checking if model has been switched during tool execution.
/// Returns Some((model_provider, model)) if a switch was requested, None otherwise.
pub type ModelSwitchCallback = Arc<Mutex<Option<(String, String)>>>;

#[derive(Debug)]
pub struct ToolLoopCancelled;

impl std::fmt::Display for ToolLoopCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("tool loop cancelled")
    }
}

impl std::error::Error for ToolLoopCancelled {}

pub fn is_tool_loop_cancelled(err: &anyhow::Error) -> bool {
    err.chain().any(|source| source.is::<ToolLoopCancelled>())
}

#[derive(Debug)]
pub struct ModelSwitchRequested {
    pub model_provider: String,
    pub model: String,
}

impl std::fmt::Display for ModelSwitchRequested {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "model switch requested to {} {}",
            self.model_provider, self.model
        )
    }
}

impl std::error::Error for ModelSwitchRequested {}

pub fn is_model_switch_requested(err: &anyhow::Error) -> Option<(String, String)> {
    err.chain()
        .filter_map(|source| source.downcast_ref::<ModelSwitchRequested>())
        .map(|e| (e.model_provider.clone(), e.model.clone()))
        .next()
}

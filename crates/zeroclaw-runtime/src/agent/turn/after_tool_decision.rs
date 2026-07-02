use crate::agent::tool_execution::ToolExecutionOutcome;
use zeroclaw_api::hook::AfterToolCallDecision;

/// Apply post-tool hook decisions to the outcome that will be recorded.
///
/// Result rewrites are last-wins, matching hook priority order after
/// `HookRunner` sorting.
pub(crate) fn apply_after_tool_decisions(
    outcome: &mut ToolExecutionOutcome,
    decisions: Vec<AfterToolCallDecision>,
) {
    for decision in decisions {
        match decision {
            AfterToolCallDecision::Continue => {}
            AfterToolCallDecision::RewriteResult(result) => {
                outcome.success = result.success;
                outcome.output = result.output.into_string();
                outcome.error_reason = result.error;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use zeroclaw_api::tool::ToolResult;

    fn outcome() -> ToolExecutionOutcome {
        ToolExecutionOutcome {
            output: "original".to_string(),
            output_data: None,
            success: true,
            error_reason: None,
            diagnostics: None,
            duration: Duration::ZERO,
            receipt: None,
        }
    }

    #[test]
    fn continue_leaves_outcome_unchanged() {
        let mut outcome = outcome();
        apply_after_tool_decisions(&mut outcome, vec![AfterToolCallDecision::Continue]);

        assert_eq!(outcome.output, "original");
        assert!(outcome.success);
        assert_eq!(outcome.error_reason, None);
    }

    #[test]
    fn rewrite_result_replaces_outcome() {
        let mut outcome = outcome();
        apply_after_tool_decisions(
            &mut outcome,
            vec![AfterToolCallDecision::RewriteResult(ToolResult {
                success: false,
                output: "rewritten".into(),
                error: Some("hook error".to_string()),
                diagnostics: None,
            })],
        );

        assert_eq!(outcome.output, "rewritten");
        assert!(!outcome.success);
        assert_eq!(outcome.error_reason.as_deref(), Some("hook error"));
    }
}

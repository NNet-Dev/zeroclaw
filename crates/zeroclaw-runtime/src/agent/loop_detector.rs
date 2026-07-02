//! Loop detection guardrail for the agent tool-call loop.

use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use zeroclaw_api::tool::ToolSideEffect;

use crate::agent::tool_classification::{ToolClass, classify};

// ── Configuration ────────────────────────────────────────────────

/// Configuration for the loop detector, typically derived from
/// `PacingConfig` fields at the call site.
#[derive(Debug, Clone)]
pub struct LoopDetectorConfig {
    /// Master switch. When `false`, `record` always returns `Ok`.
    pub enabled: bool,
    /// Number of recent calls retained for pattern analysis.
    pub window_size: usize,
    /// How many consecutive exact-repeat calls before escalation starts.
    pub max_repeats: usize,
    /// How many same-signature failed calls before failure-specific escalation starts.
    pub exact_failure_max: usize,
    /// Whether no-progress detections may abort the turn.
    pub no_progress_hard_stop: bool,
}

impl Default for LoopDetectorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            window_size: 20,
            max_repeats: 3,
            exact_failure_max: 7,
            no_progress_hard_stop: false,
        }
    }
}

// ── Result enum ──────────────────────────────────────────────────

/// Outcome of a loop-detection check after recording a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopDetectionResult {
    /// No pattern detected — continue normally.
    Ok,
    /// A suspicious pattern was detected; the caller should inject a
    /// system-level nudge message into the conversation.
    Warning(String),
    /// The tool call should be refused (output replaced with an error).
    Block(String),
    /// The agent turn should be terminated immediately.
    Break(String),
}

// ── Internal types ───────────────────────────────────────────────

/// A single recorded tool invocation inside the sliding window.
#[derive(Debug, Clone)]
struct ToolCallRecord {
    /// Tool name.
    name: String,
    /// Hash of the serialised arguments.
    args_hash: u64,
    /// Hash of the tool's output/result.
    result_hash: u64,
    /// Whether the tool call completed successfully.
    success: bool,
}

/// Produce a deterministic hash for a JSON value by recursively sorting
/// object keys before serialisation.  This ensures `{"a":1,"b":2}` and
/// `{"b":2,"a":1}` hash identically.
fn hash_value(value: &serde_json::Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    let canonical = serde_json::to_string(&canonicalise(value)).unwrap_or_default();
    canonical.hash(&mut hasher);
    hasher.finish()
}

/// Return a clone of `value` with all object keys sorted recursively.
fn canonicalise(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            sorted.sort_by_key(|(k, _)| *k);
            let new_map: serde_json::Map<String, serde_json::Value> = sorted
                .into_iter()
                .map(|(k, v)| (k.clone(), canonicalise(v)))
                .collect();
            serde_json::Value::Object(new_map)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(canonicalise).collect())
        }
        other => other.clone(),
    }
}

fn hash_str(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

// ── Detector ─────────────────────────────────────────────────────

/// Stateful loop detector that lives for the duration of a single
/// `run_tool_call_loop` invocation.
pub struct LoopDetector {
    config: LoopDetectorConfig,
    window: VecDeque<ToolCallRecord>,
}

impl LoopDetector {
    pub fn new(config: LoopDetectorConfig) -> Self {
        Self {
            window: VecDeque::with_capacity(config.window_size),
            config,
        }
    }

    pub fn record(
        &mut self,
        name: &str,
        args: &serde_json::Value,
        success: bool,
        side_effect: ToolSideEffect,
        result: &str,
    ) -> LoopDetectionResult {
        if !self.config.enabled {
            return LoopDetectionResult::Ok;
        }

        let record = ToolCallRecord {
            name: name.to_string(),
            args_hash: hash_value(args),
            result_hash: hash_str(result),
            success,
        };

        // Maintain sliding window.
        if self.window.len() >= self.config.window_size {
            self.window.pop_front();
        }
        self.window.push_back(record);

        // Run detectors in escalation order (most severe first). Failed calls
        // get failure-specific guidance instead of generic exact-repeat text.
        if let Some(result) = self.detect_exact_failure() {
            return result;
        }
        if let Some(result) = self.detect_exact_repeat() {
            return result;
        }
        if let Some(result) = self.detect_ping_pong() {
            return result;
        }
        if let Some(result) = self.detect_no_progress(side_effect) {
            return result;
        }

        LoopDetectionResult::Ok
    }

    fn detect_exact_repeat(&self) -> Option<LoopDetectionResult> {
        let max = self.config.max_repeats;
        if self.window.len() < max {
            return None;
        }

        let last = self.window.back()?;
        let consecutive = self
            .window
            .iter()
            .rev()
            .take_while(|r| r.name == last.name && r.args_hash == last.args_hash)
            .count();

        if consecutive >= max + 2 {
            Some(LoopDetectionResult::Break(format!(
                "Circuit breaker: tool '{}' called {} times consecutively with identical arguments",
                last.name, consecutive
            )))
        } else if consecutive > max {
            Some(LoopDetectionResult::Block(format!(
                "Blocked: tool '{}' called {} times consecutively with identical arguments",
                last.name, consecutive
            )))
        } else if consecutive >= max {
            Some(LoopDetectionResult::Warning(format!(
                "Warning: tool '{}' has been called {} times consecutively with identical arguments. \
                 Try a different approach.",
                last.name, consecutive
            )))
        } else {
            None
        }
    }

    /// Pattern 2: Same tool + same args failing repeatedly across the window.
    ///
    /// This is distinct from generic "no progress": the model needs to inspect
    /// the error and change strategy, while still retaining a circuit breaker
    /// for a genuinely stuck failure loop.
    fn detect_exact_failure(&self) -> Option<LoopDetectionResult> {
        let max = self.config.exact_failure_max;
        if max == 0 || self.window.len() < max {
            return None;
        }

        let last = self.window.back()?;
        if last.success {
            return None;
        }

        let count = self
            .window
            .iter()
            .filter(|r| !r.success && r.name == last.name && r.args_hash == last.args_hash)
            .count();

        if count < max {
            return None;
        }

        let guidance = "inspect the error and change strategy";
        if count >= max + 2 {
            Some(LoopDetectionResult::Break(format!(
                "Circuit breaker: tool '{}' failed {} times with identical arguments; {guidance}",
                last.name, count
            )))
        } else if count > max {
            Some(LoopDetectionResult::Block(format!(
                "Blocked: tool '{}' failed {} times with identical arguments; {guidance}",
                last.name, count
            )))
        } else {
            Some(LoopDetectionResult::Warning(format!(
                "Warning: tool '{}' has failed {} times with identical arguments; {guidance}.",
                last.name, count
            )))
        }
    }

    /// Pattern 3: Two tools alternating (A->B->A->B) for 4+ full cycles
    /// (i.e. 8 consecutive entries following the pattern).
    fn detect_ping_pong(&self) -> Option<LoopDetectionResult> {
        const MIN_CYCLES: usize = 4;
        let needed = MIN_CYCLES * 2; // each cycle = 2 calls

        if self.window.len() < needed {
            return None;
        }

        let tail: Vec<&ToolCallRecord> = self.window.iter().rev().take(needed).collect();
        // tail[0] is most recent; pattern: A, B, A, B, ...
        let a_name = &tail[0].name;
        let b_name = &tail[1].name;

        if a_name == b_name {
            return None;
        }

        let is_ping_pong = tail.iter().enumerate().all(|(i, r)| {
            if i % 2 == 0 {
                &r.name == a_name
            } else {
                &r.name == b_name
            }
        });

        if !is_ping_pong {
            return None;
        }

        // Count total alternating length for escalation.
        let mut cycles = MIN_CYCLES;
        let extended: Vec<&ToolCallRecord> = self.window.iter().rev().collect();
        for extra_pair in extended.chunks(2).skip(MIN_CYCLES) {
            if extra_pair.len() == 2
                && &extra_pair[0].name == a_name
                && &extra_pair[1].name == b_name
            {
                cycles += 1;
            } else {
                break;
            }
        }

        if cycles >= MIN_CYCLES + 2 {
            Some(LoopDetectionResult::Break(format!(
                "Circuit breaker: tools '{}' and '{}' have been alternating for {} cycles",
                a_name, b_name, cycles
            )))
        } else if cycles > MIN_CYCLES {
            Some(LoopDetectionResult::Block(format!(
                "Blocked: tools '{}' and '{}' have been alternating for {} cycles",
                a_name, b_name, cycles
            )))
        } else {
            Some(LoopDetectionResult::Warning(format!(
                "Warning: tools '{}' and '{}' appear to be alternating ({} cycles). \
                 Consider a different strategy.",
                a_name, b_name, cycles
            )))
        }
    }

    /// Pattern 4: Same tool called 5+ times (with different args each time)
    /// but producing the exact same result hash every time, counted across the
    /// whole window so interleaved unrelated calls do not reset the streak.
    fn detect_no_progress(&self, side_effect: ToolSideEffect) -> Option<LoopDetectionResult> {
        const MIN_CALLS: usize = 5;

        if self.window.len() < MIN_CALLS {
            return None;
        }

        let last = self.window.back()?;
        if classify(side_effect) != ToolClass::Idempotent {
            return None;
        }

        // the stuck agent ran 43 near-duplicate shell calls returning
        // byte-identical output, interleaved with other tools; filter (not a
        // consecutive take_while) is what lets that non-adjacent run be counted.
        let same_tool_same_result: Vec<&ToolCallRecord> = self
            .window
            .iter()
            .filter(|r| r.name == last.name && r.result_hash == last.result_hash && r.success)
            .collect();

        let count = same_tool_same_result.len();
        if count < MIN_CALLS {
            return None;
        }

        // Verify they have *different* args (otherwise exact_repeat handles it).
        let unique_args: std::collections::HashSet<u64> =
            same_tool_same_result.iter().map(|r| r.args_hash).collect();
        if unique_args.len() < 2 {
            // All same args — this is exact-repeat territory, not no-progress.
            return None;
        }

        if count >= MIN_CALLS + 2 && self.config.no_progress_hard_stop {
            Some(LoopDetectionResult::Break(format!(
                "Circuit breaker: tool '{}' called {} times with different arguments but identical results — no progress",
                last.name, count
            )))
        } else if count > MIN_CALLS {
            Some(LoopDetectionResult::Block(format!(
                "Blocked: tool '{}' called {} times with different arguments but identical results",
                last.name, count
            )))
        } else {
            Some(LoopDetectionResult::Warning(format!(
                "Warning: tool '{}' called {} times with different arguments but identical results. \
                 The current approach may not be making progress.",
                last.name, count
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn default_config() -> LoopDetectorConfig {
        LoopDetectorConfig::default()
    }

    fn config_with_repeats(max_repeats: usize) -> LoopDetectorConfig {
        LoopDetectorConfig {
            enabled: true,
            window_size: 20,
            max_repeats,
            ..Default::default()
        }
    }

    fn config_with_failure_max(exact_failure_max: usize) -> LoopDetectorConfig {
        LoopDetectorConfig {
            enabled: true,
            window_size: 20,
            exact_failure_max,
            // Keep generic exact-repeat quiet so the exact-failure tests prove
            // the failure-specific detector and message.
            max_repeats: 20,
            ..Default::default()
        }
    }

    fn record_success(
        det: &mut LoopDetector,
        name: &str,
        args: &serde_json::Value,
        result: &str,
    ) -> LoopDetectionResult {
        det.record(name, args, true, ToolSideEffect::ReadOnly, result)
    }

    fn record_success_with_side_effect(
        det: &mut LoopDetector,
        name: &str,
        args: &serde_json::Value,
        side_effect: ToolSideEffect,
        result: &str,
    ) -> LoopDetectionResult {
        det.record(name, args, true, side_effect, result)
    }

    fn record_failure(
        det: &mut LoopDetector,
        name: &str,
        args: &serde_json::Value,
        result: &str,
    ) -> LoopDetectionResult {
        det.record(name, args, false, ToolSideEffect::ReadOnly, result)
    }

    // ── Exact repeat tests ───────────────────────────────────────

    #[test]
    fn exact_repeat_warning_at_threshold() {
        let mut det = LoopDetector::new(config_with_repeats(3));
        let args = json!({"path": "/tmp/foo"});

        assert_eq!(
            record_success(&mut det, "file_read", &args, "contents"),
            LoopDetectionResult::Ok
        );
        assert_eq!(
            record_success(&mut det, "file_read", &args, "contents"),
            LoopDetectionResult::Ok
        );
        // 3rd consecutive = warning
        match record_success(&mut det, "file_read", &args, "contents") {
            LoopDetectionResult::Warning(msg) => {
                assert!(msg.contains("file_read"));
                assert!(msg.contains("3 times"));
            }
            other => panic!("expected Warning, got {other:?}"),
        }
    }

    #[test]
    fn exact_repeat_block_at_threshold_plus_one() {
        let mut det = LoopDetector::new(config_with_repeats(3));
        let args = json!({"cmd": "ls"});

        for _ in 0..3 {
            record_success(&mut det, "shell", &args, "output");
        }
        match record_success(&mut det, "shell", &args, "output") {
            LoopDetectionResult::Block(msg) => {
                assert!(msg.contains("shell"));
                assert!(msg.contains("4 times"));
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn exact_repeat_break_at_threshold_plus_two() {
        let mut det = LoopDetector::new(config_with_repeats(3));
        let args = json!({"q": "test"});

        for _ in 0..4 {
            record_success(&mut det, "search", &args, "no results");
        }
        match record_success(&mut det, "search", &args, "no results") {
            LoopDetectionResult::Break(msg) => {
                assert!(msg.contains("Circuit breaker"));
                assert!(msg.contains("search"));
            }
            other => panic!("expected Break, got {other:?}"),
        }
    }

    #[test]
    fn exact_repeat_resets_on_different_call() {
        let mut det = LoopDetector::new(config_with_repeats(3));
        let args = json!({"x": 1});

        record_success(&mut det, "tool_a", &args, "r1");
        record_success(&mut det, "tool_a", &args, "r1");
        // Interject a different tool — resets the consecutive exact-repeat streak.
        record_success(&mut det, "tool_b", &json!({}), "r2");
        record_success(&mut det, "tool_a", &args, "r1");
        // Only 2 consecutive exact repeats now; a different-result call must
        // not trip exact-repeat (and distinct results keep no-progress quiet).
        assert_eq!(
            record_success(&mut det, "tool_a", &json!({"x": 999}), "r_distinct"),
            LoopDetectionResult::Ok
        );
    }

    // ── Exact failure tests ─────────────────────────────────────

    #[test]
    fn exact_failure_warning_at_threshold() {
        let mut det = LoopDetector::new(config_with_failure_max(3));
        let args = json!({"cmd": "cargo test"});

        record_failure(&mut det, "shell", &args, "compiler error");
        record_failure(
            &mut det,
            "other",
            &json!({"path": "x"}),
            "unrelated failure",
        );
        record_failure(&mut det, "shell", &args, "compiler error");
        match record_failure(&mut det, "shell", &args, "compiler error") {
            LoopDetectionResult::Warning(msg) => {
                assert!(msg.contains("shell"), "got: {msg}");
                assert!(msg.contains("failed 3 times"), "got: {msg}");
                assert!(msg.contains("inspect the error"), "got: {msg}");
                assert!(!msg.contains("no progress"), "got: {msg}");
            }
            other => panic!("expected exact-failure Warning, got {other:?}"),
        }
    }

    #[test]
    fn exact_failure_block_and_break() {
        let mut det = LoopDetector::new(config_with_failure_max(3));
        let args = json!({"cmd": "cargo test"});

        for _ in 0..3 {
            record_failure(&mut det, "shell", &args, "compiler error");
        }
        match record_failure(&mut det, "shell", &args, "compiler error") {
            LoopDetectionResult::Block(msg) => {
                assert!(msg.contains("failed 4 times"), "got: {msg}");
                assert!(msg.contains("change strategy"), "got: {msg}");
            }
            other => panic!("expected exact-failure Block, got {other:?}"),
        }

        match record_failure(&mut det, "shell", &args, "compiler error") {
            LoopDetectionResult::Break(msg) => {
                assert!(msg.contains("failed 5 times"), "got: {msg}");
                assert!(msg.contains("Circuit breaker"), "got: {msg}");
                assert!(msg.contains("change strategy"), "got: {msg}");
            }
            other => panic!("expected exact-failure Break, got {other:?}"),
        }
    }

    #[test]
    fn exact_failure_is_windowed_not_consecutive() {
        let mut det = LoopDetector::new(config_with_failure_max(3));
        let args = json!({"cmd": "cargo test"});

        record_failure(&mut det, "shell", &args, "compiler error");
        record_success(
            &mut det,
            "file_read",
            &json!({"path": "src/lib.rs"}),
            "body",
        );
        record_failure(&mut det, "shell", &args, "compiler error");

        match record_failure(&mut det, "shell", &args, "compiler error") {
            LoopDetectionResult::Warning(msg) => {
                assert!(msg.contains("failed 3 times"), "got: {msg}");
            }
            other => panic!("expected windowed exact-failure Warning, got {other:?}"),
        }
    }

    #[test]
    fn exact_failure_ignores_successful_calls() {
        let mut det = LoopDetector::new(config_with_failure_max(3));
        let args = json!({"cmd": "cargo test"});

        record_failure(&mut det, "shell", &args, "compiler error");
        record_success(&mut det, "shell", &args, "ok");
        record_failure(&mut det, "shell", &args, "compiler error");

        assert_eq!(
            record_success(&mut det, "shell", &json!({"cmd": "cargo check"}), "ok"),
            LoopDetectionResult::Ok
        );
    }

    // ── Ping-pong tests ──────────────────────────────────────────

    #[test]
    fn ping_pong_warning_at_four_cycles() {
        let mut det = LoopDetector::new(default_config());
        let args = json!({});

        // 4 full cycles = 8 calls: A B A B A B A B
        for i in 0..8 {
            let name = if i % 2 == 0 { "read" } else { "write" };
            let result = record_success(&mut det, name, &args, &format!("r{i}"));
            if i < 7 {
                assert_eq!(result, LoopDetectionResult::Ok, "iteration {i}");
            } else {
                match result {
                    LoopDetectionResult::Warning(msg) => {
                        assert!(msg.contains("read"));
                        assert!(msg.contains("write"));
                        assert!(msg.contains("4 cycles"));
                    }
                    other => panic!("expected Warning at cycle 4, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn ping_pong_escalates_with_more_cycles() {
        let mut det = LoopDetector::new(default_config());
        let args = json!({});

        // 5 cycles = 10 calls.  The 10th call (completing cycle 5) triggers Block.
        for i in 0..10 {
            let name = if i % 2 == 0 { "fetch" } else { "parse" };
            record_success(&mut det, name, &args, &format!("r{i}"));
        }
        // 11th call extends to 5.5 cycles; detector still counts 5 full -> Block.
        let r = record_success(&mut det, "fetch", &args, "r10");
        match r {
            LoopDetectionResult::Block(msg) => {
                assert!(msg.contains("fetch"));
                assert!(msg.contains("parse"));
                assert!(msg.contains("5 cycles"));
            }
            other => panic!("expected Block at 5 cycles, got {other:?}"),
        }
    }

    #[test]
    fn ping_pong_not_triggered_for_same_tool() {
        let mut det = LoopDetector::new(default_config());
        let args = json!({});

        // Same tool repeated is not ping-pong.
        for _ in 0..10 {
            record_success(&mut det, "read", &args, "data");
        }
        // The exact_repeat detector fires, not ping_pong.
        // Verify by checking message content doesn't mention "alternating".
        let r = record_success(&mut det, "read", &args, "data");
        if let LoopDetectionResult::Break(msg) | LoopDetectionResult::Block(msg) = r {
            assert!(
                !msg.contains("alternating"),
                "should be exact-repeat, not ping-pong"
            );
        }
    }

    // ── No-progress tests ────────────────────────────────────────

    #[test]
    fn no_progress_warning_at_five_different_args_same_result() {
        let mut det = LoopDetector::new(default_config());

        for i in 0..5 {
            let args = json!({"query": format!("attempt_{i}")});
            let result = record_success(&mut det, "search", &args, "no results found");
            if i < 4 {
                assert_eq!(result, LoopDetectionResult::Ok, "iteration {i}");
            } else {
                match result {
                    LoopDetectionResult::Warning(msg) => {
                        assert!(msg.contains("search"));
                        assert!(msg.contains("identical results"));
                    }
                    other => panic!("expected Warning, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn no_progress_ignores_unknown_tools() {
        let mut det = LoopDetector::new(default_config());

        for i in 0..8 {
            let args = json!({"query": format!("attempt_{i}")});
            let result = record_success_with_side_effect(
                &mut det,
                "unclassified_search",
                &args,
                ToolSideEffect::Unknown,
                "same output",
            );
            assert_eq!(result, LoopDetectionResult::Ok, "iteration {i}");
        }
    }

    #[test]
    fn no_progress_ignores_mutating_tools() {
        let mut det = LoopDetector::new(default_config());

        for i in 0..8 {
            let args = json!({"path": format!("file_{i}.rs")});
            let result = record_success_with_side_effect(
                &mut det,
                "file_write",
                &args,
                ToolSideEffect::Mutating,
                "ok",
            );
            assert_eq!(result, LoopDetectionResult::Ok, "iteration {i}");
        }
    }

    #[test]
    fn no_progress_escalates_to_block_and_break() {
        let mut det = LoopDetector::new(LoopDetectorConfig {
            no_progress_hard_stop: true,
            ..Default::default()
        });

        // 6 calls with different args, same result.
        for i in 0..6 {
            let args = json!({"q": format!("v{i}")});
            record_success(&mut det, "web_fetch", &args, "timeout");
        }
        // 7th call: count=7 which is >= MIN_CALLS(5)+2 -> Break.
        let r7 = record_success(&mut det, "web_fetch", &json!({"q": "v6"}), "timeout");
        match r7 {
            LoopDetectionResult::Break(msg) => {
                assert!(msg.contains("web_fetch"));
                assert!(msg.contains("7 times"));
                assert!(msg.contains("no progress"));
            }
            other => panic!("expected Break at 7 calls, got {other:?}"),
        }
    }

    #[test]
    fn no_progress_hard_stop_is_opt_in_by_default() {
        let mut det = LoopDetector::new(default_config());

        for i in 0..6 {
            let args = json!({"q": format!("v{i}")});
            record_success(&mut det, "web_fetch", &args, "timeout");
        }

        let r7 = record_success(&mut det, "web_fetch", &json!({"q": "v6"}), "timeout");
        match r7 {
            LoopDetectionResult::Block(msg) => {
                assert!(msg.contains("web_fetch"));
                assert!(msg.contains("7 times"));
                assert!(msg.contains("identical results"));
            }
            other => panic!("expected Block when hard stop is disabled, got {other:?}"),
        }
    }

    #[test]
    fn no_progress_not_triggered_when_results_differ() {
        let mut det = LoopDetector::new(default_config());

        for i in 0..8 {
            let args = json!({"q": format!("v{i}")});
            let result = record_success(&mut det, "search", &args, &format!("result_{i}"));
            assert_eq!(result, LoopDetectionResult::Ok, "iteration {i}");
        }
    }

    #[test]
    fn no_progress_triggered_when_interleaved_with_other_calls() {
        // same tool + same result repeated non-consecutively, with
        // varied unrelated calls interleaved, must still be detected. The old
        // take_while logic reset the streak on any interleaved call.
        let mut det = LoopDetector::new(default_config());

        let mut last = LoopDetectionResult::Ok;
        for i in 0..5 {
            let args = json!({"q": format!("v{i}")});
            last = record_success(&mut det, "search", &args, "no results found");
            // Interleave a distinct unrelated tool each time so neither
            // ping-pong nor exact-repeat fires before no-progress.
            record_success(
                &mut det,
                &format!("reader_{i}"),
                &json!({"path": format!("/f{i}")}),
                &format!("body_{i}"),
            );
        }

        match last {
            LoopDetectionResult::Warning(msg) => {
                assert!(msg.contains("search"), "got: {msg}");
                assert!(msg.contains("identical results"), "got: {msg}");
            }
            other => panic!("expected Warning on 5th interleaved probe, got {other:?}"),
        }
    }

    #[test]
    fn no_progress_not_triggered_when_all_args_identical() {
        // If args are all the same, exact_repeat should fire, not no_progress.
        let mut det = LoopDetector::new(config_with_repeats(6));
        let args = json!({"q": "same"});

        for _ in 0..5 {
            record_success(&mut det, "search", &args, "no results");
        }
        // 6th call = exact repeat at threshold (max_repeats=6) -> Warning.
        // no_progress requires >=2 unique args, so it must NOT fire.
        let r = record_success(&mut det, "search", &args, "no results");
        match r {
            LoopDetectionResult::Warning(msg) => {
                assert!(
                    msg.contains("identical arguments"),
                    "should be exact-repeat Warning, got: {msg}"
                );
            }
            other => panic!("expected exact-repeat Warning, got {other:?}"),
        }
    }

    #[test]
    fn failed_records_do_not_trigger_no_progress() {
        let mut det = LoopDetector::new(default_config());

        for i in 0..8 {
            let args = json!({"q": format!("v{i}")});
            let result = record_failure(&mut det, "web_fetch", &args, "timeout");
            assert_eq!(result, LoopDetectionResult::Ok, "iteration {i}");
        }

        assert!(
            det.window.iter().any(|record| !record.success),
            "failed calls should still be recorded for outcome accounting"
        );
    }

    // ── Disabled / config tests ──────────────────────────────────

    #[test]
    fn disabled_detector_always_returns_ok() {
        let config = LoopDetectorConfig {
            enabled: false,
            ..Default::default()
        };
        let mut det = LoopDetector::new(config);
        let args = json!({"x": 1});

        for _ in 0..20 {
            assert_eq!(
                record_success(&mut det, "tool", &args, "same"),
                LoopDetectionResult::Ok
            );
        }
    }

    #[test]
    fn window_size_limits_memory() {
        let config = LoopDetectorConfig {
            enabled: true,
            window_size: 5,
            max_repeats: 3,
            ..Default::default()
        };
        let mut det = LoopDetector::new(config);
        let args = json!({"x": 1});

        // Fill window with 5 different tools.
        for i in 0..5 {
            record_success(&mut det, &format!("tool_{i}"), &args, "result");
        }
        assert_eq!(det.window.len(), 5);

        // Adding one more evicts the oldest.
        record_success(&mut det, "tool_5", &args, "result");
        assert_eq!(det.window.len(), 5);
        assert_eq!(det.window.front().unwrap().name, "tool_1");
    }

    // ── Ping-pong with varying args ─────────────────────────────

    #[test]
    fn ping_pong_detects_alternation_with_varying_args() {
        let mut det = LoopDetector::new(default_config());

        // A->B->A->B with different args each time — ping-pong cares only
        // about tool names, not argument equality.
        for i in 0..8 {
            let name = if i % 2 == 0 { "read" } else { "write" };
            let args = json!({"attempt": i});
            let result = record_success(&mut det, name, &args, &format!("r{i}"));
            if i < 7 {
                assert_eq!(result, LoopDetectionResult::Ok, "iteration {i}");
            } else {
                match result {
                    LoopDetectionResult::Warning(msg) => {
                        assert!(msg.contains("read"));
                        assert!(msg.contains("write"));
                        assert!(msg.contains("4 cycles"));
                    }
                    other => panic!("expected Warning at cycle 4, got {other:?}"),
                }
            }
        }
    }

    // ── Window eviction test ────────────────────────────────────

    #[test]
    fn window_eviction_prevents_stale_pattern_detection() {
        let config = LoopDetectorConfig {
            enabled: true,
            window_size: 6,
            max_repeats: 3,
            ..Default::default()
        };
        let mut det = LoopDetector::new(config);
        let args = json!({"x": 1});

        // 2 consecutive calls of "tool_a".
        record_success(&mut det, "tool_a", &args, "r");
        record_success(&mut det, "tool_a", &args, "r");

        // Fill the rest of the window with different tools (evicting the
        // first "tool_a" calls as the window is only 6).
        for i in 0..5 {
            record_success(&mut det, &format!("other_{i}"), &json!({}), "ok");
        }

        // Now "tool_a" again — only 1 consecutive, not 3.
        let r = record_success(&mut det, "tool_a", &args, "r");
        assert_eq!(
            r,
            LoopDetectionResult::Ok,
            "stale entries should be evicted"
        );
    }

    // ── hash_value key-order independence ────────────────────────

    #[test]
    fn hash_value_is_key_order_independent() {
        let a = json!({"alpha": 1, "beta": 2});
        let b = json!({"beta": 2, "alpha": 1});
        assert_eq!(
            hash_value(&a),
            hash_value(&b),
            "hash_value must produce identical hashes regardless of JSON key order"
        );
    }

    #[test]
    fn hash_value_nested_key_order_independent() {
        let a = json!({"outer": {"x": 1, "y": 2}, "z": [1, 2]});
        let b = json!({"z": [1, 2], "outer": {"y": 2, "x": 1}});
        assert_eq!(
            hash_value(&a),
            hash_value(&b),
            "nested objects must also be key-order independent"
        );
    }

    // ── Escalation order tests ───────────────────────────────────

    #[test]
    fn exact_repeat_takes_priority_over_no_progress() {
        // If tool+args are identical, exact_repeat fires before no_progress.
        let mut det = LoopDetector::new(config_with_repeats(3));
        let args = json!({"q": "same"});

        record_success(&mut det, "s", &args, "r");
        record_success(&mut det, "s", &args, "r");
        let r = record_success(&mut det, "s", &args, "r");
        match r {
            LoopDetectionResult::Warning(msg) => {
                assert!(msg.contains("identical arguments"));
            }
            other => panic!("expected exact-repeat Warning, got {other:?}"),
        }
    }
}

//! Default-off post-edit verification stage.

use crate::agent::tool_execution::ToolExecutionOutcome;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use zeroclaw_config::coding::{CodingConfig, VerifyConfig};
use zeroclaw_tool_call_parser::ParsedToolCall;

pub(crate) enum VerifyDecision {
    Pass,
    Repair { payload: String },
    Surface { payload: String },
}

#[derive(Default)]
pub(crate) struct BaselineStore {
    diagnostics: HashMap<TargetKey, HashSet<String>>,
}

pub(crate) async fn capture_verify_baselines(
    coding: &CodingConfig,
    calls: &[ParsedToolCall],
    baselines: &mut BaselineStore,
) {
    let cfg = &coding.verify;
    if !cfg.enabled || !cfg.baseline_delta {
        return;
    }

    for target in edited_targets_from_calls(calls, cfg) {
        let key = target.key();
        if baselines.diagnostics.contains_key(&key) {
            continue;
        }
        let Some(command) = command_for_target(cfg, &target) else {
            continue;
        };

        let output = run_verify_command(command, &target.package_root, cfg.timeout_secs).await;
        baselines
            .diagnostics
            .insert(key, diagnostic_set(&output.text));
    }
}

pub(crate) async fn run_verify_stage(
    coding: &CodingConfig,
    calls: &[ParsedToolCall],
    ordered_results: &[Option<(String, Option<String>, ToolExecutionOutcome)>],
    baselines: &mut BaselineStore,
    repair_budget_left: usize,
) -> VerifyDecision {
    let cfg = &coding.verify;
    if !cfg.enabled {
        return VerifyDecision::Pass;
    }

    let targets = edited_targets_from_results(calls, ordered_results, cfg);
    if targets.is_empty() {
        return VerifyDecision::Pass;
    }

    let mut failures = Vec::new();
    for target in targets {
        let Some(command) = command_for_target(cfg, &target) else {
            continue;
        };

        let output = run_verify_command(command, &target.package_root, cfg.timeout_secs).await;
        if output.success {
            continue;
        }

        let text = if cfg.baseline_delta {
            let post = diagnostic_set(&output.text);
            let baseline = baselines.diagnostics.entry(target.key()).or_default();
            let mut delta = delta_diagnostics(baseline, &post);
            if delta.is_empty() {
                continue;
            }
            delta.sort();
            delta.join("\n")
        } else {
            output.text
        };

        failures.push(render_verify_failure(command, &target.package_root, &text));
    }

    if failures.is_empty() {
        return VerifyDecision::Pass;
    }

    let payload = failures.join("\n\n");
    if repair_budget_left > 0 {
        VerifyDecision::Repair { payload }
    } else {
        VerifyDecision::Surface { payload }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TargetKey {
    language: String,
    package_root: PathBuf,
}

struct EditedTarget {
    language: String,
    package_root: PathBuf,
}

impl EditedTarget {
    fn key(&self) -> TargetKey {
        TargetKey {
            language: self.language.clone(),
            package_root: self.package_root.clone(),
        }
    }
}

struct RawVerifyOutput {
    success: bool,
    text: String,
}

fn edited_targets_from_results(
    calls: &[ParsedToolCall],
    ordered_results: &[Option<(String, Option<String>, ToolExecutionOutcome)>],
    cfg: &VerifyConfig,
) -> Vec<EditedTarget> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    for (idx, slot) in ordered_results.iter().enumerate() {
        let Some((tool_name, _, outcome)) = slot else {
            continue;
        };
        if !outcome.success || !cfg.on.iter().any(|name| name == tool_name) {
            continue;
        }

        let Some(target) = calls.get(idx).and_then(|call| target_from_call(call, cfg)) else {
            continue;
        };

        if seen.insert(target.key()) {
            targets.push(target);
        }
    }

    targets
}

fn edited_targets_from_calls(calls: &[ParsedToolCall], cfg: &VerifyConfig) -> Vec<EditedTarget> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    for call in calls {
        let Some(target) = target_from_call(call, cfg) else {
            continue;
        };
        if seen.insert(target.key()) {
            targets.push(target);
        }
    }
    targets
}

fn target_from_call(call: &ParsedToolCall, cfg: &VerifyConfig) -> Option<EditedTarget> {
    if !cfg.on.iter().any(|name| name == &call.name) {
        return None;
    }

    let path = path_arg(&call.arguments).map(PathBuf::from)?;
    let language = language_for_path(&path)?;
    if !cfg.commands.contains_key(language) {
        return None;
    }

    Some(EditedTarget {
        language: language.to_string(),
        package_root: package_root_for(&path),
    })
}

fn command_for_target<'a>(cfg: &'a VerifyConfig, target: &EditedTarget) -> Option<&'a str> {
    cfg.commands
        .get(&target.language)
        .map(String::as_str)
        .filter(|cmd| !cmd.trim().is_empty())
}

fn path_arg(args: &Value) -> Option<&str> {
    args.get("path")
        .or_else(|| args.get("file_path"))
        .or_else(|| args.get("filename"))
        .and_then(Value::as_str)
}

fn language_for_path(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("rs") => Some("rust"),
        Some("ts" | "tsx") => Some("typescript"),
        Some("js" | "jsx") => Some("javascript"),
        Some("py") => Some("python"),
        _ => None,
    }
}

fn diagnostic_set(text: &str) -> HashSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn delta_diagnostics(baseline: &HashSet<String>, post: &HashSet<String>) -> Vec<String> {
    post.difference(baseline).cloned().collect()
}

fn package_root_for(path: &Path) -> PathBuf {
    let start = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or_else(|| Path::new("."))
    };
    for ancestor in start.ancestors() {
        if ancestor.join("Cargo.toml").exists()
            || ancestor.join("package.json").exists()
            || ancestor.join("pyproject.toml").exists()
        {
            return ancestor.to_path_buf();
        }
    }
    start.to_path_buf()
}

async fn run_verify_command(command: &str, cwd: &Path, timeout_secs: u64) -> RawVerifyOutput {
    let mut child = match Command::new("sh")
        .arg("-lc")
        .arg(command)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            return RawVerifyOutput {
                success: false,
                text: format!("failed to start verify command: {err}"),
            };
        }
    };

    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let wait = child.wait();
    let timeout = Duration::from_secs(timeout_secs.max(1));
    let status = match tokio::time::timeout(timeout, wait).await {
        Ok(Ok(status)) => status,
        Ok(Err(err)) => {
            return RawVerifyOutput {
                success: false,
                text: format!("failed to wait for verify command: {err}"),
            };
        }
        Err(_) => {
            let _ = child.kill().await;
            return RawVerifyOutput {
                success: false,
                text: format!("verify command timed out after {timeout_secs}s"),
            };
        }
    };

    let mut text = String::new();
    if let Some(mut out) = stdout.take() {
        let _ = out.read_to_string(&mut text).await;
    }
    if let Some(mut err) = stderr.take() {
        if !text.is_empty() {
            text.push('\n');
        }
        let _ = err.read_to_string(&mut text).await;
    }

    RawVerifyOutput {
        success: status.success(),
        text,
    }
}

fn render_verify_failure(command: &str, cwd: &Path, text: &str) -> String {
    let trimmed = text.trim();
    let body = if trimmed.is_empty() {
        "<no output>".to_string()
    } else {
        trimmed.chars().take(8_000).collect()
    };
    format!(
        "[Verification failed]\ncommand: {command}\ncwd: {}\n\n{body}",
        cwd.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    fn outcome(success: bool) -> ToolExecutionOutcome {
        ToolExecutionOutcome {
            output: String::new(),
            output_data: None,
            success,
            error_reason: None,
            diagnostics: None,
            duration: Duration::ZERO,
            receipt: None,
        }
    }

    fn cfg() -> VerifyConfig {
        let mut cfg = VerifyConfig {
            enabled: true,
            ..VerifyConfig::default()
        };
        cfg.commands.insert("rust".into(), "cargo check".into());
        cfg
    }

    #[test]
    fn edited_target_uses_successful_configured_edit() {
        let call = ParsedToolCall {
            name: "file_edit".into(),
            arguments: serde_json::json!({"path": "src/lib.rs"}),
            tool_call_id: None,
        };
        let results = vec![Some(("file_edit".into(), None, outcome(true)))];

        let targets = edited_targets_from_results(&[call], &results, &cfg());

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].language, "rust");
    }

    #[test]
    fn edited_target_ignores_failed_edit() {
        let call = ParsedToolCall {
            name: "file_edit".into(),
            arguments: serde_json::json!({"path": "src/lib.rs"}),
            tool_call_id: None,
        };
        let results = vec![Some(("file_edit".into(), None, outcome(false)))];

        assert!(edited_targets_from_results(&[call], &results, &cfg()).is_empty());
    }

    #[test]
    fn edited_targets_dedupe_by_package_and_language() {
        let calls = vec![
            ParsedToolCall {
                name: "file_edit".into(),
                arguments: serde_json::json!({"path": "src/lib.rs"}),
                tool_call_id: None,
            },
            ParsedToolCall {
                name: "file_write".into(),
                arguments: serde_json::json!({"path": "src/main.rs"}),
                tool_call_id: None,
            },
        ];
        let results = vec![
            Some(("file_edit".into(), None, outcome(true))),
            Some(("file_write".into(), None, outcome(true))),
        ];

        let targets = edited_targets_from_results(&calls, &results, &cfg());

        assert_eq!(targets.len(), 1);
    }

    #[test]
    fn delta_filters_pre_existing_diagnostics() {
        let baseline = diagnostic_set("error[E0425]: cannot find value `old`\nwarning: old");
        let post = diagnostic_set(
            "error[E0425]: cannot find value `old`\nwarning: old\nerror[E0308]: new mismatch",
        );

        let delta = delta_diagnostics(&baseline, &post);

        assert_eq!(delta, vec!["error[E0308]: new mismatch"]);
    }

    #[test]
    fn render_verify_failure_caps_payload() {
        let text = "x".repeat(9_000);

        let rendered = render_verify_failure("cargo check", Path::new("."), &text);

        assert!(rendered.len() < 8_100);
        assert!(rendered.contains("[Verification failed]"));
    }

    #[tokio::test]
    async fn run_verify_stage_surfaces_only_new_diagnostics_with_baseline_delta() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname='x'\nversion='0.0.0'\n",
        )
        .expect("write Cargo.toml");
        fs::create_dir(temp.path().join("src")).expect("create src");
        let edited_path = temp.path().join("src/lib.rs");
        fs::write(&edited_path, "").expect("write edited file");
        fs::write(temp.path().join("diagnostics.txt"), "old failure\n").expect("write baseline");

        let mut coding = CodingConfig { verify: cfg() };
        coding
            .verify
            .commands
            .insert("rust".into(), "cat diagnostics.txt; exit 1".into());
        let calls = vec![ParsedToolCall {
            name: "file_edit".into(),
            arguments: serde_json::json!({"path": edited_path}),
            tool_call_id: None,
        }];
        let mut baselines = BaselineStore::default();

        capture_verify_baselines(&coding, &calls, &mut baselines).await;
        fs::write(
            temp.path().join("diagnostics.txt"),
            "old failure\nnew failure\n",
        )
        .expect("write post diagnostics");
        let results = vec![Some(("file_edit".into(), None, outcome(true)))];

        let decision = run_verify_stage(&coding, &calls, &results, &mut baselines, 1).await;

        match decision {
            VerifyDecision::Repair { payload } => {
                assert!(payload.contains("new failure"));
                assert!(!payload.contains("old failure"));
            }
            VerifyDecision::Pass | VerifyDecision::Surface { .. } => {
                panic!("expected repair decision with new diagnostic")
            }
        }
    }

    #[tokio::test]
    async fn run_verify_stage_surfaces_when_repair_budget_exhausted() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname='x'\nversion='0.0.0'\n",
        )
        .expect("write Cargo.toml");
        fs::create_dir(temp.path().join("src")).expect("create src");
        let edited_path = temp.path().join("src/lib.rs");
        fs::write(&edited_path, "").expect("write edited file");

        let mut coding = CodingConfig { verify: cfg() };
        coding.verify.baseline_delta = false;
        coding
            .verify
            .commands
            .insert("rust".into(), "printf 'persistent failure'; exit 1".into());
        let calls = vec![ParsedToolCall {
            name: "file_edit".into(),
            arguments: serde_json::json!({"path": edited_path}),
            tool_call_id: None,
        }];
        let results = vec![Some(("file_edit".into(), None, outcome(true)))];
        let mut baselines = BaselineStore::default();

        let decision = run_verify_stage(&coding, &calls, &results, &mut baselines, 0).await;

        match decision {
            VerifyDecision::Surface { payload } => assert!(payload.contains("persistent failure")),
            VerifyDecision::Pass | VerifyDecision::Repair { .. } => {
                panic!("expected surface decision after budget exhaustion")
            }
        }
    }
}

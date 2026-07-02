use async_trait::async_trait;
use serde_json::json;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use zeroclaw_api::diagnostics::{Diagnostic, DiagnosticSeverity};
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult, with_ephemeral_workspace_warning};
use zeroclaw_config::policy::SecurityPolicy;

use crate::code_intel::{CodeIntel, Span, SymbolDef, SymbolRef};
use crate::workspace_locks::WorkspaceFileLocks;

/// Edit a file by replacing an exact string match with new content.
///
/// Uses `old_string` → `new_string` precise replacement within the workspace.
/// The `old_string` must appear exactly once in the file (zero matches = not
/// found, multiple matches = ambiguous). `new_string` may be empty to delete
/// the matched text. Security checks mirror [`super::file_write::FileWriteTool`].
pub struct FileEditTool {
    security: Arc<SecurityPolicy>,
    code_intel: Option<Arc<CodeIntel>>,
    /// Whether edits to the workspace persist on the host filesystem. `false`
    /// on an ephemeral runtime (Docker tmpfs / no volume mount), where the
    /// rewritten file succeeds inside the container but is invisible on the
    /// host and discarded at session end. When `false`, successful edits carry
    /// a loud ephemeral-workspace warning. Mirrors
    /// [`super::file_write::FileWriteTool`]. See issue #4627.
    persistent_writes: bool,
    workspace_locks: Option<WorkspaceFileLocks>,
}

impl FileEditTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self {
            security,
            code_intel: None,
            persistent_writes: true,
            workspace_locks: None,
        }
    }

    /// Construct with an explicit persistence flag derived from the active
    /// runtime adapter's `has_filesystem_access()`. Mirrors
    /// [`super::file_write::FileWriteTool::new_with_persistence`].
    pub fn new_with_persistence(security: Arc<SecurityPolicy>, persistent_writes: bool) -> Self {
        Self {
            security,
            code_intel: None,
            persistent_writes,
            workspace_locks: None,
        }
    }

    pub fn with_code_intel(mut self, code_intel: Option<Arc<CodeIntel>>) -> Self {
        self.code_intel = code_intel;
        self
    }

    pub fn with_workspace_locks(mut self, workspace_locks: Option<WorkspaceFileLocks>) -> Self {
        self.workspace_locks = workspace_locks;
        self
    }
}

#[async_trait]
impl Tool for FileEditTool {
    fn name(&self) -> &str {
        "file_edit"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing an exact string match with new content"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file. Relative paths resolve from workspace; outside paths require policy allowlist."
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact text to find and replace (must appear exactly once in the file)"
                },
                "new_string": {
                    "type": "string",
                    "description": "The replacement text (empty string to delete the matched text)"
                }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let mut result = self.edit_file(args).await?;
        // A successful edit on an ephemeral runtime rewrites a file that never
        // reaches the host and is lost at session end; warn loudly
        if !self.persistent_writes && result.success {
            result.output = with_ephemeral_workspace_warning(&result.output).into();
        }
        Ok(result)
    }
}

impl FileEditTool {
    /// Perform the exact-string replacement edit. The ephemeral workspace
    /// warning is applied by the `Tool::execute` wrapper above.
    async fn edit_file(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // ── 1. Extract parameters ──────────────────────────────────
        let path = args.get("path").and_then(|v| v.as_str()).ok_or_else(|| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"param": "path"})),
                "file_edit: missing path parameter"
            );
            anyhow::Error::msg("Missing 'path' parameter")
        })?;

        let old_string = args
            .get("old_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"param": "old_string"})),
                    "file_edit: missing old_string parameter"
                );
                anyhow::Error::msg("Missing 'old_string' parameter")
            })?;

        let new_string = args
            .get("new_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"param": "new_string"})),
                    "file_edit: missing new_string parameter"
                );
                anyhow::Error::msg("Missing 'new_string' parameter")
            })?;

        if old_string.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("old_string must not be empty".into()),
                diagnostics: None,
            });
        }

        // ── 2. Autonomy check ──────────────────────────────────────
        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("Action blocked: autonomy is read-only".into()),
                diagnostics: None,
            });
        }

        // Rate limiting and path-allowlist checks are applied by the
        // RateLimitedTool + PathGuardedTool wrappers at registration time
        // (see zeroclaw-runtime::tools::mod).

        let full_path = self.security.resolve_tool_path(path);

        // ── 5. Canonicalise parent ─────────────────────────────────
        let Some(parent) = full_path.parent() else {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("Invalid path: missing parent directory".into()),
                diagnostics: None,
            });
        };

        let resolved_parent = match tokio::fs::canonicalize(parent).await {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!("Failed to resolve file path: {e}")),
                    diagnostics: None,
                });
            }
        };

        // ── 6. Resolved path post-validation ───────────────────────
        if !self.security.is_resolved_path_allowed(&resolved_parent) {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(
                    self.security
                        .resolved_path_violation_message(&resolved_parent),
                ),
                diagnostics: None,
            });
        }

        let Some(file_name) = full_path.file_name() else {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("Invalid path: missing file name".into()),
                diagnostics: None,
            });
        };

        let resolved_target = resolved_parent.join(file_name);
        let _write_guard = if let Some(locks) = &self.workspace_locks {
            Some(locks.lock_path(&resolved_target).await)
        } else {
            None
        };

        if self.security.is_runtime_config_path(&resolved_target) {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(
                    self.security
                        .runtime_config_violation_message(&resolved_target),
                ),
                diagnostics: None,
            });
        }

        // ── 7. Symlink check ───────────────────────────────────────
        if let Ok(meta) = tokio::fs::symlink_metadata(&resolved_target).await
            && meta.file_type().is_symlink()
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "Refusing to edit through symlink: {}",
                    resolved_target.display()
                )),
                diagnostics: None,
            });
        }

        // ── 9. Read → match → replace → write ─────────────────────
        let content = match tokio::fs::read_to_string(&resolved_target).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(format!("Failed to read file: {e}")),
                    diagnostics: None,
                });
            }
        };

        let match_count = content.matches(old_string).count();

        if match_count == 0 {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(no_match_diagnostic(&content, old_string)),
                diagnostics: None,
            });
        }

        if match_count > 1 {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "old_string matches {match_count} times; must match exactly once"
                )),
                diagnostics: None,
            });
        }

        let pre_edit_defs = self.pre_edit_defs(&resolved_target, &content, old_string);
        let new_content = content.replacen(old_string, new_string, 1);

        match tokio::fs::write(&resolved_target, &new_content).await {
            Ok(()) => {
                if let Some(code_intel) = &self.code_intel {
                    code_intel.invalidate(&resolved_target);
                }
                let mut output = format!(
                    "Edited {path}: replaced 1 occurrence ({} bytes)",
                    new_content.len()
                );
                let diagnostics = self.post_edit_symbol_diagnostics(&pre_edit_defs);
                if let Some(note) = post_edit_symbol_note(&diagnostics) {
                    output.push_str("\n\n");
                    output.push_str(&note);
                }
                Ok(ToolResult {
                    success: true,
                    output: output.into(),
                    error: None,
                    diagnostics: (!diagnostics.is_empty()).then_some(diagnostics),
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Failed to write file: {e}")),
                diagnostics: None,
            }),
        }
    }

    fn pre_edit_defs(&self, path: &Path, content: &str, old_string: &str) -> Vec<SymbolDef> {
        let Some(code_intel) = self
            .code_intel
            .as_ref()
            .filter(|code_intel| code_intel.post_edit_check_enabled())
        else {
            return Vec::new();
        };
        let Some(span) = span_for_match(path, content, old_string) else {
            return Vec::new();
        };
        code_intel.symbols_in_span(path, &span).unwrap_or_default()
    }

    fn post_edit_symbol_diagnostics(&self, pre_edit_defs: &[SymbolDef]) -> Vec<Diagnostic> {
        let code_intel = self
            .code_intel
            .as_ref()
            .filter(|code_intel| code_intel.post_edit_check_enabled());
        let Some(code_intel) = code_intel else {
            return Vec::new();
        };
        let mut seen = HashSet::new();
        let mut diagnostics = Vec::new();
        for def in pre_edit_defs {
            if !seen.insert(def.name.clone()) {
                continue;
            }
            if !code_intel
                .find_definition(&def.name)
                .unwrap_or_default()
                .is_empty()
            {
                continue;
            }
            let references = code_intel.find_references(&def.name).unwrap_or_default();
            if references.is_empty() {
                continue;
            }
            diagnostics.push(unresolved_symbol_diagnostic(&def.name, &references));
        }
        diagnostics
    }
}

fn span_for_match(path: &Path, content: &str, needle: &str) -> Option<Span> {
    let start = content.find(needle)?;
    Some(span_for_byte_range(
        path,
        content,
        start,
        start + needle.len(),
    ))
}

fn span_for_byte_range(path: &Path, content: &str, start: usize, end: usize) -> Span {
    let (start_line, start_col) = line_col_for_byte(content, start);
    let (end_line, end_col) = line_col_for_byte(content, end);
    Span {
        path: path.to_path_buf(),
        start_line,
        start_col,
        end_line,
        end_col,
    }
}

fn line_col_for_byte(content: &str, offset: usize) -> (u32, u32) {
    let prefix = &content[..offset];
    let line = prefix
        .as_bytes()
        .iter()
        .filter(|byte| **byte == b'\n')
        .count() as u32
        + 1;
    let col = prefix
        .rsplit_once('\n')
        .map_or(prefix.len(), |(_, tail)| tail.len()) as u32;
    (line, col)
}

fn unresolved_symbol_diagnostic(name: &str, references: &[SymbolRef]) -> Diagnostic {
    let first = &references[0].span;
    Diagnostic {
        file: first.path.display().to_string(),
        line: Some(first.start_line),
        col: Some(first.start_col),
        severity: DiagnosticSeverity::Error,
        code: Some("semantic.unresolved_symbol".to_string()),
        message: format!(
            "`{name}` has {} remaining reference(s) and no indexed definition after this edit",
            references.len()
        ),
    }
}

fn post_edit_symbol_note(diagnostics: &[Diagnostic]) -> Option<String> {
    if diagnostics.is_empty() {
        return None;
    }
    Some(format!(
        "## Post-edit symbol check\nPotential unresolved references after this edit:\n{}",
        diagnostics
            .iter()
            .map(format_unresolved_symbol)
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

fn format_unresolved_symbol(diagnostic: &Diagnostic) -> String {
    let line = diagnostic.line.unwrap_or(0);
    let col = diagnostic.col.unwrap_or(0);
    format!(
        "- {} (first: {}:{line}:{col})",
        diagnostic.message, diagnostic.file
    )
}

fn no_match_diagnostic(content: &str, old_string: &str) -> String {
    fn strip_leading_ws(s: &str) -> String {
        s.lines()
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n")
    }

    let needle_norm = strip_leading_ws(old_string);
    let haystack_norm = strip_leading_ws(content);
    let near = haystack_norm.matches(needle_norm.as_str()).count();

    match near {
        0 => "old_string not found in file".to_string(),
        1 => "old_string not found exactly: a block matching it ignoring leading \
              whitespace exists exactly once. The difference is indentation \
              (width, or tabs vs spaces). Re-read the target region and copy its \
              leading whitespace exactly, then retry."
            .to_string(),
        n => format!(
            "old_string not found exactly: {n} blocks match it when leading \
             whitespace is ignored. Indentation differs and the target is \
             ambiguous. Re-read the region, copy exact indentation, and include \
             enough surrounding lines to make the match unique."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_intel::CodeIntel;
    use crate::wrappers::{PathGuardedTool, RateLimitedTool};
    use zeroclaw_config::autonomy::AutonomyLevel;
    use zeroclaw_config::coding::CodeIntelConfig;
    use zeroclaw_config::policy::SecurityPolicy;

    fn test_tool(workspace: std::path::PathBuf) -> FileEditTool {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: workspace,
            ..SecurityPolicy::default()
        });
        FileEditTool::new(security)
    }

    fn test_code_intel(workspace: std::path::PathBuf, post_edit_check: bool) -> Arc<CodeIntel> {
        let cfg = CodeIntelConfig {
            enabled: true,
            post_edit_check,
            ..CodeIntelConfig::default()
        };
        Arc::new(CodeIntel::new(workspace, Arc::new(move || cfg.clone())))
    }

    fn test_tool_with_code_intel(
        workspace: std::path::PathBuf,
        code_intel: Arc<CodeIntel>,
    ) -> FileEditTool {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: workspace,
            ..SecurityPolicy::default()
        });
        FileEditTool::new(security).with_code_intel(Some(code_intel))
    }

    #[cfg(target_os = "windows")]
    fn absolute_path_outside_workspace() -> &'static str {
        r"C:\Windows\win.ini"
    }

    #[cfg(not(target_os = "windows"))]
    fn absolute_path_outside_workspace() -> &'static str {
        "/etc/passwd"
    }

    /// Wraps `FileEditTool` with the production `PathGuardedTool` + `RateLimitedTool`
    /// stack, mirroring the registration in `zeroclaw-runtime::tools::mod`. Use this
    /// in tests that exercise path-allowlist or rate-limit behavior.
    fn wrapped_tool(workspace: std::path::PathBuf) -> Box<dyn Tool> {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: workspace,
            ..SecurityPolicy::default()
        });
        Box::new(RateLimitedTool::new(
            PathGuardedTool::new(FileEditTool::new(security.clone()), security.clone()),
            security,
        ))
    }

    fn test_tool_with(
        workspace: std::path::PathBuf,
        autonomy: AutonomyLevel,
        max_actions_per_hour: u32,
    ) -> FileEditTool {
        let security = Arc::new(SecurityPolicy {
            autonomy,
            workspace_dir: workspace,
            max_actions_per_hour,
            ..SecurityPolicy::default()
        });
        FileEditTool::new(security)
    }

    fn ephemeral_tool(workspace: std::path::PathBuf) -> FileEditTool {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: workspace,
            ..SecurityPolicy::default()
        });
        FileEditTool::new_with_persistence(security, false)
    }

    #[test]
    fn file_edit_name() {
        let tool = test_tool(std::env::temp_dir());
        assert_eq!(tool.name(), "file_edit");
    }

    // ── Ephemeral-workspace warning────────────────

    #[tokio::test]
    async fn file_edit_warns_on_ephemeral_workspace() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_edit_ephemeral");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("doc.txt"), "hello world")
            .await
            .unwrap();

        let tool = ephemeral_tool(dir.clone());
        let result = tool
            .execute(json!({"path": "doc.txt", "old_string": "world", "new_string": "there"}))
            .await
            .unwrap();
        assert!(result.success, "error: {:?}", result.error);
        assert!(
            result.output.contains("EPHEMERAL WORKSPACE"),
            "ephemeral warning must be present, got: {}",
            result.output
        );
        assert!(result.output.contains("mount_workspace"));
        assert!(
            result.output.contains("Edited"),
            "original edit status must be preserved, got: {}",
            result.output
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_edit_failure_not_warned_on_ephemeral_workspace() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_edit_ephemeral_fail");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("doc.txt"), "hello world")
            .await
            .unwrap();

        let tool = ephemeral_tool(dir.clone());
        let result = tool
            .execute(json!({"path": "doc.txt", "old_string": "absent", "new_string": "x"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(!result.output.contains("EPHEMERAL WORKSPACE"));
        assert!(
            !result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("EPHEMERAL WORKSPACE")
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_edit_no_warning_when_persistent() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_edit_persistent");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("doc.txt"), "hello world")
            .await
            .unwrap();

        let tool = test_tool(dir.clone());
        let result = tool
            .execute(json!({"path": "doc.txt", "old_string": "world", "new_string": "there"}))
            .await
            .unwrap();
        assert!(result.success, "error: {:?}", result.error);
        assert!(
            !result.output.contains("EPHEMERAL WORKSPACE"),
            "no ephemeral warning expected on a persistent runtime, got: {}",
            result.output
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn no_match_diagnostic_flags_unique_whitespace_only_difference() {
        // File uses 4-space indent; old_string uses 5-space. Same content
        // otherwise — the diagnostic must point at indentation, not say "not found".
        let content = "fn main() {\n    let x = 1;\n}\n";
        let old = "     let x = 1;";
        let msg = no_match_diagnostic(content, old);
        assert!(msg.contains("ignoring leading whitespace"), "got: {msg}");
        assert!(msg.contains("indentation"), "got: {msg}");
    }

    #[test]
    fn no_match_diagnostic_plain_not_found_when_no_near_match() {
        let content = "fn main() {}\n";
        let msg = no_match_diagnostic(content, "totally unrelated text");
        assert_eq!(msg, "old_string not found in file");
    }

    #[test]
    fn no_match_diagnostic_flags_ambiguous_whitespace_matches() {
        let content = "    a = 1;\n        a = 1;\n";
        let msg = no_match_diagnostic(content, "a = 1;");
        assert!(msg.contains("blocks match"), "got: {msg}");
        assert!(msg.contains("ambiguous"), "got: {msg}");
    }

    #[test]
    fn file_edit_schema_has_required_params() {
        let tool = test_tool(std::env::temp_dir());
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["path"].is_object());
        assert!(schema["properties"]["old_string"].is_object());
        assert!(schema["properties"]["new_string"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("path")));
        assert!(required.contains(&json!("old_string")));
        assert!(required.contains(&json!("new_string")));
    }

    #[tokio::test]
    async fn file_edit_replaces_single_match() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_edit_single");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("test.txt"), "hello world")
            .await
            .unwrap();

        let tool = test_tool(dir.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "old_string": "hello",
                "new_string": "goodbye"
            }))
            .await
            .unwrap();

        assert!(result.success, "edit should succeed: {:?}", result.error);
        assert!(result.output.contains("replaced 1 occurrence"));

        let content = tokio::fs::read_to_string(dir.join("test.txt"))
            .await
            .unwrap();
        assert_eq!(content, "goodbye world");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_edit_reports_removed_symbol_with_remaining_reference() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_edit_symbol_check");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(
            dir.join("lib.rs"),
            "fn helper() {}\n\nfn main() {\n    helper();\n}\n",
        )
        .await
        .unwrap();

        let code_intel = test_code_intel(dir.clone(), true);
        assert_eq!(code_intel.find_definition("helper").unwrap().len(), 1);

        let tool = test_tool_with_code_intel(dir.clone(), code_intel);
        let result = tool
            .execute(json!({
                "path": "lib.rs",
                "old_string": "fn helper() {}\n\n",
                "new_string": ""
            }))
            .await
            .unwrap();

        assert!(result.success, "error: {:?}", result.error);
        assert!(
            result.output.contains("## Post-edit symbol check"),
            "symbol check note missing from output: {}",
            result.output
        );
        assert!(result.output.contains("`helper`"), "got: {}", result.output);
        assert!(
            result.output.contains("remaining reference"),
            "got: {}",
            result.output
        );
        let diagnostics = result.diagnostics.expect("semantic diagnostics");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].severity, DiagnosticSeverity::Error);
        assert_eq!(
            diagnostics[0].code.as_deref(),
            Some("semantic.unresolved_symbol")
        );
        assert!(diagnostics[0].message.contains("`helper`"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_edit_not_found() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_edit_notfound");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("test.txt"), "hello world")
            .await
            .unwrap();

        let tool = test_tool(dir.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "old_string": "nonexistent",
                "new_string": "replacement"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("not found"));

        let content = tokio::fs::read_to_string(dir.join("test.txt"))
            .await
            .unwrap();
        assert_eq!(content, "hello world");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_edit_multiple_matches() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_edit_multi");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("test.txt"), "aaa bbb aaa")
            .await
            .unwrap();

        let tool = test_tool(dir.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "old_string": "aaa",
                "new_string": "ccc"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("matches 2 times")
        );

        let content = tokio::fs::read_to_string(dir.join("test.txt"))
            .await
            .unwrap();
        assert_eq!(content, "aaa bbb aaa");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_edit_delete_via_empty_new_string() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_edit_delete");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("test.txt"), "keep remove keep")
            .await
            .unwrap();

        let tool = test_tool(dir.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "old_string": " remove",
                "new_string": ""
            }))
            .await
            .unwrap();

        assert!(
            result.success,
            "delete edit should succeed: {:?}",
            result.error
        );

        let content = tokio::fs::read_to_string(dir.join("test.txt"))
            .await
            .unwrap();
        assert_eq!(content, "keep keep");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_edit_missing_path_param() {
        let tool = test_tool(std::env::temp_dir());
        let result = tool
            .execute(json!({"old_string": "a", "new_string": "b"}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn file_edit_missing_old_string_param() {
        let tool = test_tool(std::env::temp_dir());
        let result = tool
            .execute(json!({"path": "f.txt", "new_string": "b"}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn file_edit_missing_new_string_param() {
        let tool = test_tool(std::env::temp_dir());
        let result = tool
            .execute(json!({"path": "f.txt", "old_string": "a"}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn file_edit_rejects_empty_old_string() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_edit_empty_old_string");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("test.txt"), "hello")
            .await
            .unwrap();

        let tool = test_tool(dir.clone());
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "old_string": "",
                "new_string": "x"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("must not be empty")
        );

        let content = tokio::fs::read_to_string(dir.join("test.txt"))
            .await
            .unwrap();
        assert_eq!(content, "hello");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_edit_blocks_path_traversal() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_edit_traversal");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = wrapped_tool(dir.clone());
        let result = tool
            .execute(json!({
                "path": "../../etc/passwd",
                "old_string": "root",
                "new_string": "hacked"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(
            result.error.as_ref().unwrap().contains("Path blocked"),
            "expected 'Path blocked' error, got: {:?}",
            result.error
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_edit_blocks_absolute_path() {
        let tool = wrapped_tool(std::env::temp_dir());
        let result = tool
            .execute(json!({
                "path": absolute_path_outside_workspace(),
                "old_string": "root",
                "new_string": "hacked"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(
            result.error.as_ref().unwrap().contains("Path blocked"),
            "expected 'Path blocked' error, got: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn file_edit_normalizes_workspace_prefixed_relative_path() {
        let root = std::env::temp_dir().join("zeroclaw_test_file_edit_workspace_prefixed");
        let workspace = root.join("workspace");
        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(workspace.join("nested"))
            .await
            .unwrap();
        tokio::fs::write(workspace.join("nested/target.txt"), "hello world")
            .await
            .unwrap();

        let tool = test_tool(workspace.clone());
        let workspace_prefixed =
            crate::util_helpers::workspace_prefixed_relative_path_for_test(&workspace)
                .join("nested/target.txt");
        let result = tool
            .execute(json!({
                "path": workspace_prefixed.to_string_lossy(),
                "old_string": "world",
                "new_string": "zeroclaw"
            }))
            .await
            .unwrap();

        assert!(result.success);
        let content = tokio::fs::read_to_string(workspace.join("nested/target.txt"))
            .await
            .unwrap();
        assert_eq!(content, "hello zeroclaw");
        assert!(!workspace.join(workspace_prefixed).exists());

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_edit_blocks_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join("zeroclaw_test_file_edit_symlink_escape");
        let workspace = root.join("workspace");
        let outside = root.join("outside");

        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        tokio::fs::create_dir_all(&outside).await.unwrap();

        symlink(&outside, workspace.join("escape_dir")).unwrap();

        let tool = test_tool(workspace.clone());
        let result = tool
            .execute(json!({
                "path": "escape_dir/target.txt",
                "old_string": "a",
                "new_string": "b"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("escapes workspace")
        );

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_edit_blocks_symlink_target_file() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join("zeroclaw_test_file_edit_symlink_target");
        let workspace = root.join("workspace");
        let outside = root.join("outside");

        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        tokio::fs::create_dir_all(&outside).await.unwrap();

        tokio::fs::write(outside.join("target.txt"), "original")
            .await
            .unwrap();
        symlink(outside.join("target.txt"), workspace.join("linked.txt")).unwrap();

        let tool = test_tool(workspace.clone());
        let result = tool
            .execute(json!({
                "path": "linked.txt",
                "old_string": "original",
                "new_string": "hacked"
            }))
            .await
            .unwrap();

        assert!(!result.success, "editing through symlink must be blocked");
        assert!(
            result.error.as_deref().unwrap_or("").contains("symlink"),
            "error should mention symlink"
        );

        let content = tokio::fs::read_to_string(outside.join("target.txt"))
            .await
            .unwrap();
        assert_eq!(content, "original", "original file must not be modified");

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn file_edit_blocks_readonly_mode() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_edit_readonly");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("test.txt"), "hello")
            .await
            .unwrap();

        let tool = test_tool_with(dir.clone(), AutonomyLevel::ReadOnly, 20);
        let result = tool
            .execute(json!({
                "path": "test.txt",
                "old_string": "hello",
                "new_string": "world"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("read-only"));

        let content = tokio::fs::read_to_string(dir.join("test.txt"))
            .await
            .unwrap();
        assert_eq!(content, "hello");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_edit_nonexistent_file() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_edit_nofile");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = test_tool(dir.clone());
        let result = tool
            .execute(json!({
                "path": "missing.txt",
                "old_string": "a",
                "new_string": "b"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("Failed to read file")
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_edit_absolute_path_in_workspace() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_edit_abs_path");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        // Canonicalize so the workspace dir matches resolved paths on macOS (/private/var/…)
        let dir = tokio::fs::canonicalize(&dir).await.unwrap();

        tokio::fs::write(dir.join("target.txt"), "old content")
            .await
            .unwrap();

        let tool = test_tool(dir.clone());

        let abs_path = dir.join("target.txt");
        let result = tool
            .execute(json!({
                "path": abs_path.to_string_lossy().to_string(),
                "old_string": "old content",
                "new_string": "new content"
            }))
            .await
            .unwrap();

        assert!(
            result.success,
            "editing via absolute workspace path should succeed, error: {:?}",
            result.error
        );

        let content = tokio::fs::read_to_string(dir.join("target.txt"))
            .await
            .unwrap();
        assert_eq!(content, "new content");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_edit_blocks_null_byte_in_path() {
        let dir = std::env::temp_dir().join("zeroclaw_test_file_edit_null_byte");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = wrapped_tool(dir.clone());
        let result = tool
            .execute(json!({
                "path": "test\0evil.txt",
                "old_string": "old",
                "new_string": "new"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result.error.as_ref().unwrap().contains("Path blocked"),
            "expected 'Path blocked' error, got: {:?}",
            result.error
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_edit_blocks_path_outside_workspace() {
        let root = std::env::temp_dir().join("zeroclaw_test_file_edit_outside_workspace");
        let workspace = root.join("workspace");
        let outside = root.join("outside.txt");
        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        tokio::fs::write(&outside, "original").await.unwrap();

        let tool = test_tool(workspace.clone());
        let result = tool
            .execute(json!({
                "path": outside.to_string_lossy(),
                "old_string": "original",
                "new_string": "hacked"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        let content = tokio::fs::read_to_string(&outside).await.unwrap();
        assert_eq!(
            content, "original",
            "file outside workspace must not be modified"
        );

        let _ = tokio::fs::remove_dir_all(&root).await;
    }
}

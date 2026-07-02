use std::collections::HashSet;
use std::path::PathBuf;

use zeroclaw_tool_call_parser::ParsedToolCall;
use zeroclaw_tools::code_intel::{CodeIntel, SymbolDef, render_symbols};

const SECTION_HEADER: &str = "## Verified Symbol Context\n\n";
const SECTION_INTRO: &str =
    "# auto-resolved from pending file edit/write targets; ground truth, not a guess\n\n";
const MAX_CANDIDATES: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditTarget {
    path: PathBuf,
    context: String,
}

impl EditTarget {
    fn new(path: PathBuf, context: String) -> Self {
        Self { path, context }
    }
}

pub(crate) fn edit_targets_from_tool_calls(tool_calls: &[ParsedToolCall]) -> Vec<EditTarget> {
    let mut seen = HashSet::new();
    tool_calls
        .iter()
        .filter_map(edit_target_from_tool_call)
        .filter(|target| seen.insert(target.path.clone()))
        .take(MAX_CANDIDATES)
        .collect()
}

pub(crate) fn edit_targets_key(edit_targets: &[EditTarget]) -> String {
    edit_targets
        .iter()
        .map(|target| format!("{}:{}", target.path.display(), stable_hash(&target.context)))
        .collect::<Vec<_>>()
        .join("|")
}

pub(crate) fn build_symbol_context_section(
    code_intel: &CodeIntel,
    edit_targets: &[EditTarget],
    compact_context: bool,
    max_chars: usize,
) -> Option<String> {
    if compact_context || max_chars == 0 || edit_targets.is_empty() {
        return None;
    }

    let body_budget = max_chars.saturating_sub(SECTION_HEADER.len() + SECTION_INTRO.len());
    if body_budget == 0 {
        return None;
    }

    let mut defs = Vec::new();
    let mut seen = HashSet::new();

    for target in edit_targets.iter().take(MAX_CANDIDATES) {
        if let Ok(symbols) = code_intel.document_symbol(&target.path) {
            for symbol in symbols.into_iter().take(MAX_CANDIDATES) {
                push_unique_def(&mut defs, &mut seen, symbol.def);
            }
        }

        for name in symbol_name_candidates(&target.context)
            .into_iter()
            .take(MAX_CANDIDATES)
        {
            if let Ok(symbols) = code_intel.find_definition(&name) {
                for symbol in symbols.into_iter().take(MAX_CANDIDATES) {
                    push_unique_def(&mut defs, &mut seen, symbol);
                }
            }
        }
    }

    if defs.is_empty() {
        return None;
    }

    let rendered = render_symbols(&defs, body_budget);
    if rendered.is_empty() {
        return None;
    }

    let mut section =
        String::with_capacity(SECTION_HEADER.len() + SECTION_INTRO.len() + rendered.len() + 1);
    section.push_str(SECTION_HEADER);
    section.push_str(SECTION_INTRO);
    section.push_str(&rendered);
    section.push('\n');
    Some(section)
}

fn push_unique_def(
    defs: &mut Vec<SymbolDef>,
    seen: &mut HashSet<(String, PathBuf, u32, u32)>,
    def: SymbolDef,
) {
    let key = (
        def.name.clone(),
        def.span.path.clone(),
        def.span.start_line,
        def.span.start_col,
    );
    if seen.insert(key) {
        defs.push(def);
    }
}

fn edit_target_from_tool_call(call: &ParsedToolCall) -> Option<EditTarget> {
    match call.name.as_str() {
        "file_edit" => {
            let path = path_arg(&call.arguments)?;
            let context = ["old_string", "new_string"]
                .into_iter()
                .filter_map(|key| call.arguments.get(key).and_then(|value| value.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            Some(EditTarget::new(path, context))
        }
        "file_write" => {
            let path = path_arg(&call.arguments)?;
            let context = call
                .arguments
                .get("content")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            Some(EditTarget::new(path, context))
        }
        _ => None,
    }
}

fn path_arg(arguments: &serde_json::Value) -> Option<PathBuf> {
    arguments
        .get("path")
        .and_then(|value| value.as_str())
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
}

fn symbol_name_candidates(request: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    request
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':'))
        .filter(|token| is_symbol_like(token))
        .map(ToOwned::to_owned)
        .filter(|name| seen.insert(name.clone()))
        .collect()
}

fn is_symbol_like(token: &str) -> bool {
    if token.len() < 3 {
        return false;
    }
    token.contains("::")
        || token
            .chars()
            .next()
            .map(|c| c.is_ascii_uppercase())
            .unwrap_or(false)
}

fn stable_hash(value: &str) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::tempdir;
    use zeroclaw_config::coding::CodeIntelConfig;

    use super::*;

    #[test]
    fn extracts_explicit_edit_targets_from_tool_calls() {
        let calls = vec![
            ParsedToolCall {
                name: "file_edit".to_string(),
                arguments: serde_json::json!({
                    "path": "src/lib.rs",
                    "old_string": "Widget::new()",
                    "new_string": "Widget::from_config()"
                }),
                tool_call_id: None,
            },
            ParsedToolCall {
                name: "shell".to_string(),
                arguments: serde_json::json!({"command": "cargo test"}),
                tool_call_id: None,
            },
        ];

        assert_eq!(
            edit_targets_from_tool_calls(&calls),
            vec![EditTarget::new(
                PathBuf::from("src/lib.rs"),
                "Widget::new()\nWidget::from_config()".to_string()
            )]
        );
    }

    #[test]
    fn renders_context_for_explicit_edit_target() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub struct Widget {}\n").unwrap();
        let code_intel =
            CodeIntel::new(dir.path().to_path_buf(), Arc::new(CodeIntelConfig::default));

        let section = build_symbol_context_section(
            &code_intel,
            &[EditTarget::new(
                PathBuf::from("lib.rs"),
                "Widget".to_string(),
            )],
            false,
            1_000,
        )
        .unwrap();

        assert!(section.contains("## Verified Symbol Context"));
        assert!(section.contains("Widget"));
        assert!(section.contains("pub struct Widget"));
    }

    #[test]
    fn target_key_includes_path_and_context() {
        let key = edit_targets_key(&[EditTarget::new(
            PathBuf::from("src/lib.rs"),
            "Widget".to_string(),
        )]);

        assert!(key.starts_with("src/lib.rs:"));
    }

    #[test]
    fn compact_context_suppresses_section() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub struct Widget {}\n").unwrap();
        let code_intel =
            CodeIntel::new(dir.path().to_path_buf(), Arc::new(CodeIntelConfig::default));

        assert!(
            build_symbol_context_section(
                &code_intel,
                &[EditTarget::new(
                    PathBuf::from("lib.rs"),
                    "Widget".to_string()
                )],
                true,
                1_000
            )
            .is_none()
        );
    }

    #[test]
    fn empty_targets_fail_open() {
        let dir = tempdir().unwrap();
        let code_intel =
            CodeIntel::new(dir.path().to_path_buf(), Arc::new(CodeIntelConfig::default));

        assert!(build_symbol_context_section(&code_intel, &[], false, 1_000).is_none());
    }
}

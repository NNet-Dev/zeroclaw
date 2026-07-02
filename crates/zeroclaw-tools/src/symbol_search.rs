use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use zeroclaw_api::tool::{Tool, ToolResult, ToolSideEffect};

use crate::code_intel::{CodeIntel, SymbolDef, SymbolInfo, SymbolRef, render_symbols};

pub struct SymbolSearchTool {
    code_intel: Arc<CodeIntel>,
}

impl SymbolSearchTool {
    pub const NAME: &'static str = "symbol_search";

    pub fn new(code_intel: Arc<CodeIntel>) -> Self {
        Self { code_intel }
    }
}

#[async_trait]
impl Tool for SymbolSearchTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Find source symbols with the code-intelligence index. Modes: find_definition, find_references, document_symbol."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["find_definition", "find_references", "document_symbol"],
                    "description": "Symbol lookup mode",
                    "default": "find_definition"
                },
                "name": {
                    "type": "string",
                    "description": "Symbol name for find_definition or find_references"
                },
                "path": {
                    "type": "string",
                    "description": "Source file path for document_symbol"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum results to render",
                    "default": 50
                }
            }
        })
    }

    fn side_effect(&self) -> ToolSideEffect {
        ToolSideEffect::ReadOnly
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let mode = args
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("find_definition");
        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(50)
            .min(200);

        let output = match mode {
            "find_definition" => {
                let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
                    return Ok(ToolResult::err("name is required for find_definition"));
                };
                match self.code_intel.find_definition(name) {
                    Ok(defs) => render_definition_results(name, &defs, max_results),
                    Err(_) => format!("No definition found for `{name}`."),
                }
            }
            "find_references" => {
                let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
                    return Ok(ToolResult::err("name is required for find_references"));
                };
                match self.code_intel.find_references(name) {
                    Ok(references) => render_reference_results(name, &references, max_results),
                    Err(_) => format!("No references found for `{name}`."),
                }
            }
            "document_symbol" => {
                let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
                    return Ok(ToolResult::err("path is required for document_symbol"));
                };
                match self.code_intel.document_symbol(&PathBuf::from(path)) {
                    Ok(symbols) => render_document_symbols(path, &symbols, max_results),
                    Err(_) => format!("No document symbols found for `{path}`."),
                }
            }
            other => {
                return Ok(ToolResult::err(format!(
                    "Invalid mode '{other}'. Allowed values: find_definition, find_references, document_symbol."
                )));
            }
        };

        Ok(ToolResult::ok(output))
    }
}

fn render_definition_results(name: &str, defs: &[SymbolDef], max_results: usize) -> String {
    if defs.is_empty() {
        return format!("No definition found for `{name}`.");
    }
    let limited: Vec<_> = defs.iter().take(max_results).cloned().collect();
    render_symbols(&limited, 16_000)
}

fn render_reference_results(name: &str, references: &[SymbolRef], max_results: usize) -> String {
    if references.is_empty() {
        return format!("No references found for `{name}`.");
    }
    references
        .iter()
        .take(max_results)
        .map(|reference| {
            format!(
                "- {}:{}:{}",
                reference.span.path.display(),
                reference.span.start_line,
                reference.span.start_col
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_document_symbols(path: &str, symbols: &[SymbolInfo], max_results: usize) -> String {
    if symbols.is_empty() {
        return format!("No document symbols found for `{path}`.");
    }
    let defs: Vec<_> = symbols
        .iter()
        .take(max_results)
        .map(|symbol| symbol.def.clone())
        .collect();
    render_symbols(&defs, 16_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn find_definition_formats_success() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "pub struct Widget {}\n").unwrap();
        let ci = Arc::new(CodeIntel::new(
            dir.path().to_path_buf(),
            Arc::new(zeroclaw_config::coding::CodeIntelConfig::default),
        ));
        let tool = SymbolSearchTool::new(ci);

        let result = tool
            .execute(json!({"mode": "find_definition", "name": "Widget"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("Widget"));
        assert!(result.output.contains("pub struct Widget"));
    }
}

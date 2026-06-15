use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write;
use std::sync::Arc;
use zeroclaw_api::tool::{Tool, ToolResult};
use zeroclaw_memory::{Memory, docs};

/// Tool-gated retrieval over the ingested document corpus ("past work",
/// reference material).
///
/// Unlike conversational memory — which is auto-injected into context every
/// turn — the document corpus is reached only on demand through this tool and
/// is scoped to a hierarchical taxonomy path, so unrelated documents never
/// pollute the agent's context. Documents are ingested with
/// `zeroclaw docs ingest` and stored under the reserved `docs/` namespace.
pub struct DocsSearchTool {
    memory: Arc<dyn Memory>,
}

impl DocsSearchTool {
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for DocsSearchTool {
    fn name(&self) -> &str {
        "docs_search"
    }

    fn description(&self) -> &str {
        "Search the ingested document corpus (past work, reference material, knowledge base) for relevant content snippets. Scope to a taxonomy path like 'teaching/mathematics' to drill into a category, or omit scope to search all documents. Returns ranked snippets with their source document and category."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords or phrase to search for across documents."
                },
                "scope": {
                    "type": "string",
                    "description": "Optional taxonomy path to restrict the search, e.g. 'teaching/mathematics/year-9'. Omit to search the whole corpus."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max snippets to return (default: 5)."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
        if query.trim().is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("'query' is required and must be non-empty".into()),
            });
        }

        #[allow(clippy::cast_possible_truncation)]
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(5, |v| v as usize);

        // Translate the optional taxonomy scope into a namespace prefix.
        // No scope → search the whole corpus under the docs root.
        let prefix = match args.get("scope").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => docs::namespace_for_path(s.trim()),
            _ => docs::DOCS_NAMESPACE_ROOT.to_string(),
        };

        match self
            .memory
            .recall_namespace_prefix(&prefix, query, limit, None, None, None)
            .await
        {
            Ok(entries) if entries.is_empty() => Ok(ToolResult {
                success: true,
                output: "No matching documents found.".into(),
                error: None,
            }),
            Ok(entries) => {
                let mut output = format!("Found {} document snippet(s):\n", entries.len());
                let root_prefix = format!("{}/", docs::DOCS_NAMESPACE_ROOT);
                for entry in &entries {
                    let score = entry
                        .score
                        .map_or_else(String::new, |s| format!(" [{:.0}%]", s * 100.0));
                    // Strip the reserved root for display so the model sees the
                    // human-meaningful taxonomy path (e.g. `teaching/mathematics`).
                    let category = entry
                        .namespace
                        .strip_prefix(&root_prefix)
                        .unwrap_or(&entry.namespace);
                    let _ = writeln!(
                        output,
                        "- [{category}] {}{score}\n  {}",
                        entry.key, entry.content
                    );
                }
                Ok(ToolResult {
                    success: true,
                    output,
                    error: None,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Document search failed: {e}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use zeroclaw_memory::{MemoryCategory, SqliteMemory};

    async fn seeded() -> (TempDir, Arc<dyn Memory>) {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new("test", tmp.path()).unwrap();
        // Two document chunks in distinct taxonomy subtrees plus one ordinary
        // conversational memory in the default namespace.
        mem.store_with_metadata(
            "algebra.md#0",
            "Solving quadratic equations by factoring.",
            MemoryCategory::Custom("document".into()),
            None,
            Some("docs/teaching/mathematics/year-9"),
            Some(0.5),
        )
        .await
        .unwrap();
        mem.store_with_metadata(
            "scrape.py#0",
            "Prior art script that scrapes patent filings.",
            MemoryCategory::Custom("document".into()),
            None,
            Some("docs/scripts/prior-art"),
            Some(0.5),
        )
        .await
        .unwrap();
        mem.store("note", "User prefers concise answers", MemoryCategory::Core, None)
            .await
            .unwrap();
        (tmp, Arc::new(mem))
    }

    #[tokio::test]
    async fn empty_query_is_rejected() {
        let (_t, mem) = seeded().await;
        let tool = DocsSearchTool::new(mem);
        let res = tool.execute(json!({"query": "  "})).await.unwrap();
        assert!(!res.success);
    }

    #[tokio::test]
    async fn scope_restricts_to_subtree() {
        let (_t, mem) = seeded().await;
        let tool = DocsSearchTool::new(mem);
        let res = tool
            .execute(json!({"query": "quadratic equations factoring", "scope": "teaching/mathematics"}))
            .await
            .unwrap();
        assert!(res.success);
        assert!(res.output.contains("algebra.md#0"), "got: {}", res.output);
        // The prior-art script lives in a different subtree and must not appear.
        assert!(!res.output.contains("scrape.py#0"), "got: {}", res.output);
    }

    #[tokio::test]
    async fn corpus_search_excludes_conversational_memory() {
        let (_t, mem) = seeded().await;
        let tool = DocsSearchTool::new(mem);
        let res = tool
            .execute(json!({"query": "concise answers prefers"}))
            .await
            .unwrap();
        assert!(res.success);
        // The conversational `note` lives in `default`, outside the docs root.
        assert!(!res.output.contains("User prefers concise answers"));
    }

    #[test]
    fn name_and_schema() {
        let tmp = TempDir::new().unwrap();
        let mem: Arc<dyn Memory> = Arc::new(SqliteMemory::new("test", tmp.path()).unwrap());
        let tool = DocsSearchTool::new(mem);
        assert_eq!(tool.name(), "docs_search");
        assert!(tool.parameters_schema()["properties"]["scope"].is_object());
    }
}

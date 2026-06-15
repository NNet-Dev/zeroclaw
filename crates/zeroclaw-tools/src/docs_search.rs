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
///
/// Retrieval is bounded by the agent's subscribed corpora (`allowed_roots`):
/// each entry is a namespace prefix (e.g. `docs/teaching`) the agent may read.
/// This is a hard boundary — a query can never reach a corpus the agent is not
/// subscribed to, mirroring the cross-agent `read_memory_from` allowlist.
pub struct DocsSearchTool {
    memory: Arc<dyn Memory>,
    /// Namespace prefixes this agent may retrieve from. An agent subscribed to
    /// no corpora gets an empty list and retrieves nothing.
    allowed_roots: Vec<String>,
}

impl DocsSearchTool {
    /// Unrestricted constructor: the whole `docs/` corpus is searchable.
    /// Used for the operator/admin surface and tests.
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self {
            memory,
            allowed_roots: vec![docs::DOCS_NAMESPACE_ROOT.to_string()],
        }
    }

    /// Restrict retrieval to the given corpus namespace roots (the hard
    /// boundary). Each root is a `docs/<corpus>` prefix the agent subscribes to.
    pub fn with_corpora(memory: Arc<dyn Memory>, allowed_roots: Vec<String>) -> Self {
        Self {
            memory,
            allowed_roots,
        }
    }

    /// True when `requested` (a resolved namespace prefix) falls inside at least
    /// one subscribed corpus root.
    fn is_within_allowed(&self, requested: &str) -> bool {
        self.allowed_roots.iter().any(|root| {
            requested == root || requested.starts_with(&format!("{root}/"))
        })
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

        // No subscribed corpora → hard boundary denies everything.
        if self.allowed_roots.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "This agent is not subscribed to any document corpora.".into(),
                error: None,
            });
        }

        // Resolve which namespace prefixes to search. A scope must fall inside a
        // subscribed corpus; otherwise it's denied (the hard boundary). With no
        // scope we search the union of all subscribed corpora.
        let search_roots: Vec<String> = match args.get("scope").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => {
                let requested = docs::namespace_for_path(s.trim());
                if !self.is_within_allowed(&requested) {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "scope '{}' is outside this agent's subscribed corpora",
                            s.trim()
                        )),
                    });
                }
                vec![requested]
            }
            _ => self.allowed_roots.clone(),
        };

        // Recall from each root, merge, and keep the top `limit` by score.
        let mut merged: Vec<_> = Vec::new();
        for root in &search_roots {
            match self
                .memory
                .recall_namespace_prefix(root, query, limit, None, None, None)
                .await
            {
                Ok(entries) => merged.extend(entries),
                Err(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Document search failed: {e}")),
                    });
                }
            }
        }
        // Highest score first; unscored entries sort last. Dedupe by id in case
        // overlapping roots returned the same chunk.
        merged.sort_by(|a, b| {
            b.score
                .unwrap_or(f64::MIN)
                .partial_cmp(&a.score.unwrap_or(f64::MIN))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        merged.dedup_by(|a, b| a.id == b.id);
        merged.truncate(limit);

        if merged.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No matching documents found.".into(),
                error: None,
            });
        }

        let mut output = format!("Found {} document snippet(s):\n", merged.len());
        let root_prefix = format!("{}/", docs::DOCS_NAMESPACE_ROOT);
        for entry in &merged {
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

    #[tokio::test]
    async fn subscription_is_a_hard_boundary() {
        let (_t, mem) = seeded().await;
        // Subscribed only to the teaching corpus.
        let tool = DocsSearchTool::with_corpora(mem, vec!["docs/teaching".to_string()]);

        // In-corpus query succeeds.
        let ok = tool
            .execute(json!({"query": "quadratic equations factoring"}))
            .await
            .unwrap();
        assert!(ok.success);
        assert!(ok.output.contains("algebra.md#0"), "got: {}", ok.output);

        // Unscoped search must NOT reach the unsubscribed scripts corpus.
        let scripts = tool
            .execute(json!({"query": "patent filings scrapes claims"}))
            .await
            .unwrap();
        assert!(!scripts.output.contains("scrape.py#0"), "got: {}", scripts.output);

        // Explicitly scoping to an unsubscribed corpus is denied.
        let denied = tool
            .execute(json!({"query": "patent", "scope": "scripts/prior-art"}))
            .await
            .unwrap();
        assert!(!denied.success);
        assert!(denied.error.unwrap().contains("outside"));
    }

    #[tokio::test]
    async fn no_subscription_returns_nothing() {
        let (_t, mem) = seeded().await;
        let tool = DocsSearchTool::with_corpora(mem, vec![]);
        let res = tool.execute(json!({"query": "quadratic"})).await.unwrap();
        assert!(res.success);
        assert!(res.output.contains("not subscribed"));
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

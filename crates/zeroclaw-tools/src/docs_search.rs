use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write;
use std::sync::Arc;
use zeroclaw_api::tool::{Tool, ToolResult};
use zeroclaw_memory::embeddings::EmbeddingProvider;
use zeroclaw_memory::{Memory, QdrantMemory, docs};

/// A pre-built external vector index federated into `docs_search` — the "RAG
/// index" intent. It is queried live (wholesale `recall`) and merged with the
/// internal corpus; nothing is ingested. Subscription is enforced upstream by
/// only constructing indexes for the agent's subscribed bundles.
pub struct FederatedIndex {
    /// Display label for results from this index (e.g. `qdrant:legal`).
    pub label: String,
    /// The backend to query. Any `Memory` works; Qdrant is the first kind.
    pub memory: Arc<dyn Memory>,
}

impl FederatedIndex {
    /// Build a Qdrant-backed federated index. Lazy: the collection is contacted
    /// on first query, so construction never blocks or fails here.
    pub fn qdrant(
        label: impl Into<String>,
        url: &str,
        collection: &str,
        api_key: Option<String>,
        embedder: Arc<dyn EmbeddingProvider>,
    ) -> Self {
        let mem = QdrantMemory::new_lazy("docs-index", url, collection, api_key, embedder);
        Self {
            label: label.into(),
            memory: Arc::new(mem),
        }
    }
}

/// Tool-gated retrieval over the document corpus ("past work", reference
/// material). Covers both knowledge-bundle source intents: ingested **doc
/// folders** (internal `docs/` namespace) and federated **RAG indexes**
/// (external vector indexes queried live).
///
/// Unlike conversational memory — which is auto-injected into context every
/// turn — the corpus is reached only on demand through this tool and is scoped
/// to a hierarchical taxonomy path, so unrelated documents never pollute the
/// agent's context.
///
/// Retrieval is bounded by the agent's subscribed corpora (`allowed_roots`
/// for internal folders, `federated` for external indexes): a query can never
/// reach a corpus the agent is not subscribed to, mirroring the cross-agent
/// `read_memory_from` allowlist.
pub struct DocsSearchTool {
    memory: Arc<dyn Memory>,
    /// Namespace prefixes this agent may retrieve from (ingested folders). An
    /// agent subscribed to no corpora gets an empty list.
    allowed_roots: Vec<String>,
    /// External vector indexes federated at query time (RAG-index sources).
    federated: Vec<FederatedIndex>,
}

impl DocsSearchTool {
    /// Unrestricted constructor: the whole `docs/` corpus is searchable, no
    /// federated indexes. Used for the operator/admin surface and tests.
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self {
            memory,
            allowed_roots: vec![docs::DOCS_NAMESPACE_ROOT.to_string()],
            federated: Vec::new(),
        }
    }

    /// Restrict retrieval to the given corpus namespace roots (the hard
    /// boundary), with no federated indexes.
    pub fn with_corpora(memory: Arc<dyn Memory>, allowed_roots: Vec<String>) -> Self {
        Self {
            memory,
            allowed_roots,
            federated: Vec::new(),
        }
    }

    /// Restrict to the given corpus roots AND federate the given external
    /// indexes. Both halves are bounded by the agent's subscription.
    pub fn with_corpora_and_indexes(
        memory: Arc<dyn Memory>,
        allowed_roots: Vec<String>,
        federated: Vec<FederatedIndex>,
    ) -> Self {
        Self {
            memory,
            allowed_roots,
            federated,
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

        // No subscribed corpora (neither folders nor indexes) → deny everything.
        if self.allowed_roots.is_empty() && self.federated.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "This agent is not subscribed to any document corpora.".into(),
                error: None,
            });
        }

        // Resolve which internal namespace prefixes to search. A scope must fall
        // inside a subscribed corpus; otherwise it's denied (the hard boundary).
        // With no scope we search the union of all subscribed corpora. A scope is
        // taxonomy-specific to ingested folders, so federated indexes are only
        // queried when no scope is given.
        let scoped = args
            .get("scope")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty());
        let search_roots: Vec<String> = if scoped {
            let raw = args.get("scope").and_then(|v| v.as_str()).unwrap_or("").trim();
            let requested = docs::namespace_for_path(raw);
            if !self.is_within_allowed(&requested) {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "scope '{raw}' is outside this agent's subscribed corpora"
                    )),
                });
            }
            vec![requested]
        } else {
            self.allowed_roots.clone()
        };

        // Collect (display category, entry). Internal folders strip the `docs/`
        // root to show the taxonomy path; federated results carry the index label.
        let root_prefix = format!("{}/", docs::DOCS_NAMESPACE_ROOT);
        let mut merged: Vec<(String, zeroclaw_memory::MemoryEntry)> = Vec::new();

        for root in &search_roots {
            match self
                .memory
                .recall_namespace_prefix(root, query, limit, None, None, None)
                .await
            {
                Ok(entries) => {
                    for e in entries {
                        let category = e
                            .namespace
                            .strip_prefix(&root_prefix)
                            .unwrap_or(&e.namespace)
                            .to_string();
                        merged.push((category, e));
                    }
                }
                Err(err) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Document search failed: {err}")),
                    });
                }
            }
        }

        // Federate external RAG indexes (skipped when an internal scope is set).
        if !scoped {
            for index in &self.federated {
                match index.memory.recall(query, limit, None, None, None).await {
                    Ok(entries) => {
                        for e in entries {
                            merged.push((format!("index:{}", index.label), e));
                        }
                    }
                    // A single unreachable index shouldn't fail the whole
                    // search; skip it and return what the other sources have.
                    Err(_) => {}
                }
            }
        }

        // Highest score first; unscored entries sort last. Dedupe by id in case
        // overlapping sources returned the same chunk.
        merged.sort_by(|a, b| {
            b.1.score
                .unwrap_or(f64::MIN)
                .partial_cmp(&a.1.score.unwrap_or(f64::MIN))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        merged.dedup_by(|a, b| a.1.id == b.1.id);
        merged.truncate(limit);

        if merged.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No matching documents found.".into(),
                error: None,
            });
        }

        let mut output = format!("Found {} document snippet(s):\n", merged.len());
        for (category, entry) in &merged {
            let score = entry
                .score
                .map_or_else(String::new, |s| format!(" [{:.0}%]", s * 100.0));
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

    #[tokio::test]
    async fn federated_index_results_are_merged_and_labelled() {
        let (_t, internal) = seeded().await;
        // Stand-in external "RAG index": a separate store with one doc. Any
        // `Memory` works as a federated index — Qdrant is just the first kind.
        let ext_tmp = TempDir::new().unwrap();
        let ext = SqliteMemory::new("ext", ext_tmp.path()).unwrap();
        ext.store(
            "policy.pdf#0",
            "Refund policy: returns accepted within 30 days.",
            MemoryCategory::Custom("document".into()),
            None,
        )
        .await
        .unwrap();
        let federated = vec![FederatedIndex {
            label: "qdrant:legal".to_string(),
            memory: Arc::new(ext),
        }];
        // Subscribed only to a federated index (no internal folders).
        let tool = DocsSearchTool::with_corpora_and_indexes(internal, vec![], federated);

        let res = tool
            .execute(json!({"query": "refund policy returns days"}))
            .await
            .unwrap();
        assert!(res.success, "{res:?}");
        assert!(res.output.contains("policy.pdf#0"), "got: {}", res.output);
        // Federated results are labelled with the index, not a docs/ path.
        assert!(res.output.contains("[index:qdrant:legal]"), "got: {}", res.output);
    }

    #[tokio::test]
    async fn explicit_scope_skips_federated_indexes() {
        let (_t, internal) = seeded().await;
        let ext_tmp = TempDir::new().unwrap();
        let ext = SqliteMemory::new("ext", ext_tmp.path()).unwrap();
        ext.store("ext.md#0", "external quadratic note", MemoryCategory::Core, None)
            .await
            .unwrap();
        let federated = vec![FederatedIndex {
            label: "qdrant:legal".to_string(),
            memory: Arc::new(ext),
        }];
        let tool = DocsSearchTool::with_corpora_and_indexes(
            internal,
            vec!["docs/teaching".to_string()],
            federated,
        );
        // Scoped to an internal folder → federated index must not contribute.
        let res = tool
            .execute(json!({"query": "quadratic", "scope": "teaching/mathematics"}))
            .await
            .unwrap();
        assert!(res.success);
        assert!(!res.output.contains("index:qdrant:legal"), "got: {}", res.output);
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

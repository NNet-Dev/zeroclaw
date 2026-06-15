//! Document-corpus namespace conventions for the tool-gated docs RAG.
//!
//! Documents ingested via `zeroclaw docs ingest` live under a reserved
//! namespace root (`docs`) so they stay isolated from conversational memory:
//! they are excluded from the always-on per-turn context injection and are
//! reached only through the pull-based `docs_search` tool. Hierarchy is
//! encoded directly in the `namespace` path (e.g. `docs/teaching/mathematics/
//! year-9`), which lets `recall_namespace_prefix` drill into a subtree without
//! any schema migration.

/// Reserved namespace root for ingested documents.
pub const DOCS_NAMESPACE_ROOT: &str = "docs";

/// True when `namespace` belongs to the document corpus — the root itself or
/// any descendant path `docs/...`. Context assembly uses this to keep document
/// chunks out of the always-on conversational memory injection.
pub fn is_docs_namespace(namespace: &str) -> bool {
    namespace == DOCS_NAMESPACE_ROOT
        || namespace.starts_with(&format!("{DOCS_NAMESPACE_ROOT}/"))
}

/// Build a hierarchical namespace path from a relative taxonomy path (derived
/// from the ingest folder structure). Each segment is slugified and empty
/// segments are dropped; the bare root is returned when `rel` is empty.
///
/// e.g. `teaching/Mathematics/Year 9` → `docs/teaching/mathematics/year-9`
pub fn namespace_for_path(rel: &str) -> String {
    let mut out = String::from(DOCS_NAMESPACE_ROOT);
    for seg in rel.split(['/', '\\']) {
        let slug = slugify_segment(seg);
        if !slug.is_empty() {
            out.push('/');
            out.push_str(&slug);
        }
    }
    out
}

/// Slugify a single taxonomy segment: lowercase, whitespace/underscore → `-`,
/// and keep only alphanumerics, `-`, and `.`.
fn slugify_segment(seg: &str) -> String {
    seg.trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_whitespace() || c == '_' { '-' } else { c })
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '.')
        .collect()
}

/// A reference to a pre-built external vector index — the "RAG index" intent
/// for a knowledge-bundle source (queried live, never ingested).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexRef {
    /// A Qdrant collection. `url` is the server base (e.g. `http://host:6333`).
    Qdrant {
        url: String,
        collection: String,
        api_key: Option<String>,
    },
}

impl IndexRef {
    /// Short human label for display (e.g. `qdrant:my_collection`).
    pub fn label(&self) -> String {
        match self {
            IndexRef::Qdrant { collection, .. } => format!("qdrant:{collection}"),
        }
    }
}

/// Classification of a knowledge-bundle `source` entry. The config's own
/// dashboard help documents two intents — "RAG indexes, doc folders" — so a
/// source is either a local folder/file to ingest, or a reference to an
/// existing vector index to federate at query time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocSource {
    /// A local directory or file to ingest into the internal store. Carries the
    /// original (un-expanded) string; `~`/`file://` are resolved by the caller.
    Folder(String),
    /// An external vector index queried live (never ingested).
    Index(IndexRef),
}

/// Classify a bundle `source` string by scheme.
///
/// - `qdrant://[apikey@]host[:port]/collection` → `Index` (http)
/// - `qdrant+https://[apikey@]host[:port]/collection` → `Index` (https)
/// - `file://path` or any other bare path → `Folder`
pub fn classify_source(source: &str) -> DocSource {
    let s = source.trim();
    if let Some(rest) = s.strip_prefix("qdrant+https://") {
        if let Some(idx) = parse_qdrant_authority(rest, true) {
            return DocSource::Index(idx);
        }
    } else if let Some(rest) = s.strip_prefix("qdrant://") {
        if let Some(idx) = parse_qdrant_authority(rest, false) {
            return DocSource::Index(idx);
        }
    }
    DocSource::Folder(s.to_string())
}

/// Parse `[apikey@]host[:port]/collection` into a Qdrant index reference.
/// Returns `None` when the collection segment is missing.
fn parse_qdrant_authority(rest: &str, https: bool) -> Option<IndexRef> {
    let (api_key, hostpath) = match rest.split_once('@') {
        Some((key, tail)) if !key.is_empty() => (Some(key.to_string()), tail),
        _ => (None, rest),
    };
    let (authority, collection) = hostpath.split_once('/')?;
    if authority.is_empty() || collection.is_empty() {
        return None;
    }
    let scheme = if https { "https" } else { "http" };
    Some(IndexRef::Qdrant {
        url: format!("{scheme}://{authority}"),
        collection: collection.trim_matches('/').to_string(),
        api_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_and_descendants_are_docs_namespaces() {
        assert!(is_docs_namespace("docs"));
        assert!(is_docs_namespace("docs/teaching"));
        assert!(is_docs_namespace("docs/teaching/mathematics/year-9"));
    }

    #[test]
    fn unrelated_namespaces_are_not_docs() {
        assert!(!is_docs_namespace("default"));
        assert!(!is_docs_namespace("docsy")); // prefix-but-not-segment guard
        assert!(!is_docs_namespace("documents"));
    }

    #[test]
    fn namespace_for_path_slugifies_and_nests() {
        assert_eq!(
            namespace_for_path("teaching/Mathematics/Year 9"),
            "docs/teaching/mathematics/year-9"
        );
    }

    #[test]
    fn namespace_for_path_handles_empty_and_messy_segments() {
        assert_eq!(namespace_for_path(""), "docs");
        assert_eq!(namespace_for_path("/"), "docs");
        assert_eq!(
            namespace_for_path("Prior_Art//Scripts/"),
            "docs/prior-art/scripts"
        );
    }

    #[test]
    fn bare_paths_and_file_urls_are_folders() {
        assert_eq!(
            classify_source("~/work/prior-art"),
            DocSource::Folder("~/work/prior-art".into())
        );
        assert_eq!(
            classify_source("file:///shared/docs"),
            DocSource::Folder("file:///shared/docs".into())
        );
    }

    #[test]
    fn qdrant_uris_are_indexes() {
        assert_eq!(
            classify_source("qdrant://localhost:6333/my_corpus"),
            DocSource::Index(IndexRef::Qdrant {
                url: "http://localhost:6333".into(),
                collection: "my_corpus".into(),
                api_key: None,
            })
        );
        assert_eq!(
            classify_source("qdrant+https://secret@cloud.qdrant.io/legal"),
            DocSource::Index(IndexRef::Qdrant {
                url: "https://cloud.qdrant.io".into(),
                collection: "legal".into(),
                api_key: Some("secret".into()),
            })
        );
    }

    #[test]
    fn qdrant_uri_without_collection_falls_back_to_folder() {
        // No `/collection` segment → not a valid index, treat as a path.
        assert!(matches!(
            classify_source("qdrant://localhost:6333"),
            DocSource::Folder(_)
        ));
    }

    #[test]
    fn index_label_is_readable() {
        let idx = IndexRef::Qdrant {
            url: "http://h:6333".into(),
            collection: "legal".into(),
            api_key: None,
        };
        assert_eq!(idx.label(), "qdrant:legal");
    }
}


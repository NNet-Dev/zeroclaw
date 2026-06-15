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
}

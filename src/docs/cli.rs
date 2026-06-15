//! Handlers for `zeroclaw docs <subcommand>`.

use crate::config::Config;
use crate::memory::traits::{Memory, MemoryCategory};
use anyhow::{Context, Result, bail};
use console::style;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use zeroclaw_memory::{chunker, docs};

/// Approximate tokens per chunk; mirrors the hardware-RAG chunker setting.
const CHUNK_MAX_TOKENS: usize = 512;
/// Baseline importance stamped on ingested document chunks.
const DOC_IMPORTANCE: f64 = 0.5;
/// File extensions we can currently extract text from.
const SUPPORTED_EXTS: &[&str] = &["md", "txt", "markdown", "text"];

/// Dispatch `zeroclaw docs <subcommand>`.
pub async fn handle_command(command: crate::DocsCommands, config: &Config) -> Result<()> {
    match command {
        crate::DocsCommands::Ingest {
            path,
            collection,
            recursive,
            force,
        } => handle_ingest(config, &path, collection, recursive, force).await,
        crate::DocsCommands::Search {
            query,
            scope,
            limit,
        } => handle_search(config, &query, scope.as_deref(), limit).await,
        crate::DocsCommands::List => handle_list(config).await,
    }
}

/// Ingest a file or directory tree into the `docs/` namespace.
async fn handle_ingest(
    config: &Config,
    path: &str,
    collection: Option<String>,
    recursive: bool,
    force: bool,
) -> Result<()> {
    let root = PathBuf::from(path);
    if !root.exists() {
        bail!("path does not exist: {}", root.display());
    }

    // The base directory is what taxonomy paths are computed relative to. For a
    // single-file ingest the base is the file's parent so the file itself maps
    // to the collection root.
    let (base_dir, is_single_file) = if root.is_dir() {
        (root.clone(), false)
    } else {
        (
            root.parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf),
            true,
        )
    };

    // Default the collection name to the ingested directory's name so files
    // land under e.g. `docs/teaching/...` when ingesting a `teaching/` folder.
    let collection = collection.unwrap_or_else(|| {
        if is_single_file {
            String::new()
        } else {
            base_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        }
    });

    let mut files = Vec::new();
    if is_single_file {
        files.push(root.clone());
    } else {
        collect_supported(&root, recursive, &mut files);
    }
    files.sort();

    if files.is_empty() {
        println!(
            "No supported documents found under {} (looking for: {}).",
            root.display(),
            SUPPORTED_EXTS.join(", ")
        );
        return Ok(());
    }

    let memory = crate::memory::cli::create_memory_with_embedder(config)?;

    println!(
        "Ingesting {} document(s) from {} into collection '{}'…",
        files.len(),
        root.display(),
        if collection.is_empty() {
            docs::DOCS_NAMESPACE_ROOT
        } else {
            &collection
        }
    );

    let mut ingested = 0usize;
    let mut skipped = 0usize;
    let mut chunks_total = 0usize;
    let mut failed = 0usize;

    for file in &files {
        let rel = file.strip_prefix(&base_dir).unwrap_or(file);
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        // Taxonomy path = collection + the file's parent directories. The
        // filename is part of the key, not the namespace.
        let parent_rel = rel
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let taxonomy = join_taxonomy(&collection, &parent_rel);
        let namespace = docs::namespace_for_path(&taxonomy);

        let first_key = format!("{rel_str}#0");
        if !force
            && memory
                .get(&first_key)
                .await
                .ok()
                .flatten()
                .is_some()
        {
            skipped += 1;
            continue;
        }

        let text = match std::fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("  {} {}: {e}", style("skip").yellow(), rel_str);
                failed += 1;
                continue;
            }
        };

        let chunks = chunker::chunk_markdown(&text, CHUNK_MAX_TOKENS);
        if chunks.is_empty() {
            continue;
        }

        let mut stored = 0usize;
        for chunk in &chunks {
            let key = format!("{rel_str}#{}", chunk.index);
            if let Err(e) = memory
                .store_with_metadata(
                    &key,
                    &chunk.content,
                    MemoryCategory::Custom("document".into()),
                    None,
                    Some(&namespace),
                    Some(DOC_IMPORTANCE),
                )
                .await
            {
                eprintln!("  {} {key}: {e}", style("error").red());
                failed += 1;
                continue;
            }
            stored += 1;
        }
        chunks_total += stored;
        ingested += 1;
        println!(
            "  {} {rel_str} → {} ({stored} chunk(s))",
            style("ok").green(),
            namespace
        );
    }

    println!(
        "\nDone: {} ingested, {} skipped (already present), {} chunk(s) stored{}.",
        ingested,
        skipped,
        chunks_total,
        if failed > 0 {
            format!(", {failed} error(s)")
        } else {
            String::new()
        }
    );
    Ok(())
}

/// Search the corpus, optionally scoped to a taxonomy subtree.
async fn handle_search(
    config: &Config,
    query: &str,
    scope: Option<&str>,
    limit: usize,
) -> Result<()> {
    let memory = crate::memory::cli::create_memory_with_embedder(config)?;
    let prefix = match scope {
        Some(s) if !s.trim().is_empty() => docs::namespace_for_path(s.trim()),
        _ => docs::DOCS_NAMESPACE_ROOT.to_string(),
    };

    let entries = memory
        .recall_namespace_prefix(&prefix, query, limit, None, None, None)
        .await
        .context("document search failed")?;

    if entries.is_empty() {
        println!("No matching documents found.");
        return Ok(());
    }

    let root_prefix = format!("{}/", docs::DOCS_NAMESPACE_ROOT);
    println!("Found {} document snippet(s):\n", entries.len());
    for entry in &entries {
        let category = entry
            .namespace
            .strip_prefix(&root_prefix)
            .unwrap_or(&entry.namespace);
        let score = entry
            .score
            .map_or_else(String::new, |s| format!(" [{:.0}%]", s * 100.0));
        println!(
            "{} {}{}",
            style(format!("[{category}]")).cyan(),
            style(&entry.key).bold(),
            score
        );
        println!("  {}\n", entry.content.trim());
    }
    Ok(())
}

/// List taxonomy categories present in the corpus with per-category counts.
async fn handle_list(config: &Config) -> Result<()> {
    let memory = crate::memory::cli::create_memory_with_embedder(config)?;
    let entries = memory.list(None, None).await.context("memory list failed")?;

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for entry in &entries {
        if docs::is_docs_namespace(&entry.namespace) {
            *counts.entry(entry.namespace.clone()).or_default() += 1;
        }
    }

    if counts.is_empty() {
        println!("No documents ingested yet. Run `zeroclaw docs ingest <path>`.");
        return Ok(());
    }

    let root_prefix = format!("{}/", docs::DOCS_NAMESPACE_ROOT);
    let total: usize = counts.values().sum();
    println!("Document corpus — {} chunk(s) across {} categor(ies):\n", total, counts.len());
    for (namespace, count) in &counts {
        let category = namespace.strip_prefix(&root_prefix).unwrap_or(namespace);
        println!("  {:<48} {count} chunk(s)", style(category).cyan());
    }
    Ok(())
}

/// Join a collection name with a relative parent path into a taxonomy path,
/// skipping empty segments (e.g. single-file ingest with no collection).
fn join_taxonomy(collection: &str, parent_rel: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !collection.is_empty() {
        parts.push(collection);
    }
    if !parent_rel.is_empty() {
        parts.push(parent_rel);
    }
    parts.join("/")
}

/// Recursively collect files with a supported extension under `dir`.
fn collect_supported(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if recursive {
                collect_supported(&path, recursive, out);
            }
        } else if path.is_file() && is_supported(&path) {
            out.push(path);
        }
    }
}

fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| SUPPORTED_EXTS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_taxonomy_combines_and_skips_empties() {
        assert_eq!(join_taxonomy("teaching", "mathematics/year-9"), "teaching/mathematics/year-9");
        assert_eq!(join_taxonomy("teaching", ""), "teaching");
        assert_eq!(join_taxonomy("", "scripts"), "scripts");
        assert_eq!(join_taxonomy("", ""), "");
    }

    #[test]
    fn is_supported_matches_known_extensions() {
        assert!(is_supported(Path::new("a/b/notes.md")));
        assert!(is_supported(Path::new("a/b/notes.TXT")));
        assert!(!is_supported(Path::new("a/b/sheet.xlsx")));
        assert!(!is_supported(Path::new("a/b/noext")));
    }
}

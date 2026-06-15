//! Handlers for `zeroclaw docs <subcommand>`.

use super::extract::{self, Extracted};
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

/// Dispatch `zeroclaw docs <subcommand>`.
pub async fn handle_command(command: crate::DocsCommands, config: &Config) -> Result<()> {
    match command {
        crate::DocsCommands::Ingest {
            path,
            collection,
            recursive,
            force,
        } => handle_ingest(config, &path, collection, recursive, force).await,
        crate::DocsCommands::Sync { bundle } => handle_sync(config, bundle).await,
        crate::DocsCommands::Search {
            query,
            scope,
            limit,
        } => handle_search(config, &query, scope.as_deref(), limit).await,
        crate::DocsCommands::List => handle_list(config).await,
    }
}

/// Running totals across one or more ingested trees.
#[derive(Default)]
struct IngestStats {
    ingested: usize,
    skipped: usize,
    chunks_total: usize,
    failed: usize,
    needs_feature: usize,
}

impl IngestStats {
    fn print_summary(&self) {
        println!(
            "\nDone: {} ingested, {} skipped (already present), {} chunk(s) stored{}.",
            self.ingested,
            self.skipped,
            self.chunks_total,
            if self.failed > 0 {
                format!(", {} error(s)", self.failed)
            } else {
                String::new()
            }
        );
        if self.needs_feature > 0 {
            println!(
                "{} {} file(s) (PDF/Office) were skipped — rebuild with \
                 `--features docs-extract` to ingest them.",
                style("note:").yellow(),
                self.needs_feature
            );
        }
    }
}

/// Ingest a file or directory tree into the `docs/` namespace (CLI entrypoint).
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

    // Default the collection name to the ingested directory's name so files land
    // under e.g. `docs/teaching/...` when ingesting a `teaching/` folder.
    let collection = collection.unwrap_or_else(|| {
        if root.is_dir() {
            root.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        }
    });

    let memory = crate::memory::cli::create_memory_with_embedder(config)?;
    let mut stats = IngestStats::default();
    ingest_tree(memory.as_ref(), &root, &collection, recursive, force, &mut stats).await?;
    stats.print_summary();
    Ok(())
}

/// Ingest configured corpora (`knowledge_bundles`) from their `sources`.
/// Each bundle's documents land under `docs/<bundle>/...`. With `only` set,
/// ingest just that one bundle.
async fn handle_sync(config: &Config, only: Option<String>) -> Result<()> {
    let bundles = &config.knowledge_bundles;
    if bundles.is_empty() {
        println!("No [knowledge_bundles.*] configured. Define a corpus, e.g.:\n");
        println!("  [knowledge_bundles.prior_art]");
        println!("  sources = [\"~/work/prior-art\"]");
        return Ok(());
    }
    if let Some(name) = &only
        && !bundles.contains_key(name)
    {
        bail!("no knowledge bundle named '{name}'");
    }

    let memory = crate::memory::cli::create_memory_with_embedder(config)?;
    let mut stats = IngestStats::default();

    for (name, bundle) in bundles {
        if only.as_ref().is_some_and(|o| o != name) {
            continue;
        }
        if bundle.sources.is_empty() {
            println!(
                "{} bundle '{name}' has no sources — skipping.",
                style("note:").yellow()
            );
            continue;
        }
        println!("{} corpus '{name}'", style("syncing").bold());
        for source in &bundle.sources {
            match docs::classify_source(source) {
                // RAG-index sources are federated at query time, never ingested.
                docs::DocSource::Index(idx) => {
                    println!(
                        "  {} {} (federated at query time)",
                        style("index").cyan(),
                        idx.label()
                    );
                }
                docs::DocSource::Folder(path) => {
                    let expanded = expand_source(&path);
                    let root = PathBuf::from(&expanded);
                    if !root.exists() {
                        eprintln!("  {} source not found: {source}", style("skip").yellow());
                        stats.failed += 1;
                        continue;
                    }
                    // The bundle name is the collection root, so all of a
                    // bundle's folder sources merge into one `docs/<bundle>`.
                    ingest_tree(memory.as_ref(), &root, name, true, false, &mut stats).await?;
                }
            }
        }
    }
    stats.print_summary();
    Ok(())
}

/// Expand a leading `~` and strip a `file://` scheme from a source path.
/// Remote schemes (http/s3/…) are not yet supported and pass through unchanged
/// so the caller's existence check reports them as missing.
fn expand_source(source: &str) -> String {
    let s = source.strip_prefix("file://").unwrap_or(source);
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}/{}", home.to_string_lossy(), rest);
        }
    }
    s.to_string()
}

/// Walk a tree (or single file) and ingest every supported document into
/// `docs/<collection>/...`, accumulating into `stats`. Shared by `ingest` and
/// `sync`.
async fn ingest_tree(
    memory: &dyn Memory,
    root: &Path,
    collection: &str,
    recursive: bool,
    force: bool,
    stats: &mut IngestStats,
) -> Result<()> {
    // The base directory is what taxonomy paths are computed relative to. For a
    // single-file ingest the base is the file's parent so the file maps to the
    // collection root.
    let (base_dir, is_single_file) = if root.is_dir() {
        (root.to_path_buf(), false)
    } else {
        (
            root.parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf),
            true,
        )
    };

    let mut files = Vec::new();
    if is_single_file {
        files.push(root.to_path_buf());
    } else {
        collect_supported(root, recursive, &mut files);
    }
    files.sort();

    if files.is_empty() {
        println!(
            "  no documents under {} (supported: {})",
            root.display(),
            extract::supported_summary()
        );
        return Ok(());
    }

    for file in &files {
        let rel = file.strip_prefix(&base_dir).unwrap_or(file);
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        // Taxonomy path = collection + the file's parent directories. The
        // filename is part of the key, not the namespace.
        let parent_rel = rel
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let taxonomy = join_taxonomy(collection, &parent_rel);
        let namespace = docs::namespace_for_path(&taxonomy);

        // Key includes the collection so the same relative path under different
        // corpora doesn't collide.
        let key_base = join_taxonomy(collection, &rel_str);
        let first_key = format!("{key_base}#0");
        if !force && memory.get(&first_key).await.ok().flatten().is_some() {
            stats.skipped += 1;
            continue;
        }

        let text = match extract::extract(file) {
            Ok(Extracted::Text(t)) => t,
            Ok(Extracted::NeedsFeature) => {
                stats.needs_feature += 1;
                continue;
            }
            Ok(Extracted::Unsupported) => continue,
            Err(e) => {
                eprintln!("  {} {}: {e}", style("skip").yellow(), rel_str);
                stats.failed += 1;
                continue;
            }
        };

        let chunks = chunker::chunk_markdown(&text, CHUNK_MAX_TOKENS);
        if chunks.is_empty() {
            continue;
        }

        let mut stored = 0usize;
        for chunk in &chunks {
            let key = format!("{key_base}#{}", chunk.index);
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
                stats.failed += 1;
                continue;
            }
            stored += 1;
        }
        stats.chunks_total += stored;
        stats.ingested += 1;
        println!(
            "  {} {rel_str} → {} ({stored} chunk(s))",
            style("ok").green(),
            namespace
        );
    }
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
        } else if path.is_file() && extract::is_known_doc(&path) {
            out.push(path);
        }
    }
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
}

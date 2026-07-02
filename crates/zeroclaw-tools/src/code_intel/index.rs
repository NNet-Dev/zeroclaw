use super::parser::{ParsedFile, parse_source};
use super::registry::LanguageRegistry;
use super::types::{CodeIntelError, Span, SymbolDef, SymbolInfo, SymbolRef};
use parking_lot::RwLock;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zeroclaw_config::coding::CodeIntelConfig;

pub type CodeIntelConfigResolver = Arc<dyn Fn() -> CodeIntelConfig + Send + Sync>;

pub struct SymbolIndex {
    workspace_root: PathBuf,
    registry: Arc<LanguageRegistry>,
    config: CodeIntelConfigResolver,
    cache: RwLock<IndexCache>,
}

#[derive(Debug, Default)]
struct IndexCache {
    files: HashMap<PathBuf, CachedFile>,
    order: VecDeque<PathBuf>,
    total_bytes: usize,
}

#[derive(Debug, Clone)]
struct CachedFile {
    digest: [u8; 32],
    bytes: usize,
    parsed: ParsedFile,
}

impl SymbolIndex {
    pub fn new(
        workspace_root: PathBuf,
        registry: Arc<LanguageRegistry>,
        config: CodeIntelConfigResolver,
    ) -> Self {
        Self {
            workspace_root,
            registry,
            config,
            cache: RwLock::new(IndexCache::default()),
        }
    }

    pub fn resolve(&self, name: &str) -> Result<Vec<SymbolDef>, CodeIntelError> {
        self.ensure_workspace_indexed()?;
        let cache = self.cache.read();
        Ok(cache
            .files
            .values()
            .flat_map(|file| file.parsed.definitions.iter())
            .filter(|def| def.name == name || def.signature.contains(name))
            .cloned()
            .collect())
    }

    pub fn references(&self, name: &str) -> Result<Vec<SymbolRef>, CodeIntelError> {
        self.ensure_workspace_indexed()?;
        let cache = self.cache.read();
        Ok(cache
            .files
            .values()
            .flat_map(|file| file.parsed.references.iter())
            .filter(|reference| reference.name == name)
            .cloned()
            .collect())
    }

    pub fn document_symbols(&self, path: &Path) -> Result<Vec<SymbolInfo>, CodeIntelError> {
        let normalized = self.normalize_path(path);
        let config = (self.config)();
        self.ensure_indexed(&normalized, &config)?;
        let cache = self.cache.read();
        Ok(cache
            .files
            .get(&normalized)
            .map(|file| {
                file.parsed
                    .definitions
                    .iter()
                    .cloned()
                    .map(|def| SymbolInfo {
                        def,
                        container: None,
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    pub fn symbols_in_span(
        &self,
        path: &Path,
        span: &Span,
    ) -> Result<Vec<SymbolDef>, CodeIntelError> {
        let normalized = self.normalize_path(path);
        let config = (self.config)();
        self.ensure_indexed(&normalized, &config)?;
        let cache = self.cache.read();
        Ok(cache
            .files
            .get(&normalized)
            .map(|file| {
                file.parsed
                    .definitions
                    .iter()
                    .filter(|def| spans_overlap(&def.span, span))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    pub fn invalidate(&self, path: &Path) {
        let normalized = self.normalize_path(path);
        let mut cache = self.cache.write();
        if let Some(file) = cache.files.remove(&normalized) {
            cache.total_bytes = cache.total_bytes.saturating_sub(file.bytes);
        }
        cache.order.retain(|entry| entry != &normalized);
    }

    pub fn post_edit_check_enabled(&self) -> bool {
        (self.config)().post_edit_check
    }

    fn ensure_workspace_indexed(&self) -> Result<(), CodeIntelError> {
        let config = (self.config)();
        for (visited, path) in self.source_files()?.into_iter().enumerate() {
            if visited >= config.max_indexed_files {
                break;
            }
            self.ensure_indexed(path.as_path(), &config)?;
        }
        Ok(())
    }

    fn ensure_indexed(&self, path: &Path, config: &CodeIntelConfig) -> Result<(), CodeIntelError> {
        let source = fs::read_to_string(path)
            .map_err(|e| CodeIntelError::Io(path.to_path_buf(), e.to_string()))?;
        let digest = digest(source.as_bytes());
        let bytes = source.len();

        {
            let cache = self.cache.read();
            if cache
                .files
                .get(path)
                .map(|cached| cached.digest == digest)
                .unwrap_or(false)
            {
                return Ok(());
            }
        }

        if bytes > config.max_indexed_bytes {
            return Err(CodeIntelError::BudgetExceeded);
        }

        let parsed = parse_source(&self.registry, path, &source)?;
        let mut cache = self.cache.write();
        if let Some(old) = cache.files.remove(path) {
            cache.total_bytes = cache.total_bytes.saturating_sub(old.bytes);
        }
        cache.total_bytes = cache.total_bytes.saturating_add(bytes);
        cache.files.insert(
            path.to_path_buf(),
            CachedFile {
                digest,
                bytes,
                parsed,
            },
        );
        cache.order.retain(|entry| entry != path);
        cache.order.push_back(path.to_path_buf());
        self.evict_over_budget(&mut cache, config);
        Ok(())
    }

    fn evict_over_budget(&self, cache: &mut IndexCache, config: &CodeIntelConfig) {
        while cache.files.len() > config.max_indexed_files
            || cache.total_bytes > config.max_indexed_bytes
        {
            let Some(path) = cache.order.pop_front() else {
                break;
            };
            if let Some(file) = cache.files.remove(&path) {
                cache.total_bytes = cache.total_bytes.saturating_sub(file.bytes);
            }
        }
    }

    fn source_files(&self) -> Result<Vec<PathBuf>, CodeIntelError> {
        let mut out = Vec::new();
        collect_source_files(&self.workspace_root, &self.registry, &mut out)?;
        out.sort();
        Ok(out)
    }

    fn normalize_path(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace_root.join(path)
        }
    }
}

fn collect_source_files(
    dir: &Path,
    registry: &LanguageRegistry,
    out: &mut Vec<PathBuf>,
) -> Result<(), CodeIntelError> {
    let entries =
        fs::read_dir(dir).map_err(|e| CodeIntelError::Io(dir.to_path_buf(), e.to_string()))?;
    for entry in entries {
        let entry = entry.map_err(|e| CodeIntelError::Io(dir.to_path_buf(), e.to_string()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| CodeIntelError::Io(path.clone(), e.to_string()))?;
        if file_type.is_dir() {
            if should_skip_dir(&path) {
                continue;
            }
            collect_source_files(&path, registry, out)?;
        } else if file_type.is_file() && registry.for_path(&path).is_some() {
            out.push(path);
        }
    }
    Ok(())
}

fn should_skip_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            matches!(
                name,
                ".git" | "target" | "node_modules" | "dist" | "build" | ".next" | ".venv"
            )
        })
        .unwrap_or(false)
}

fn spans_overlap(a: &Span, b: &Span) -> bool {
    if a.path != b.path {
        return false;
    }
    a.start_line <= b.end_line && b.start_line <= a.end_line
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn resolves_and_invalidates_rust_definition() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        fs::write(&file, "pub struct Widget {}\n").unwrap();
        let index = SymbolIndex::new(
            dir.path().to_path_buf(),
            Arc::new(LanguageRegistry::new()),
            Arc::new(CodeIntelConfig::default),
        );

        assert_eq!(index.resolve("Widget").unwrap().len(), 1);
        fs::write(&file, "pub struct Gadget {}\n").unwrap();
        index.invalidate(&file);

        assert!(index.resolve("Widget").unwrap().is_empty());
        assert_eq!(index.resolve("Gadget").unwrap().len(), 1);
    }
}

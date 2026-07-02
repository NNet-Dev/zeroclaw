mod index;
mod parser;
mod registry;
mod signature;
mod types;

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use signature::{render_symbol, render_symbols};
pub use types::{CodeIntelError, Span, SymbolDef, SymbolInfo, SymbolKind, SymbolRef};

use index::{CodeIntelConfigResolver, SymbolIndex};
use registry::LanguageRegistry;

#[derive(Clone)]
pub struct CodeIntel {
    index: Arc<SymbolIndex>,
}

impl CodeIntel {
    pub fn new(workspace_root: PathBuf, config: CodeIntelConfigResolver) -> Self {
        let registry = Arc::new(LanguageRegistry::new());
        Self {
            index: Arc::new(SymbolIndex::new(workspace_root, registry, config)),
        }
    }

    pub fn find_definition(&self, name: &str) -> Result<Vec<SymbolDef>, CodeIntelError> {
        self.index.resolve(name)
    }

    pub fn find_references(&self, name: &str) -> Result<Vec<SymbolRef>, CodeIntelError> {
        self.index.references(name)
    }

    pub fn document_symbol(&self, path: &Path) -> Result<Vec<SymbolInfo>, CodeIntelError> {
        self.index.document_symbols(path)
    }

    pub fn symbols_in_span(
        &self,
        path: &Path,
        span: &Span,
    ) -> Result<Vec<SymbolDef>, CodeIntelError> {
        self.index.symbols_in_span(path, span)
    }

    pub fn invalidate(&self, path: &Path) {
        self.index.invalidate(path);
    }

    pub fn post_edit_check_enabled(&self) -> bool {
        self.index.post_edit_check_enabled()
    }
}

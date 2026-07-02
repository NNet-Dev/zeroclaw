use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

/// A resolved source span. Lines are 1-based and columns are 0-based.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub path: PathBuf,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Impl,
    Module,
    Const,
    Field,
    Variable,
    Class,
    Interface,
    TypeAlias,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolDef {
    pub name: String,
    pub kind: SymbolKind,
    pub span: Span,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolRef {
    pub name: String,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolInfo {
    pub def: SymbolDef,
    pub container: Option<String>,
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CodeIntelError {
    #[error("unsupported language for {0}")]
    UnsupportedLanguage(PathBuf),
    #[error("parse failed for {0}: {1}")]
    ParseFailed(PathBuf, String),
    #[error("io error reading {0}: {1}")]
    Io(PathBuf, String),
    #[error("symbol index budget exceeded")]
    BudgetExceeded,
}

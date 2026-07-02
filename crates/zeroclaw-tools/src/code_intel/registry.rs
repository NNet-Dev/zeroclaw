use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageKind {
    Rust,
    Python,
    TypeScript,
}

#[derive(Debug, Clone, Copy)]
pub struct LanguageSpec {
    pub kind: LanguageKind,
    pub language: inkjet::Language,
}

#[derive(Debug, Default)]
pub struct LanguageRegistry;

impl LanguageRegistry {
    pub fn new() -> Self {
        Self
    }

    pub fn for_path(&self, path: &Path) -> Option<LanguageSpec> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "rs" => Some(LanguageSpec {
                kind: LanguageKind::Rust,
                language: inkjet::Language::Rust,
            }),
            "py" => Some(LanguageSpec {
                kind: LanguageKind::Python,
                language: inkjet::Language::Python,
            }),
            "ts" => Some(LanguageSpec {
                kind: LanguageKind::TypeScript,
                language: inkjet::Language::Typescript,
            }),
            _ => None,
        }
    }
}

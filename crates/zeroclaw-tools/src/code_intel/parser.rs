use super::registry::{LanguageKind, LanguageRegistry};
use super::types::{CodeIntelError, Span, SymbolDef, SymbolKind, SymbolRef};
use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub definitions: Vec<SymbolDef>,
    pub references: Vec<SymbolRef>,
}

pub fn parse_source(
    registry: &LanguageRegistry,
    path: &Path,
    source: &str,
) -> Result<ParsedFile, CodeIntelError> {
    let spec = registry
        .for_path(path)
        .ok_or_else(|| CodeIntelError::UnsupportedLanguage(path.to_path_buf()))?;

    // Inkjet's highlighter is tree-sitter based and validates that the bundled
    // grammar can parse this source. Symbol extraction below stays conservative.
    let mut highlighter = inkjet::Highlighter::new();
    let highlights = highlighter
        .highlight_raw(spec.language, &source)
        .map_err(|e| CodeIntelError::ParseFailed(path.to_path_buf(), e.to_string()))?;
    for event in highlights {
        event.map_err(|e| CodeIntelError::ParseFailed(path.to_path_buf(), e.to_string()))?;
    }

    let definitions = definitions_for_language(spec.kind, path, source);
    let references = identifier_references(path, source);

    Ok(ParsedFile {
        definitions,
        references,
    })
}

fn definitions_for_language(kind: LanguageKind, path: &Path, source: &str) -> Vec<SymbolDef> {
    match kind {
        LanguageKind::Rust => rust_definitions(path, source),
        LanguageKind::Python => python_definitions(path, source),
        LanguageKind::TypeScript => typescript_definitions(path, source),
    }
}

fn rust_definitions(path: &Path, source: &str) -> Vec<SymbolDef> {
    let patterns = [
        (rust_fn_re(), SymbolKind::Function),
        (rust_struct_re(), SymbolKind::Struct),
        (rust_enum_re(), SymbolKind::Enum),
        (rust_trait_re(), SymbolKind::Trait),
        (rust_impl_re(), SymbolKind::Impl),
        (rust_mod_re(), SymbolKind::Module),
        (rust_const_re(), SymbolKind::Const),
    ];
    collect_line_defs(path, source, &patterns)
}

fn python_definitions(path: &Path, source: &str) -> Vec<SymbolDef> {
    let patterns = [
        (python_fn_re(), SymbolKind::Function),
        (python_class_re(), SymbolKind::Class),
    ];
    collect_line_defs(path, source, &patterns)
}

fn typescript_definitions(path: &Path, source: &str) -> Vec<SymbolDef> {
    let patterns = [
        (ts_fn_re(), SymbolKind::Function),
        (ts_class_re(), SymbolKind::Class),
        (ts_interface_re(), SymbolKind::Interface),
        (ts_type_re(), SymbolKind::TypeAlias),
        (ts_enum_re(), SymbolKind::Enum),
        (ts_var_re(), SymbolKind::Variable),
    ];
    collect_line_defs(path, source, &patterns)
}

fn collect_line_defs(
    path: &Path,
    source: &str,
    patterns: &[(&Regex, SymbolKind)],
) -> Vec<SymbolDef> {
    let mut defs = Vec::new();
    for (line_idx, line) in source.lines().enumerate() {
        for (regex, kind) in patterns {
            let Some(caps) = regex.captures(line) else {
                continue;
            };
            let Some(name_match) = caps.name("name") else {
                continue;
            };
            defs.push(SymbolDef {
                name: name_match.as_str().to_string(),
                kind: *kind,
                span: Span {
                    path: path.to_path_buf(),
                    start_line: (line_idx + 1) as u32,
                    start_col: name_match.start() as u32,
                    end_line: (line_idx + 1) as u32,
                    end_col: name_match.end() as u32,
                },
                signature: line.trim().to_string(),
            });
            break;
        }
    }
    defs
}

fn identifier_references(path: &Path, source: &str) -> Vec<SymbolRef> {
    let mut refs = Vec::new();
    for (line_idx, line) in source.lines().enumerate() {
        for matched in identifier_re().find_iter(line) {
            let name = matched.as_str();
            if is_keyword(name) {
                continue;
            }
            refs.push(SymbolRef {
                name: name.to_string(),
                span: Span {
                    path: path.to_path_buf(),
                    start_line: (line_idx + 1) as u32,
                    start_col: matched.start() as u32,
                    end_line: (line_idx + 1) as u32,
                    end_col: matched.end() as u32,
                },
            });
        }
    }
    refs
}

fn is_keyword(name: &str) -> bool {
    matches!(
        name,
        "as" | "async"
            | "await"
            | "break"
            | "class"
            | "const"
            | "continue"
            | "def"
            | "else"
            | "enum"
            | "export"
            | "false"
            | "fn"
            | "for"
            | "from"
            | "function"
            | "if"
            | "impl"
            | "import"
            | "in"
            | "interface"
            | "let"
            | "match"
            | "mod"
            | "pub"
            | "return"
            | "self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "var"
            | "where"
            | "while"
    )
}

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("code-intel regex must compile"))
}

fn rust_fn_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(
        &CELL,
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+)?fn\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
    )
}

fn rust_struct_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(
        &CELL,
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?struct\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
    )
}

fn rust_enum_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(
        &CELL,
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?enum\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
    )
}

fn rust_trait_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(
        &CELL,
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?trait\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
    )
}

fn rust_impl_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(
        &CELL,
        r"^\s*impl(?:<[^>]+>)?\s+(?:[A-Za-z_][A-Za-z0-9_:<>]*\s+for\s+)?(?P<name>[A-Za-z_][A-Za-z0-9_:<>]*)",
    )
}

fn rust_mod_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(
        &CELL,
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
    )
}

fn rust_const_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(
        &CELL,
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const|static)\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
    )
}

fn python_fn_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(
        &CELL,
        r"^\s*(?:async\s+)?def\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
    )
}

fn python_class_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(&CELL, r"^\s*class\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)")
}

fn ts_fn_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(
        &CELL,
        r"^\s*(?:export\s+)?(?:async\s+)?function\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
    )
}

fn ts_class_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(
        &CELL,
        r"^\s*(?:export\s+)?class\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
    )
}

fn ts_interface_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(
        &CELL,
        r"^\s*(?:export\s+)?interface\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
    )
}

fn ts_type_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(
        &CELL,
        r"^\s*(?:export\s+)?type\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
    )
}

fn ts_enum_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(
        &CELL,
        r"^\s*(?:export\s+)?enum\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
    )
}

fn ts_var_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(
        &CELL,
        r"^\s*(?:export\s+)?(?:const|let|var)\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
    )
}

fn identifier_re() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    regex(&CELL, r"[A-Za-z_][A-Za-z0-9_]*")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parses_rust_definitions() {
        let registry = LanguageRegistry::new();
        let parsed = parse_source(
            &registry,
            &PathBuf::from("sample.rs"),
            "pub struct Widget {}\nimpl Widget {}\npub async fn make_widget() {}\n",
        )
        .unwrap();

        assert!(parsed.definitions.iter().any(|def| def.name == "Widget"));
        assert!(
            parsed
                .definitions
                .iter()
                .any(|def| def.name == "make_widget")
        );
    }

    #[test]
    fn unsupported_language_fails_open_at_boundary() {
        let registry = LanguageRegistry::new();
        let err = parse_source(&registry, &PathBuf::from("sample.txt"), "hello").unwrap_err();

        assert!(matches!(err, CodeIntelError::UnsupportedLanguage(_)));
    }
}

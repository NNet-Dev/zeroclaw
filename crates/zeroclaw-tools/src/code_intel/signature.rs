use super::types::SymbolDef;

pub fn render_symbol(def: &SymbolDef) -> String {
    format!(
        "- {} ({:?})\n  defined: {}:{}:{}\n  signature: {}",
        def.name,
        def.kind,
        def.span.path.display(),
        def.span.start_line,
        def.span.start_col,
        def.signature
    )
}

pub fn render_symbols(defs: &[SymbolDef], max_chars: usize) -> String {
    let mut out = String::new();
    for def in defs {
        let rendered = render_symbol(def);
        let next_len = out.len() + rendered.len() + usize::from(!out.is_empty());
        if next_len > max_chars {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&rendered);
    }
    out
}

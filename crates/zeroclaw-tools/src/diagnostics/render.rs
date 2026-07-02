use zeroclaw_api::diagnostics::{Diagnostic, DiagnosticSeverity};

pub fn render_diagnostics_block(diagnostics: &[Diagnostic], cap: usize) -> String {
    if diagnostics.is_empty() || cap == 0 {
        return String::new();
    }

    let mut ordered: Vec<&Diagnostic> = diagnostics.iter().collect();
    ordered.sort_by_key(|diagnostic| diagnostic.severity.rank());

    let mut block = String::from("<diagnostics>");
    for diagnostic in ordered {
        let line = render_line(diagnostic);
        if block.len() + 1 + line.len() + "\n</diagnostics>".len() > cap {
            break;
        }
        block.push('\n');
        block.push_str(&line);
    }
    block.push_str("\n</diagnostics>");

    if block == "<diagnostics>\n</diagnostics>" {
        String::new()
    } else {
        block
    }
}

fn render_line(diagnostic: &Diagnostic) -> String {
    let severity = match diagnostic.severity {
        DiagnosticSeverity::Error => "ERROR",
        DiagnosticSeverity::Warning => "WARNING",
        DiagnosticSeverity::Note => "NOTE",
    };
    let location = match (&diagnostic.file, diagnostic.line, diagnostic.col) {
        (file, Some(line), Some(col)) if !file.is_empty() => format!(" {file} [{line}:{col}]"),
        (file, Some(line), None) if !file.is_empty() => format!(" {file} [{line}]"),
        (file, None, _) if !file.is_empty() => format!(" {file}"),
        (_, Some(line), Some(col)) => format!(" [{line}:{col}]"),
        (_, Some(line), None) => format!(" [{line}]"),
        _ => String::new(),
    };
    let code = diagnostic
        .code
        .as_deref()
        .map(|code| format!(" [{code}]"))
        .unwrap_or_default();
    format!("{severity}{location} {}{code}", diagnostic.message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(severity: DiagnosticSeverity, message: &str) -> Diagnostic {
        Diagnostic {
            file: "src/main.rs".to_string(),
            line: Some(1),
            col: Some(0),
            severity,
            code: None,
            message: message.to_string(),
        }
    }

    #[test]
    fn renders_errors_before_warnings() {
        let block = render_diagnostics_block(
            &[
                diag(DiagnosticSeverity::Warning, "unused variable"),
                diag(DiagnosticSeverity::Error, "cannot find value"),
            ],
            1_000,
        );

        let error_index = block.find("ERROR").unwrap();
        let warning_index = block.find("WARNING").unwrap();
        assert!(error_index < warning_index, "{block}");
    }

    #[test]
    fn cap_preserves_earlier_errors() {
        let block = render_diagnostics_block(
            &[
                diag(DiagnosticSeverity::Error, "first error"),
                diag(DiagnosticSeverity::Warning, &"warning ".repeat(100)),
            ],
            120,
        );

        assert!(block.contains("first error"));
        assert!(!block.contains("warning warning warning"));
    }

    #[test]
    fn empty_input_renders_empty() {
        assert_eq!(render_diagnostics_block(&[], 1_000), "");
    }
}

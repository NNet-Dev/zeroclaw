use regex::Regex;
use serde::Deserialize;
use std::sync::OnceLock;
use zeroclaw_api::diagnostics::{Diagnostic, DiagnosticSeverity};

pub fn parse_diagnostics(tool_name: &str, args: &serde_json::Value, raw: &str) -> Vec<Diagnostic> {
    let diagnostics = parse_cargo_json(raw);
    if !diagnostics.is_empty() {
        return diagnostics;
    }

    let diagnostics = parse_pyright_json(raw);
    if !diagnostics.is_empty() {
        return diagnostics;
    }

    if !looks_like_diagnostic_invocation(tool_name, args) {
        return Vec::new();
    }

    parse_text_diagnostics(raw)
}

fn looks_like_diagnostic_invocation(tool_name: &str, args: &serde_json::Value) -> bool {
    let haystack = format!(
        "{} {}",
        tool_name,
        args.get("command")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
    )
    .to_ascii_lowercase();

    ["cargo", "rustc", "tsc", "pyright", "pytest", "mypy"]
        .iter()
        .any(|needle| haystack.contains(needle))
}

fn parse_cargo_json(raw: &str) -> Vec<Diagnostic> {
    raw.lines()
        .filter_map(|line| serde_json::from_str::<CargoMessage>(line).ok())
        .filter(|msg| msg.reason.as_deref() == Some("compiler-message"))
        .filter_map(|msg| {
            let message = msg.message?;
            let primary = message
                .spans
                .iter()
                .find(|span| span.is_primary)
                .or_else(|| message.spans.first());
            Some(Diagnostic {
                file: primary
                    .map(|span| span.file_name.clone())
                    .unwrap_or_default(),
                line: primary.and_then(|span| span.line_start),
                col: primary
                    .and_then(|span| span.column_start)
                    .map(|col| col.saturating_sub(1)),
                severity: severity_from_str(&message.level),
                code: message.code.and_then(|code| code.code),
                message: message.message,
            })
        })
        .collect()
}

fn parse_pyright_json(raw: &str) -> Vec<Diagnostic> {
    let Ok(report) = serde_json::from_str::<PyrightReport>(raw) else {
        return Vec::new();
    };

    report
        .general_diagnostics
        .into_iter()
        .map(|diag| Diagnostic {
            file: diag.file.unwrap_or_default(),
            line: diag
                .range
                .as_ref()
                .map(|range| range.start.line.saturating_add(1)),
            col: diag.range.as_ref().map(|range| range.start.character),
            severity: severity_from_str(&diag.severity),
            code: diag.rule,
            message: diag.message,
        })
        .collect()
}

fn parse_text_diagnostics(raw: &str) -> Vec<Diagnostic> {
    raw.lines()
        .filter_map(|line| parse_tsc_line(line).or_else(|| parse_colon_line(line)))
        .collect()
}

fn parse_tsc_line(line: &str) -> Option<Diagnostic> {
    static TSC_RE: OnceLock<Regex> = OnceLock::new();
    let re = TSC_RE.get_or_init(|| {
        Regex::new(r"^(?P<file>.+)\((?P<line>\d+),(?P<col>\d+)\):\s*(?P<severity>error|warning|note)\s*(?P<code>[A-Z]+\d+)?:?\s*(?P<message>.+)$")
            .expect("valid tsc diagnostic regex")
    });
    let captures = re.captures(line)?;
    Some(Diagnostic {
        file: captures.name("file")?.as_str().to_string(),
        line: parse_u32(captures.name("line")?.as_str()),
        col: parse_u32(captures.name("col")?.as_str()).map(|col| col.saturating_sub(1)),
        severity: severity_from_str(captures.name("severity")?.as_str()),
        code: captures
            .name("code")
            .map(|code| code.as_str().trim().to_string())
            .filter(|code| !code.is_empty()),
        message: captures.name("message")?.as_str().trim().to_string(),
    })
}

fn parse_colon_line(line: &str) -> Option<Diagnostic> {
    static COLON_RE: OnceLock<Regex> = OnceLock::new();
    let re = COLON_RE.get_or_init(|| {
        Regex::new(r"^(?P<file>.+?):(?P<line>\d+):(?P<col>\d+):\s*(?P<severity>error|warning|note)(?:\[[^\]]+\])?:\s*(?P<message>.+)$")
            .expect("valid colon diagnostic regex")
    });
    let captures = re.captures(line)?;
    Some(Diagnostic {
        file: captures.name("file")?.as_str().to_string(),
        line: parse_u32(captures.name("line")?.as_str()),
        col: parse_u32(captures.name("col")?.as_str()).map(|col| col.saturating_sub(1)),
        severity: severity_from_str(captures.name("severity")?.as_str()),
        code: None,
        message: captures.name("message")?.as_str().trim().to_string(),
    })
}

fn parse_u32(value: &str) -> Option<u32> {
    value.parse().ok()
}

fn severity_from_str(value: &str) -> DiagnosticSeverity {
    match value.to_ascii_lowercase().as_str() {
        "error" => DiagnosticSeverity::Error,
        "warning" | "warn" => DiagnosticSeverity::Warning,
        _ => DiagnosticSeverity::Note,
    }
}

#[derive(Deserialize)]
struct CargoMessage {
    reason: Option<String>,
    message: Option<RustcMessage>,
}

#[derive(Deserialize)]
struct RustcMessage {
    message: String,
    level: String,
    code: Option<RustcCode>,
    #[serde(default)]
    spans: Vec<RustcSpan>,
}

#[derive(Deserialize)]
struct RustcCode {
    code: Option<String>,
}

#[derive(Deserialize)]
struct RustcSpan {
    file_name: String,
    line_start: Option<u32>,
    column_start: Option<u32>,
    #[serde(default)]
    is_primary: bool,
}

#[derive(Deserialize)]
struct PyrightReport {
    #[serde(default, rename = "generalDiagnostics")]
    general_diagnostics: Vec<PyrightDiagnostic>,
}

#[derive(Deserialize)]
struct PyrightDiagnostic {
    file: Option<String>,
    severity: String,
    message: String,
    rule: Option<String>,
    range: Option<PyrightRange>,
}

#[derive(Deserialize)]
struct PyrightRange {
    start: PyrightPosition,
}

#[derive(Deserialize)]
struct PyrightPosition {
    line: u32,
    character: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_cargo_json_compiler_message() {
        let raw = r#"{"reason":"compiler-message","message":{"message":"cannot find value `barr` in this scope","code":{"code":"E0425"},"level":"error","spans":[{"file_name":"src/main.rs","line_start":42,"column_start":17,"is_primary":true}]}}"#;
        let diagnostics = parse_diagnostics(
            "shell",
            &json!({"command":"cargo check --message-format=json"}),
            raw,
        );

        assert_eq!(diagnostics.len(), 1);
        let diag = &diagnostics[0];
        assert_eq!(diag.file, "src/main.rs");
        assert_eq!(diag.line, Some(42));
        assert_eq!(diag.col, Some(16));
        assert_eq!(diag.severity, DiagnosticSeverity::Error);
        assert_eq!(diag.code.as_deref(), Some("E0425"));
        assert!(diag.message.contains("cannot find value"));
    }

    #[test]
    fn unknown_tool_output_returns_empty() {
        assert!(parse_diagnostics("ls", &json!({}), "plain text").is_empty());
    }

    #[test]
    fn malformed_json_returns_empty() {
        assert!(
            parse_diagnostics("shell", &json!({"command":"cargo check"}), "{\"reason\"").is_empty()
        );
    }

    #[test]
    fn parses_tsc_text_line() {
        let diagnostics = parse_diagnostics(
            "shell",
            &json!({"command":"tsc --noEmit"}),
            "src/app.ts(10,5): error TS2304: Cannot find name 'foo'.",
        );

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].file, "src/app.ts");
        assert_eq!(diagnostics[0].line, Some(10));
        assert_eq!(diagnostics[0].col, Some(4));
        assert_eq!(diagnostics[0].code.as_deref(), Some("TS2304"));
    }

    #[test]
    fn parses_pyright_json() {
        let raw = r#"{"generalDiagnostics":[{"file":"src/app.py","severity":"error","message":"x is not defined","rule":"reportUndefinedVariable","range":{"start":{"line":2,"character":8}}}]}"#;
        let diagnostics =
            parse_diagnostics("shell", &json!({"command":"pyright --outputjson"}), raw);

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].line, Some(3));
        assert_eq!(diagnostics[0].col, Some(8));
        assert_eq!(
            diagnostics[0].code.as_deref(),
            Some("reportUndefinedVariable")
        );
    }
}

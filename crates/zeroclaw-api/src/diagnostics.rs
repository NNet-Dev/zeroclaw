use serde::{Deserialize, Serialize};

/// Severity for a structured diagnostic emitted by a compiler, checker, or
/// verifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Note,
}

impl DiagnosticSeverity {
    pub fn rank(self) -> u8 {
        match self {
            Self::Error => 0,
            Self::Warning => 1,
            Self::Note => 2,
        }
    }
}

/// A single structured diagnostic parsed from toolchain output.
///
/// Coordinate convention: `line` is 1-based and `col` is 0-based.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub file: String,
    pub line: Option<u32>,
    pub col: Option<u32>,
    pub severity: DiagnosticSeverity,
    pub code: Option<String>,
    pub message: String,
}

/// Stable diagnostic identity used by later baseline-delta verification.
///
/// Line and column are deliberately excluded so inserting lines above a
/// pre-existing diagnostic does not make it appear new.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DiagnosticKey {
    pub file: String,
    pub code: Option<String>,
    pub message: String,
}

impl Diagnostic {
    pub fn key(&self) -> DiagnosticKey {
        DiagnosticKey {
            file: self.file.clone(),
            code: self.code.clone(),
            message: self.message.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn diagnostic_key_excludes_line_and_column() {
        let first = Diagnostic {
            file: "src/main.rs".to_string(),
            line: Some(10),
            col: Some(4),
            severity: DiagnosticSeverity::Error,
            code: Some("E0425".to_string()),
            message: "cannot find value".to_string(),
        };
        let shifted = Diagnostic {
            line: Some(20),
            col: Some(8),
            ..first.clone()
        };

        assert_eq!(first.key(), shifted.key());

        let mut keys = HashSet::new();
        keys.insert(first.key());
        assert!(keys.contains(&shifted.key()));
    }

    #[test]
    fn severity_round_trips_as_snake_case() {
        let encoded = serde_json::to_string(&DiagnosticSeverity::Warning).unwrap();
        assert_eq!(encoded, "\"warning\"");
        let decoded: DiagnosticSeverity = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, DiagnosticSeverity::Warning);
    }
}

//! Coding-agent reliability configuration.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use zeroclaw_macros::Configurable;

/// Coding-agent reliability settings (`[coding]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "coding"]
pub struct CodingConfig {
    /// Post-edit verification settings (`[coding.verify]`).
    #[serde(default)]
    #[nested]
    pub verify: VerifyConfig,
    /// Code-intelligence backend settings (`[coding.code_intel]`).
    #[serde(default)]
    #[nested]
    pub code_intel: CodeIntelConfig,
}

/// Tree-sitter-backed code-intelligence settings.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "coding.code_intel"]
pub struct CodeIntelConfig {
    /// Enable the code-intelligence backend and symbol pull-tool.
    #[serde(default)]
    pub enabled: bool,
    /// Register the symbol_search pull-tool when code intelligence is enabled.
    #[serde(default = "default_symbol_search_tool")]
    pub symbol_search_tool: bool,
    /// Enable proactive pre-edit symbol context injection.
    #[serde(default = "default_pre_edit_injection")]
    pub pre_edit_injection: bool,
    /// Enable post-edit symbol checks.
    #[serde(default = "default_post_edit_check")]
    pub post_edit_check: bool,
    /// Maximum characters available to future injected symbol context.
    #[serde(default = "default_max_injection_chars")]
    pub max_injection_chars: usize,
    /// Maximum files held in the symbol index.
    #[serde(default = "default_max_indexed_files")]
    pub max_indexed_files: usize,
    /// Maximum source bytes held in the symbol index.
    #[serde(default = "default_max_indexed_bytes")]
    pub max_indexed_bytes: usize,
}

impl Default for CodeIntelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            symbol_search_tool: default_symbol_search_tool(),
            pre_edit_injection: default_pre_edit_injection(),
            post_edit_check: default_post_edit_check(),
            max_injection_chars: default_max_injection_chars(),
            max_indexed_files: default_max_indexed_files(),
            max_indexed_bytes: default_max_indexed_bytes(),
        }
    }
}

/// Engine-driven post-edit verification settings.
#[derive(Debug, Clone, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[prefix = "coding.verify"]
pub struct VerifyConfig {
    /// Enable engine-driven verification after configured edit tools.
    #[serde(default)]
    pub enabled: bool,
    /// Tool names that can trigger verification.
    #[serde(default = "default_verify_tools")]
    pub on: Vec<String>,
    /// Language-to-command map, e.g. `rust = "cargo check"`.
    #[serde(default)]
    pub commands: HashMap<String, String>,
    /// Only surface verifier output that was not present before the edit.
    #[serde(default = "default_baseline_delta")]
    pub baseline_delta: bool,
    /// Maximum repair turns injected for one loop before surfacing failure.
    #[serde(default = "default_max_repair_turns")]
    pub max_repair_turns: usize,
    /// Wall-clock timeout for each verify command.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            on: default_verify_tools(),
            commands: HashMap::new(),
            baseline_delta: default_baseline_delta(),
            max_repair_turns: default_max_repair_turns(),
            timeout_secs: default_timeout_secs(),
        }
    }
}

fn default_verify_tools() -> Vec<String> {
    vec!["file_write".to_string(), "file_edit".to_string()]
}

fn default_baseline_delta() -> bool {
    true
}

fn default_max_repair_turns() -> usize {
    2
}

fn default_timeout_secs() -> u64 {
    60
}

fn default_symbol_search_tool() -> bool {
    true
}

fn default_pre_edit_injection() -> bool {
    true
}

fn default_post_edit_check() -> bool {
    true
}

fn default_max_injection_chars() -> usize {
    4_000
}

fn default_max_indexed_files() -> usize {
    2_000
}

fn default_max_indexed_bytes() -> usize {
    64 * 1024 * 1024
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coding_verify_defaults_off() {
        let cfg: CodingConfig = toml::from_str("").expect("empty coding config should parse");

        assert!(!cfg.verify.enabled);
        assert!(!cfg.code_intel.enabled);
        assert!(cfg.code_intel.symbol_search_tool);
        assert!(cfg.code_intel.pre_edit_injection);
        assert!(cfg.code_intel.post_edit_check);
        assert_eq!(cfg.code_intel.max_injection_chars, 4_000);
        assert_eq!(cfg.code_intel.max_indexed_files, 2_000);
        assert_eq!(cfg.code_intel.max_indexed_bytes, 64 * 1024 * 1024);
        assert_eq!(cfg.verify.on, vec!["file_write", "file_edit"]);
        assert!(cfg.verify.commands.is_empty());
        assert!(cfg.verify.baseline_delta);
        assert_eq!(cfg.verify.max_repair_turns, 2);
        assert_eq!(cfg.verify.timeout_secs, 60);
    }

    #[test]
    fn coding_verify_partial_config_keeps_safe_defaults() {
        let cfg: CodingConfig = toml::from_str(
            r#"
            [verify]
            enabled = true
            "#,
        )
        .expect("partial verify config should parse");

        assert!(cfg.verify.enabled);
        assert_eq!(cfg.verify.on, vec!["file_write", "file_edit"]);
        assert!(cfg.verify.commands.is_empty());
        assert!(cfg.verify.baseline_delta);
        assert_eq!(cfg.verify.max_repair_turns, 2);
        assert_eq!(cfg.verify.timeout_secs, 60);
    }

    #[test]
    fn coding_code_intel_partial_config_keeps_safe_defaults() {
        let cfg: CodingConfig = toml::from_str(
            r#"
            [code_intel]
            enabled = true
            max_indexed_files = 12
            "#,
        )
        .expect("partial code-intel config should parse");

        assert!(cfg.code_intel.enabled);
        assert!(cfg.code_intel.symbol_search_tool);
        assert!(cfg.code_intel.pre_edit_injection);
        assert!(cfg.code_intel.post_edit_check);
        assert_eq!(cfg.code_intel.max_injection_chars, 4_000);
        assert_eq!(cfg.code_intel.max_indexed_files, 12);
        assert_eq!(cfg.code_intel.max_indexed_bytes, 64 * 1024 * 1024);
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coding_verify_defaults_off() {
        let cfg: CodingConfig = toml::from_str("").expect("empty coding config should parse");

        assert!(!cfg.verify.enabled);
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
}

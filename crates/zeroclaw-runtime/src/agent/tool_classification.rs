//! Shared tool side-effect classification for loop accounting.

use zeroclaw_api::tool::ToolSideEffect;

/// Side-effect class consumed by loop progress accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolClass {
    Idempotent,
    Mutating,
    Unknown,
}

pub(crate) fn classify(side_effect: ToolSideEffect) -> ToolClass {
    match side_effect {
        ToolSideEffect::ReadOnly => ToolClass::Idempotent,
        ToolSideEffect::Mutating => ToolClass::Mutating,
        ToolSideEffect::Unknown => ToolClass::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_is_idempotent() {
        assert_eq!(classify(ToolSideEffect::ReadOnly), ToolClass::Idempotent);
    }

    #[test]
    fn mutating_stays_mutating() {
        assert_eq!(classify(ToolSideEffect::Mutating), ToolClass::Mutating);
    }

    #[test]
    fn unknown_stays_unknown() {
        assert_eq!(classify(ToolSideEffect::Unknown), ToolClass::Unknown);
    }
}

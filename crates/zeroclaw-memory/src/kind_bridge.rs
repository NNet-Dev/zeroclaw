//! Mapping helpers between knowledge graph node kinds and memory kinds.

use crate::knowledge_graph::NodeType;
use crate::traits::{MemoryKind, SemanticSubtype};

/// Map a knowledge graph node type onto the semantic memory taxonomy.
pub fn node_type_to_semantic_subtype(node_type: NodeType) -> SemanticSubtype {
    match node_type {
        NodeType::Decision => SemanticSubtype::Decision,
        NodeType::Expert | NodeType::Client | NodeType::Contact => SemanticSubtype::Entity,
        NodeType::Pattern | NodeType::Lesson | NodeType::Technology | NodeType::Interaction => {
            SemanticSubtype::Fact
        }
    }
}

/// Map a knowledge graph node type onto a recallable memory kind.
pub fn node_type_to_memory_kind(node_type: NodeType) -> MemoryKind {
    MemoryKind::Semantic(node_type_to_semantic_subtype(node_type))
}

/// Stable source label used by KG-origin recall candidates.
pub fn node_source_key(node_id: &str) -> String {
    format!("kg:{node_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_node_types_to_semantic_subtypes() {
        assert_eq!(
            node_type_to_semantic_subtype(NodeType::Decision),
            SemanticSubtype::Decision
        );
        assert_eq!(
            node_type_to_semantic_subtype(NodeType::Expert),
            SemanticSubtype::Entity
        );
        assert_eq!(
            node_type_to_semantic_subtype(NodeType::Pattern),
            SemanticSubtype::Fact
        );
    }

    #[test]
    fn source_key_is_namespaced() {
        assert_eq!(node_source_key("abc"), "kg:abc");
    }
}

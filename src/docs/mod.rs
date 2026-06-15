//! `zeroclaw docs` — ingest and search a tool-gated document corpus (RAG).
//!
//! Documents are chunked, embedded, and stored in the shared memory backend
//! under the reserved `docs/` namespace (see [`zeroclaw_memory::docs`]). The
//! ingest folder structure becomes the taxonomy, and the agent reaches the
//! corpus only on demand through the `docs_search` tool — it is never
//! auto-injected into per-turn conversational context.

pub mod cli;

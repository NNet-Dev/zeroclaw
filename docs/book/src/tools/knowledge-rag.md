# Knowledge & document RAG

Give an agent a searchable library of your documents — past work, reference
material, a knowledge base — that it can look things up in on demand. ZeroClaw
ingests files into a vector store and exposes them to the agent through the
`docs_search` tool.

The corpus is **pull-based**: it is reached only when the agent calls
`docs_search`, never auto-injected into the conversation. So you can load
hundreds of documents without flooding the agent's per-turn context — unlike
conversational memory, which is recalled every turn.

## Quick start (no extra setup)

The default SQLite backend already stores and searches documents — no external
services required.

```bash
# Ingest a folder of documents into the corpus
zeroclaw docs ingest ~/work/prior-art

# Search it
zeroclaw docs search "patent claim drafting"

# See what's loaded
zeroclaw docs list
```

Out of the box this is keyword (BM25) search. For semantic search, configure an
embedding provider under `[memory]` (`embedding_provider = "openai"` or
`"custom:<url>"`); the same setting powers conversational memory.

> **Document formats:** `.md` and `.txt` work in any build. PDF, DOCX, XLSX,
> and PPTX require the `docs-extract` build feature
> (`cargo build --features docs-extract`); without it those files are reported
> as skipped during ingest.

## Folders become the taxonomy

When you ingest a directory, its folder structure becomes a hierarchical
category path. Ingesting `~/teaching` maps:

```
~/teaching/mathematics/year-9/algebra.md   →   teaching/mathematics/year-9
```

You can then drill into a subtree at search time:

```bash
zeroclaw docs search "quadratics" --scope teaching/mathematics
```

No manual tagging — reorganise by moving files.

## Corpora & per-agent subscriptions

A **corpus** is a named, reusable set of sources defined once with a
`[knowledge_bundles.<name>]` block. Agents subscribe to one or more by listing
them in `knowledge_bundles` — the same pattern as `skill_bundles` and
`mcp_bundles`.

```toml
# Define corpora once, globally
[knowledge_bundles.prior_art]
sources = ["~/work/prior-art"]

[knowledge_bundles.teaching]
sources = ["~/teaching"]

# Each agent subscribes to the corpora it should see
[agents.clamps]
knowledge_bundles = ["prior_art"]

[agents.tutor]
knowledge_bundles = ["teaching"]
```

Ingest everything a corpus references with:

```bash
zeroclaw docs sync               # all configured corpora
zeroclaw docs sync --bundle teaching
```

A bundle's documents land under `docs/<bundle>/…`, so each corpus is isolated.

### Subscriptions are a hard boundary

An agent's `docs_search` can retrieve **only** from the corpora it subscribes
to. Querying an unsubscribed corpus returns nothing — there is no escape hatch.
This mirrors the cross-agent `read_memory_from` allowlist, and it's what keeps,
say, one agent's work separate from another's. An agent subscribed to no corpora
retrieves no documents at all.

## Two kinds of source

A bundle's `sources` can mix two intents, and results from both are merged:

| Source | Intent | Behaviour |
|---|---|---|
| `~/work/prior-art`, `file:///shared/docs` | **doc folder** | ingested into the internal store under `docs/<bundle>` |
| `qdrant://host:6333/collection`, `qdrant+https://KEY@host/collection` | **RAG index** | an existing vector index, queried live at search time — never ingested |

```toml
[knowledge_bundles.legal]
sources = [
  "~/legal/contracts",                              # ingested folder
  "qdrant+https://KEY@xyz.cloud.qdrant.io/case_law" # federated live index
]
```

Folder sources work on the default SQLite backend. The `qdrant://` index intent
requires a Qdrant instance you already run — see
[Qdrant (vector database)](../setup/qdrant.md) for how to stand one up and wire
it in.

## CLI reference

| Command | Purpose |
|---|---|
| `zeroclaw docs ingest <path> [--collection N]` | Ingest a file or directory ad hoc |
| `zeroclaw docs sync [--bundle N]` | Ingest configured `knowledge_bundles` from their `sources` |
| `zeroclaw docs search <query> [--scope path]` | Search the corpus (operator view: all of `docs/`) |
| `zeroclaw docs list` | List ingested categories and chunk counts |

## Backends & scale

The corpus lives in whatever `[memory]` backend you've configured:

- **SQLite** (default) — zero setup; fine for hundreds to low thousands of
  chunks.
- **Qdrant / Postgres+pgvector** — for large corpora, shared/multi-instance
  deployments, or federating an existing vector index. See
  [Qdrant (vector database)](../setup/qdrant.md).

# Qdrant (vector database)

[Qdrant](https://qdrant.tech) is an optional vector-search backend. ZeroClaw is
a **client** of Qdrant — it connects to an instance you run (or a managed one),
it does not provision or manage the server for you.

You only need Qdrant for scale, shared/multi-instance deployments, or to
federate an existing vector index into [knowledge & document
RAG](../tools/knowledge-rag.md). For most setups the default SQLite backend is
enough and needs no external service.

## Running Qdrant

Pick whichever fits your environment.

### Docker / Compose

```bash
docker run -p 6333:6333 -p 6334:6334 \
  -v "$(pwd)/qdrant_storage:/qdrant/storage" \
  qdrant/qdrant
```

Or a `docker-compose.yml`:

```yaml
services:
  qdrant:
    image: qdrant/qdrant:latest
    restart: unless-stopped
    ports:
      - "6333:6333"   # REST
      - "6334:6334"   # gRPC
    volumes:
      - ./qdrant_storage:/qdrant/storage
```

### Native binary

Download a release from [Qdrant's GitHub
releases](https://github.com/qdrant/qdrant/releases) and run it directly — no
container runtime required:

```bash
tar xzf qdrant-x86_64-unknown-linux-gnu.tar.gz
./qdrant            # listens on http://localhost:6333 by default
```

### Qdrant Cloud (managed)

If you'd rather not run a server, use [Qdrant
Cloud](https://cloud.qdrant.io). You get a `https://…cloud.qdrant.io` URL and an
API key — supply both in the config below.

### Verify it's reachable

```bash
curl http://localhost:6333/healthz      # → "healthz check passed"
```

## Wiring Qdrant into ZeroClaw

There are two independent places Qdrant can be used.

### As the memory backend

Stores all agent memory (and the document corpus) in Qdrant:

```toml
[memory]
backend = "qdrant.main"

[storage.qdrant.main]
url = "http://localhost:6333"      # or set QDRANT_URL
collection = "zeroclaw_memories"   # default if omitted
api_key = "…"                      # Qdrant Cloud / secured instances; or QDRANT_API_KEY
```

The interactive `zeroclaw` onboarding flow also offers Qdrant in its **Memory
backend** picker; it collects these connection details (it does not start a
server).

### As a federated document index

Plug an existing Qdrant collection into a corpus without changing your main
backend — see [Knowledge & document RAG](../tools/knowledge-rag.md):

```toml
[knowledge_bundles.case_law]
sources = ["qdrant+https://KEY@xyz.cloud.qdrant.io/case_law"]
```

The `qdrant://…` (plain HTTP) and `qdrant+https://…` (TLS) schemes accept an
optional `KEY@` prefix for the API key and a `/collection` suffix.

## Environment fallbacks

When a field is omitted, ZeroClaw falls back to these environment variables:

| Field | Env var | Default |
|---|---|---|
| `url` | `QDRANT_URL` | — |
| `collection` | `QDRANT_COLLECTION` | `zeroclaw_memories` |
| `api_key` | `QDRANT_API_KEY` | — |

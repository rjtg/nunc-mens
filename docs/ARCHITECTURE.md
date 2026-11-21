# Architecture and decision log

## Scope

A minimal shell-accessible memory tool, built in Rust, for a human and coding agents. Authoritative data is human-readable Markdown; an optional cached ONNX embedding model, executed in-process via Rust `fastembed`, improves search relevance. **MCP support is deliberately out of scope for MVP.**

## Data flow

1. Discover Git root; capture with `nunc serve` FIFO or `nunc log`/`nunc write` -> atomically create one `.nunc/entries/<id>.md` file.
2. Parse entry files and explicitly registered read-only `--source` paths -> update repo-scoped SQLite.
3. Query vector storage and optional cosine similarity -> fuse rankings -> print plaintext or JSON.

## Format

Proposed entry boundary: a dedicated heading or marker with stable generated ID and metadata, followed by verbatim Markdown body until the next recognized entry marker. The implementer must choose a format where arbitrary user text (including headings, code fences and marker-like strings) cannot accidentally split entries. Prefer explicit length-delimited or escaped boundaries and add roundtrip tests. Journal is readable without the app. Do not use the entry text itself as identity, since identical thoughts may be logged twice.

Individual files: `.nunc/entries/<id>.md`, with stable IDs and timestamps; no automatic editing of old entries. Preserve the original body. Provenance uses relative path + stable entry ID.

## Persistence

- `rusqlite` with bundled SQLite (vector storage) for entry text and metadata.

- Index schema version, model identifier and vector dimensions; reject mismatches.
- Index is cache, not user data: support rebuild from Markdown and removal/reconciliation of deleted entries.
- Use transactions for incremental writes. Full rebuild to a temporary DB and atomic rename on the same filesystem where feasible; preserve the previous valid index on failure.
- Ensure journal append concurrency safety and crash behavior. Avoid shelling out to append arbitrary text.

## Embedding interface

Use the **Rust `fastembed` crate** to load a small multilingual Hugging Face ONNX embedding model directly in the CLI process (ONNX Runtime is a native dependency). No Ollama, embedding server, HTTP inference or daemon. Pin the model ID and revision after verifying support in the chosen crate version. Cache models locally; only the explicit `nunc models download` command may access the network to fetch model artifacts. `log`, `reindex`, `search`, and `doctor` never download. Keep the embedding interface isolated for deterministic tests. Logging and keyword search never need model files. Model loading may incur startup latency; lazy-load only for indexing and semantic/hybrid queries.

## Ranking

Semantic retrieval only: cached fastembed query vector + cosine similarity against compatible vectors loaded from SQLite, linear scan in Rust.

## Explicit non-goals for MVP

- MCP server or mandatory global background daemon (optional repo-local foreground `serve` is supported)
- Hosted/vector SaaS, remote telemetry, Ollama and auto-sync
- QMD dependency or heavy local reranking LLM
- Distributed multiuser journal, cloud backup, automatic AI summarization
- Automatic indexing of arbitrary directories
- Rewriting the user's original notes

## Implementation guidance

Keep dependency and binary footprint modest; check ONNX Runtime distribution, model licensing and cross-platform build support. Ensure no network/model dependency for fast capture, good error messages, and fully isolated unit/integration tests. Document actual latency and footprint only after measuring, not as promises.

## Superseding architecture decision: Git-repository-first FIFO ingestion

**[REPO_FIFO.md](REPO_FIFO.md) is normative and replaces the daily append-only journal and 'no background process' assumptions above.** The server is an **optional foreground, repository-local process**, not a mandatory global daemon.

- Discover Git root from nested paths; `.nunc/entries/<id>.md` stores one memory per completed FIFO frame, with stable ID, timestamp, metadata and original Markdown body. Persist atomically before indexing.
- `nunc serve` creates a named pipe and session metadata with a generated or explicit multipart-style boundary. Only complete delimiter lines terminate messages. FIFO is not durable, and concurrent writers need serialization or socket-based coordination; no implicit persistence acknowledgement.
- `nunc write`/`nunc log` fall back to direct file persistence when the server is absent. Indexing errors never discard saved Markdown; `reindex` recovers.
- Repeatable `--source` flags index explicitly selected **read-only** source files/directories with deterministic chunks and file/line provenance. Watch changes, reconcile deletions, deduplicate overlaps, respect ignore rules and avoid symlink escapes.
- SQLite is a disposable **repo-scoped** index outside Git; `fastembed` uses locally cached ONNX models. Runtime FIFO/session files and indexes must never be committed. Memory entries may be Git-tracked intentionally after privacy review.

## Binding MVP indexing decision

`fastembed` is an embedding generator, **not** a search index. Use a repo-scoped SQLite file at `$XDG_STATE_HOME/nunc-mens/<repo-id>/index.sqlite3` (default `~/.local/state/nunc-mens/<repo-id>/index.sqlite3`), outside Git. Persist IDs, source paths, kind, content, content hashes, model identifier/revision/dimensions and embeddings as `f32` BLOBs. For queries, generate one vector with the cached model, read compatible vectors and compute cosine similarity in Rust, sorting top-k results. Start with linear scan; no SQLite vector extension or separate vector DB. **No FTS5, BM25, keyword or hybrid search in MVP.** If model unavailable, writes persist but indexing remains pending and search errors clearly. Reindex is reproducible from Markdown and registered sources, and must never implicitly download a model.

# Architecture and decision log

## Scope

A minimal shell-accessible memory tool, built in Rust, for a human and coding agents. Authoritative data is human-readable Markdown; an optional cached ONNX embedding model, executed in-process via Rust `fastembed`, improves search relevance. **MCP support is deliberately out of scope for MVP.**

## Data flow

1. Discover Git root; capture with `nunc serve` FIFO or `nunc log`/`nunc write` -> atomically create one `.nunc/entries/<id>.md` file.
2. Parse entry files and explicitly registered read-only `--source` paths -> update repo-scoped SQLite.
3. Query FTS5 and optional cosine similarity -> fuse rankings -> print plaintext or JSON.

## Format

Proposed entry boundary: a dedicated heading or marker with stable generated ID and metadata, followed by verbatim Markdown body until the next recognized entry marker. The implementer must choose a format where arbitrary user text (including headings, code fences and marker-like strings) cannot accidentally split entries. Prefer explicit length-delimited or escaped boundaries and add roundtrip tests. Journal is readable without the app. Do not use the entry text itself as identity, since identical thoughts may be logged twice.

Individual files: `.nunc/entries/<id>.md`, with stable IDs and timestamps; no automatic editing of old entries. Preserve the original body. Provenance uses relative path + stable entry ID.

## Persistence

- `rusqlite` with bundled SQLite (FTS5) for entry text and metadata.
- FTS5 table for keyword retrieval. A standard SQLite table storing encoded `f32` embeddings is sufficient for the MVP (linear scan + cosine in Rust, bounded query volume). Do not add an ANN database initially.
- Index schema version, model identifier and vector dimensions; reject mismatches.
- Index is cache, not user data: support rebuild from Markdown and removal/reconciliation of deleted entries.
- Use transactions for incremental writes. Full rebuild to a temporary DB and atomic rename on the same filesystem where feasible; preserve the previous valid index on failure.
- Ensure journal append concurrency safety and crash behavior. Avoid shelling out to append arbitrary text.

## Embedding interface

Use the **Rust `fastembed` crate** to load a small multilingual Hugging Face ONNX embedding model directly in the CLI process (ONNX Runtime is a native dependency). No Ollama, embedding server, HTTP inference or daemon. Pin the model ID and revision after verifying support in the chosen crate version. Cache models locally; only the explicit `nunc models download` command may access the network to fetch model artifacts. `log`, `reindex`, `search`, and `doctor` never download. Keep the embedding interface isolated for deterministic tests. Logging and keyword search never need model files. Model loading may incur startup latency; lazy-load only for indexing and semantic/hybrid queries.

## Ranking

Keyword = SQLite FTS5 BM25. Semantic = query embedding + cosine similarity over compatible indexed vectors. Hybrid = fuse ranked candidate lists with deterministic reciprocal rank fusion (RRF) or equivalent; prefer exact identifiers and don't overengineer. Each result includes mode, rank/score, stable ID, timestamp and source path.

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

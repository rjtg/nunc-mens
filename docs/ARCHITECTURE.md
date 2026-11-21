# Architecture and decision log

## Scope

A minimal shell-accessible memory tool, built in Rust, for a human and coding agents. Authoritative data is human-readable Markdown; an optional local embedding service improves search relevance. **MCP support is deliberately out of scope for MVP.**

## Data flow

1. Capture with `nunc log` (or shorthand) -> append to local daily Markdown journal.
2. Parse Markdown entries -> normalize metadata, keep verbatim body -> update SQLite.
3. Query FTS5 and optional cosine similarity -> fuse rankings -> print plaintext or JSON.

## Format

Proposed entry boundary: a dedicated heading or marker with stable generated ID and metadata, followed by verbatim Markdown body until the next recognized entry marker. The implementer must choose a format where arbitrary user text (including headings, code fences and marker-like strings) cannot accidentally split entries. Prefer explicit length-delimited or escaped boundaries and add roundtrip tests. Journal is readable without the app. Do not use the entry text itself as identity, since identical thoughts may be logged twice.

Daily files: `YYYY-MM-DD.md`, sorted by timestamp and insertion order; no automatic editing of old entries. Preserve the original body. Provenance uses relative path + stable entry ID.

## Persistence

- `rusqlite` with bundled SQLite (FTS5) for entry text and metadata.
- FTS5 table for keyword retrieval. A standard SQLite table storing encoded `f32` embeddings is sufficient for the MVP (linear scan + cosine in Rust, bounded query volume). Do not add an ANN database initially.
- Index schema version, model identifier and vector dimensions; reject mismatches.
- Index is cache, not user data: support rebuild from Markdown and removal/reconciliation of deleted entries.
- Use transactions for incremental writes. Full rebuild to a temporary DB and atomic rename on the same filesystem where feasible; preserve the previous valid index on failure.
- Ensure journal append concurrency safety and crash behavior. Avoid shelling out to append arbitrary text.

## Embedding interface

Start with HTTP calls to **locally configured Ollama**, using its current `/api/embed` request/response model. Provider should be isolated behind a trait or small module for tests and future replacement. Default local URL `http://127.0.0.1:11434`; never send private text to public services by default. Embeddings are optional for write and keyword operations; failures must be visible.

## Ranking

Keyword = SQLite FTS5 BM25. Semantic = query embedding + cosine similarity over compatible indexed vectors. Hybrid = fuse ranked candidate lists with deterministic reciprocal rank fusion (RRF) or equivalent; prefer exact identifiers and don't overengineer. Each result includes mode, rank/score, stable ID, timestamp and source path.

## Explicit non-goals for MVP

- MCP server or background process
- Hosted/vector SaaS, remote telemetry and auto-sync
- QMD dependency or heavy local reranking LLM
- Distributed multiuser journal, cloud backup, automatic AI summarization
- Automatic indexing of arbitrary directories
- Rewriting the user's original notes

## Implementation guidance

Keep dependency and binary footprint modest; synchronous HTTP is adequate. Ensure no network/model dependency for fast capture, good error messages, and fully isolated unit/integration tests. Document actual latency and footprint only after measuring, not as promises.

# Implementation roadmap

The bootstrap contains the **specification only**. Start implementing without waiting for further product decisions; record sensible deviations in docs.

## M0 — Runnable Rust project

- [ ] Initialize `Cargo.toml`, `src/main.rs`, `Cargo.lock` and minimal modules.
- [ ] Add clap CLI, `--help`, `--version`, meaningful errors; set binary name to `nunc`.
- [ ] Add `.gitignore` and CI workflow for format, clippy, tests.
- [ ] Provide test fixture setup with isolated temporary data directories.

Acceptance: `cargo run -- --help` and `cargo test` work from a clean checkout.

## M1 — Reliable journal

- [ ] `nunc log TEXT...`, `nunc "TEXT..."`, and `nunc log --stdin`.
- [ ] Daily Markdown files with timezone-aware timestamps and persistent unique entry IDs.
- [ ] Optional tags, best-effort Git context, configuration and collision-safe append.
- [ ] Roundtrip tests for Unicode, newlines, fenced code, duplicate text and concurrent writes.
- [ ] **Dogfood:** create a temporary journal and log at least one actual implementation decision with the compiled CLI.

Acceptance: writes are durable, human-readable, lossless and independent of model inference.

## M2 — Searchable local index

- [ ] SQLite entry storage + FTS5 and schema migration/version marker.
- [ ] Parser of journal entry format, incremental `reindex` and `reindex --full`.
- [ ] Handle file edits/deletions, stable IDs, stale-index diagnostics, safe full rebuild.
- [ ] Keyword `search --mode keyword`, `--limit`, and machine-readable `--json`.
- [ ] **Dogfood:** search the M1 decision and verify stable ID and source path.

Acceptance: empty/new journals work; deleted index can be rebuilt; exact tokens and identifiers are searchable without cached model files.

## M3 — Local semantics + hybrid search

- [ ] Integrate Rust `fastembed` with a pinned, verified multilingual ONNX model and deterministic test doubles; document ONNX Runtime platform constraints.
- [ ] Implement explicit `nunc models download` with a configurable local cache. Other commands must never trigger model downloads.
- [ ] Generate/store vectors, record model and dimension, reject mismatches.
- [ ] Cosine-ranked semantic search and hybrid fusion with keyword candidates.
- [ ] Explicit fallbacks and actionable error messages when the model is absent or fails to load.
- [ ] **Dogfood:** search a real development note with paraphrased wording; report whether actual in-process fastembed inference was executed.

Acceptance: semantic/hybrid search works with a cached model; keyword mode remains available without any model files; JSON identifies used mode.

## M4 — Agent-ready release

- [ ] `doctor`, usage documentation and configuration examples.
- [ ] Integration tests for errors, corrupt entries, no backend and JSON stability.
- [ ] CI, lint, release/build instructions, benchmark notes if measured.
- [ ] Test that an agent can log a decision, reindex, find it later, and cite the file and entry ID.

Acceptance: README commands are genuine runnable commands; publish a concise verified test report and remaining limitations.

## Future ideas (not required for MVP)

Optional MCP adapter, fast ANN index if measured scale demands it, configurable alternative embedding providers, contextual chunking for longer notes, and a `nunc sync` alias if it becomes valuable. Do not implement speculatively.
NaN
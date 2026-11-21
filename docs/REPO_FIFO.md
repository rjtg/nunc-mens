# Repository-local runtime, FIFO protocol, and source indexing

**Status:** target design; not implemented. This document supersedes the earlier global daily-journal-first layout. The implementation issue and roadmap refer here.

## Repo-first lifecycle

Run `nunc serve` from any directory inside a Git repository. Discover the root with `git rev-parse --show-toplevel`; keep runtime and generated entries under that root, not under the current subdirectory. `nunc init` can create the optional checked-in `.nunc/config.toml` and storage directories. For non-Git directories, use the working directory as an explicit standalone root (documented, without walking into unrelated parents). Do not index arbitrary paths by default.

```text
my-repo/
  .git/
  AGENTS.md
  src/
  docs/
  .nunc/
    config.toml           # optional, shareable
    entries/
      <unique-id>.md      # one immutable-ish entry per message
    input.fifo            # runtime only; never tracked
    session.json          # runtime only; never tracked
```

Index DB: XDG state directory keyed by canonical repository identity (avoid collisions for repositories with identical basenames). Embedding model cache: XDG cache directory. Neither belongs in Git. `.nunc/entries/` is **eligible** for version control but not automatically committed; document secret/privacy implications and offer an explicit ignore/private option. `.nunc/input.fifo` and `.nunc/session.json` must be gitignored; never commit runtime artifacts. The runtime should remove stale session artifacts safely, verify ownership and reject symlink/path traversal attacks.

## Start server and sources

```sh
nunc serve --source ./src --source ./docs --source ./AGENTS.md
nunc serve --boundary 'my-session-token' --source ./docs
nunc status
```

`--source` is repeatable and accepts repo-relative files or directories; canonicalize, validate scope, avoid symlink escapes and deduplicate overlapping roots. Sources are **read-only**; only `.nunc/entries/` receives newly ingested memory. For directories, recursively enumerate supported text/Markdown files with documented limits and Git-ignore-aware filtering; do not index binaries, `.git`, build directories, the index or runtime artifacts. Use a file watcher with initial scan, debounced incremental updates and deletion reconciliation; offer a periodic/manual `reindex` fallback. Chunk source documents deterministically with source path and line ranges; memory entries remain individual documents. Search must expose `source` and `kind=memory|source` and support `--type` filters. Unconfigured sources must not be crawled.

## Session-specific boundary protocol

At startup generate a cryptographically random, sufficiently long boundary (e.g. 128+ random bits encoded as ASCII), or accept `--boundary VALUE` after validation (nonempty, bounded, safe ASCII without CR/LF). Print the exact FIFO path and boundary to stdout and atomically publish a permission-restricted `.nunc/session.json` for clients. The boundary is a **delimiter, not authentication**.

A record ends when a **complete line** exactly equals `--<boundary>` followed by LF (accept CRLF consistently if documented). The delimiter line is excluded from the entry body; blank lines and Markdown code fences are otherwise preserved byte-for-byte. A partial delimiter is not a delimiter. Buffer until a complete delimiter arrives; allow multiple records in a stream and delimiters split across reads. Reject empty records or oversized messages with bounded memory and actionable errors. A line equal to the delimiter inside the body collides; document this and test it. Random boundaries make accidental collisions very unlikely, not impossible. EOF without delimiter is an incomplete record, **not** an implicit commit; define discard/recovery behavior. This is a simple line-delimited multipart-inspired framing protocol, not full HTTP multipart MIME.

```sh
# Example only: actual boundary is printed at runtime.
nunc serve --boundary 'demo-123' --source ./docs
cat > .nunc/input.fifo <<'EOF'
Cache invalidation uses versioned keys.

This is a second paragraph.

--demo-123
EOF
```

On completion, allocate a collision-resistant stable ID, timestamp with offset and repository/source metadata, write a **separate Markdown file** (e.g. YAML front matter + original body) via temporary file + atomic rename, then enqueue or perform immediate cached-fastembed vector indexing in SQLite. The original body must not be rewritten or summarized. Indexing failure must not lose the committed Markdown entry; report pending/error state and permit `reindex`.

## FIFO concurrency, delivery and fallback

A FIFO is a byte stream, **not** a durable queue or transactional IPC. A writer opening it without a reader can block; successful write does not prove persistence or indexing. Large concurrent writes can interleave (atomicity only up to OS `PIPE_BUF` per write). Do not promise multiwriter safety from boundaries alone. Provide `nunc write TEXT...` / `nunc write --stdin` as a client that reads the current session metadata, frames input correctly and serializes writers using a robust cross-process lock or a Unix-domain socket where supported. Document what guarantees can be made for raw FIFO writers. Use restrictive directory/FIFO permissions and avoid exposing private entries to other local users.

When `serve` is absent, `nunc write` and `nunc log` must **not block indefinitely**: atomically write the entry file directly and leave indexing for `reindex` or the next server startup. Prefer an explicit acknowledgement protocol for clients that need durable/indexed confirmation; FIFO writes alone cannot supply it. On restart generate a new default boundary and replace session metadata safely. Only one server may own a repo runtime at a time.

## Search, model and Git behavior

`nunc search QUERY` searches the current repository's SQLite-stored vectors generated by cached `fastembed`. `--source` filters registered source paths/names, `--type memory|source` filters document kind; `--json` remains supported; semantic-only search is the MVP. Include repo identity, source path, kind, entry ID or chunk ID, and line range for source chunks in results. `nunc reindex` reconciles entries and registered source files, including deletions. The watcher may keep search fresh during `serve`; without server, `reindex` remains usable. No Ollama, mandatory daemon, external inference API or surprise model download. `nunc models download` explicitly prepares a compatible multilingual ONNX model for in-process Rust `fastembed`.

## Required tests

- Git root discovery from nested working directories; standalone mode and multiple independent repositories.
- Random and user-specified boundary, split reads, multiple records, CRLF behavior, Markdown blank lines/code fences, collisions, oversized/empty/incomplete messages.
- FIFO absence, no reader, server restart/stale metadata, simultaneous writers and cross-process locking, shutdown and recovery.
- One entry per file, atomic write, metadata/body roundtrip, crash between write and index, reindex recovery.
- Multiple `--source` flags, overlapping roots, ignored/binary files, source changes/deletions, path escapes, chunk provenance.
- Capture without model cache; clear search/indexing errors when absent; fastembed semantic ranking when cached; JSON provenance.
- Agent dogfooding: run `nunc serve` in an isolated fixture Git repo, send a real note through FIFO or `nunc write`, verify saved file and search result. Record actual commands and results.

## Binding MVP semantic-only search and index

The **search index is a repository-scoped SQLite file outside Git**: `$XDG_STATE_HOME/nunc-mens/<repo-id>/index.sqlite3` (default `~/.local/state/nunc-mens/<repo-id>/index.sqlite3`). `fastembed` only generates vectors; SQLite persists them as `f32` BLOBs with content, IDs, hashes, paths, kind, source line ranges and model metadata. Search embeds the query with the cached model and computes cosine similarity over the stored vectors in Rust (linear scan), returning top-k with provenance. **No FTS5, BM25, keyword fallback, hybrid mode, ANN index or external vector DB in MVP.** If no model is cached, FIFO/CLI capture still persists Markdown, indexing is pending, and search reports a clear error. `nunc reindex` catches up after explicit `nunc models download`; never download implicitly.

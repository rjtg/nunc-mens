# CLI contract (target for implementation)

**Status:** specification, not implemented yet. Executable: `nunc`.

## Capture

```sh
nunc log "Investigated auth failure; clock skew was the cause"
nunc "Quick thought: cache invalidation can wait"
printf 'Multiline note\nwith context\n' | nunc log --stdin
nunc log --tag auth --tag staging --repo myservice "Fixed deploy failure"
```

- `nunc log TEXT...` appends one timestamped entry. Argument text joined with spaces; don't infer formatting or execute it.
- `nunc TEXT...` is an exact shorthand for `nunc log TEXT...` when no recognized subcommand is supplied; use robust clap dispatch and document ambiguity.
- `nunc log --stdin` reads one complete entry until EOF, preserving newline structure; reject empty/whitespace-only entries.
- `--tag` repeatable, `--repo` optional explicit context; otherwise best-effort detect `git rev-parse --show-toplevel` from current directory. Never fail just because no Git repo is found.
- Use local ISO-8601 / RFC3339 timestamp with timezone offset. File naming: `YYYY-MM-DD.md` based on local date. Store stable entry ID, timestamp, repo/tags (if supplied) and original text in a documented, parseable Markdown structure.
- Preserve line breaks and code fences. Handle Unicode and special characters without quoting errors.
- Log writes must not call the embedding model. On success, show ID and path; nonzero exit on failure.

## Index

```sh
nunc reindex
nunc reindex --full
nunc doctor
nunc models download
```

- Index all supported entries in the configured journal directory, and **only** there. `reindex` incrementally refreshes changed and removed entries when possible; `--full` recreates the SQLite search index from source and discards stale data.
- FTS5 is always available for indexed entries. Generate embeddings in-process with `fastembed` only when the configured model is already cached. If it is missing or cannot load, still build the keyword index and report counts and the reason. Never download during indexing.
- Persist embedding provider/model/dimensions/version. When those change, invalidate/rebuild affected embeddings; never compare vectors with incompatible dimensions.
- No silent stale results: writes either trigger freshness validation on search or show a clear 'index needs refresh' warning. For MVP, explicit reindex with clear stale warning is acceptable.
- `doctor` reports effective paths, index state, cached model availability, configured embedding model and missing prerequisites **without displaying journal text**.

## Retrieval

```sh
nunc search "staging authentication issue"
nunc search --mode keyword --limit 10 "clock skew"
nunc search --mode semantic "deployment login broke"
nunc search --mode hybrid --json "CI failures"
```

- Keyword: SQLite FTS5/BM25; match exact tokens and identifiers as reliably as possible (define tokenizer behavior/tests).
- Semantic: embed query using the same provider/model as indexed entries, then cosine similarity over stored `f32` vectors. Fail clearly if the cached model/index is unavailable; do not mislabel keyword-only hits as semantic.
- Hybrid: merge FTS5 and vector candidates deterministically (e.g., reciprocal rank fusion) with exact-match protection. Default to hybrid if available; otherwise transparently fall back to keyword with a warning to stderr.
- Return `--limit` best-ranked results (default 5), each with ID, timestamp, file path and verbatim text. Provide an explicit score/rank and search mode in JSON. Avoid returning arbitrary file contents.
- `--json` returns a single valid JSON object, including results, `mode_requested`, `mode_used`, and warnings; don't emit diagnostic lines on stdout. Human formatting may be more compact.
- Stable exit codes: 0 success including no results; 2 usage; nonzero for IO/index errors or explicit semantic-backend failures. Document exit-code conventions as implemented.

## Configuration (initial)

```sh
export NUNC_JOURNAL_DIR="$HOME/.local/share/nunc-mens/journal"
export NUNC_INDEX_PATH="$HOME/.local/state/nunc-mens/index.sqlite3"
export NUNC_EMBED_MODEL="<supported-multilingual-fastembed-model-id>"
export NUNC_MODEL_CACHE_DIR="$HOME/.cache/nunc-mens/models"
```

Defaults are proposals; implement OS/XDG-aware path handling with tests. Embeddings run in-process using the Rust `fastembed` crate and a supported multilingual Hugging Face ONNX model. Choose and document an actual model ID supported by the pinned crate version during implementation; do not treat the placeholder above as a real model ID. `nunc models download` is the only command allowed to fetch model files; all other commands must be offline and must use the cache. `log` and keyword search must work without model files. No Ollama or HTTP inference service.

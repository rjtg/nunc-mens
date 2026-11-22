# Architecture

```text
CLI → Repo + Config
    ├→ log or offline write → Markdown entry
    ├→ serve → boundary parser → Markdown entry
    ├→ reindex or watcher → snapshot of Markdown and sources → FastEmbed → SQLite
    └→ search → FastEmbed query → SQLite scan → cosine-ranked results
```

## Ownership and reuse

- `repo` owns discovery, scope, validated source paths and configuration. `fs` handles bounded reads, private directories, locks and atomic writes.
- `entries` is the shared authoritative capture service. Its fixed, versioned front matter starts with `---`, contains one JSON object (valid YAML) and closes with `---`. The remainder is original UTF-8 input. Metadata holds a UUID v4, an offset-aware RFC3339 timestamp, optional repository label and tags. Filename and UUID must agree. Same-directory temporary files are synced and published without clobbering another note, followed by a directory sync.
- `protocol` parses a bounded stream independently of Linux or macOS I/O. `fifo` owns session metadata, reader, cooperative writer locks and lifecycle. `watch` refreshes on a separate thread and keeps the ONNX session warm. Every committed Markdown entry is available for retry after an indexing failure or restart.
- `documents` builds snapshots of authoritative memories and explicitly selected source files. Source opens use component-wise `openat` with symlink following disabled. `chunk` uses CommonMark block offsets and the local model tokenizer. Token budgets preserve fitting blocks such as lists and fences, and split an oversized block on valid UTF-8 boundaries. Optional semantic mode embeds adjacent units and creates boundaries after topic shifts.
- Stable chunk IDs combine the parent identity, raw chunk SHA-256 and the occurrence of that exact text within the parent. Changes elsewhere can preserve IDs even when line offsets shift. The index stores parents once and uses the same raw content digest for vector reuse. The parent hash and layout settings identify reusable Markdown layouts.
- `model` handles production inference and model downloads. `vectors` describes embedding compatibility and the binary representation. The index accepts an embedder interface so SQLite and failure tests can use fixed vectors.
- `index` serializes writers and protects database readers against same-directory replacement with a shared lock. `search` filters candidates by provenance, computes cosine in Rust with f64 accumulation, and retains the best K. Storage, capture, and indexing failures retain their own error context.
- The optional search context budget operates after ranking and visibility checks. It loads parents through the same index read guard, reserves matched blocks across distinct sources and uses the remaining budget to expand contiguous excerpts. The existing chunk metadata identifies the match and a separate window identifies the actual returned text. No index migration is required.
- Markdown bundles use the same ranked candidates and window allocator. The budget counter runs the same section writer used for stdout, so it includes the full attribution and literal excerpt framing.
- Temporal filters compare original event instants or local calendar dates before ranking. Timelines and date-only search read durable entries with the same ledger visibility rules. Temporal neighbors run under that shared guard and report time deltas separately from retrieval scores. The index and original Markdown need no migration.
- Topic views reuse the timeline selection and the current parent hashes to check indexed passage vectors. A complete-link cutoff prevents groups from merging through a chain of weak links. Groups and labels are derived from the selected records on each invocation.
- Search by example and topic grouping share indexed passage pooling. Search resolves a durable reference and verifies its body hash, then averages up to 64 stored passage vectors. It excludes the reference parent and uses the same ranking, filters and context formats as text search.
- Search explanation counts are collected during the existing scan, when requested. The source-scope counters observe SQLite rows after type selection. Ranking counters track visibility, dates and scored parents. The report separates top-K truncation from post-ranking budget omissions.

## Persistent search transport

The FIFO continues to accept writes. Search needs bidirectional framing for statuses and output, so the same foreground server can also own a Unix domain socket. The FIFO ownership lock guards socket publication and stale endpoint cleanup. The socket is mode 0600 inside an owner-only runtime directory. A listener admits one worker and at most eight queued clients. Requests reuse the normal CLI parser and formatter, with the action restricted to search.

Protocol version 1 uses one connection per request. The worker sends byte 2 when ready, or byte 1 followed by a framed UTF-8 error when busy. The client sends a big-endian `u32` length and up to 1 MiB of JSON containing version 1 and the command's Unix byte arguments as arrays of bytes, including the executable and `search`. It keeps both directions open. Byte 0 marks success, with framed stdout and stderr payloads using the same length encoding. Byte 1 marks a framed error. Stdout is capped at 8 MiB and diagnostics at 64 KiB. The client receives both frames before writing output locally. The protocol serves the same CLI version and can reject unfamiliar request versions.

The watcher and the search worker share one mutex-protected ONNX session, keyed by model artifact identity and timestamps. The session holds up to 64 exact query-text and vector pairs in least-recently-used order. Queries longer than 4 KiB bypass the cache. The session fixes model identity, tokenizer, pooling and query normalization. Replacing or invalidating the session drops its vectors. A lookup releases the model mutex before acquiring ledger and index read locks. The sole search worker retains the read-only SQLite connection between invocations. It reacquires the normal locks and validates the current index headers every request. A rebuilt file with a different device or inode replaces the cached connection. Every search ranks current eligible rows and evaluates the current archive and forget ledger. Cancellation checkpoints cover scan batches and major stage boundaries. Native inference, SQLite calls and a started filesystem snapshot can run to their next boundary.

## SQLite and recovery

Schema version 2 adds a parent table and replaces single-memory rows with chunk rows. The metadata table records repository identity, model revision and dimensions, strategy and snapshot digest. Chunk rows reference parents and hold byte and line provenance, hashes and 384 little-endian IEEE-754 f32 values. A version 1 database is rebuilt automatically by `reindex` from durable Markdown and sources. Model incompatibility requires a full rebuild. No migration rewrites entry files.

An incremental refresh reuses unchanged embeddings, updates metadata, and deletes absent documents in one rollback-journal transaction. A full rebuild stages a separate file and publishes it after successful indexing and sync. Index schema or model changes require a full rebuild. Corrupt source Markdown stops a refresh and preserves the previous index. Correct or move the indicated entry, then retry. If a corrupt index also has an unrecoverable rollback journal, move both files aside before rebuilding.

Normal and keyword-only refreshes create an FTS5 external-content table over the shared chunk rows. The additive migration backfills terms in one transaction without regenerating existing vectors. SQLite triggers reconcile terms when chunk rows are inserted, edited, or removed. Keyword scoring uses native BM25 and literal text or provenance matches before the shared visibility, ranking and context path. A keyword-only refresh can construct new structural layouts with a UTF-8 byte budget. Those layouts have their own versioned settings key. If the resulting snapshot needs embeddings or canonical layouts, a readiness flag prevents incomplete semantic retrieval until a regular refresh.

Hybrid search retains bounded cosine and FTS5 lists under the same read lock and durable visibility view. It combines the lists by chunk ID using normalized reciprocal rank fusion, then applies the optional recency adjustment, sort, deduplication and budget. All three modes load full provenance and any requested context only for their final ranked candidates.

## Embeddings

FastEmbed 5.15.0 runs `intfloat/multilingual-e5-small` at revision `614241f622f53c4eeff9890bdc4f31cfecc418b3` with CPU ONNX Runtime 1.24, mean pooling, and the model's `query: ` and `passage: ` prefixes. The explicit HTTPS downloader verifies artifact sizes and the weights/tokenizer checksums, probes inference, and atomically publishes the revision directory. Inference initializes from local files and sends no project text over the network.

Freshness checks and polling reread eligible files. Search scans selected vectors linearly. Duplicate discovery groups normalized hashes and records pairwise evidence for semantic candidates. Cleanup suggestions compare current Markdown bytes under the shared ledger lock and protect consolidation lineage. Semantic scanning and duplicate review suit modest corpora. Long memories retain their full bodies in durable Markdown and are indexed as multiple model-sized passages.

## Durable operations

`git` shares parsing and entry publication between ingest, import, sync and the optional hook. Commit memories use a deterministic UUID derived from the worktree identity and full Git commit hash. Git traversal streams one hash at a time, and bounded process readers limit individual metadata and diffs.

`ledger` owns manual and consolidation archive associations, undo state and content-free tombstones in `.nunc/ledger.json`. Worktree mutations take the memory lock before acquiring the SQLite lock. Searches consult the ledger under a shared memory lock before returning indexed data. The disposable index never decides visibility.

- Apply publishes the new Markdown summary first, with a summary kind that stays invisible until a single atomic ledger replacement commits its associations. If publication is interrupted, originals stay searchable. The same body and source revisions derive the same operation ID, so a retry can reuse a staged summary. Competing applications serialize and can neither claim an already archived source nor form a cycle.
- Manual archive publishes one ledger update for all selected IDs. Search and duplicate discovery share ledger visibility, and the rebuildable index retains archived rows for raw retrieval.
- Undo toggles the durable association and leaves the summary raw-only. Nested associations retain history. A forgotten source remains an ID marker with no retained body.
- Forget commits all tombstones together before removing any files or index rows. If interrupted, search and import consult the tombstone immediately. Retry forget or run reindex to remove pending files and rows. Physical erasure and copies outside the selected entries are outside this guarantee.
- Reindex refreshes SQLite from surviving Markdown and configured sources. Archive status comes from the ledger in both normal and raw search. Back up the ledger with entry Markdown to preserve state.

## Index health

The deep audit acquires existing shared locks without creating runtime files. It derives expected chunk IDs and content from the same layout and identity code used by reindex. Ledger validity is checked independently of SQLite integrity. Repairs take locks in the same order as reindex and rebuild through the same staged database publication path. Offline repair removes known bad derived rows transactionally and leaves missing vectors pending. Authoritative files remain untouched, and visibility continues to come from the ledger in every search.

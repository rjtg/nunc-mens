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
- `documents` builds deterministic snapshots of authoritative memories and configured source files. Source opens are anchored at the worktree via component-wise `openat` with symlink following disabled. Source chunks fit a 1,200-byte budget, preserve UTF-8 and carry inclusive line ranges. An exceptionally long line may span chunks sharing its line number. Source IDs hash the chunker version, relative path, and chunk ordinal. Stable paths and ordinals preserve IDs across rebuilds. Editing earlier lines can change chunk boundaries and IDs' associated text. SHA-256 of each chunk's content identifies reusable embeddings, and the snapshot digest covers source scope, provenance and content.
- `model` is the only production embedder and the only downloader. `vectors` describes embedding compatibility and the binary representation. The index accepts an embedder interface so SQLite and failure tests can use fixed vectors.
- `index` serializes writers and protects database readers against same-directory replacement with a shared lock. `search` filters candidates by provenance, computes cosine in Rust with f64 accumulation, and retains the best K. Storage, capture, and indexing failures retain their own error context.

## SQLite and recovery

Schema version 1 is stored in SQLite `user_version`. A single metadata row records the repository ID, model ID and revision, dimensions and snapshot digest. Document rows contain ID, kind, path, optional timestamp or line range, original text, content hash and vector. The vector is exactly 384 little-endian IEEE-754 f32 values for the pinned model. Ingestion and ranking reject invalid dimensions and non-finite or zero-norm embeddings.

An incremental refresh reuses unchanged embeddings, updates metadata, and deletes absent documents in one rollback-journal transaction. A full rebuild stages a separate file and publishes it after successful indexing and sync. Index schema or model changes require a full rebuild. Corrupt source Markdown stops a refresh and preserves the previous index. Correct or move the indicated entry, then retry. If a corrupt index also has an unrecoverable rollback journal, move both files aside before rebuilding.

## Embeddings

FastEmbed 5.15.0 runs `intfloat/multilingual-e5-small` at revision `614241f622f53c4eeff9890bdc4f31cfecc418b3` with CPU ONNX Runtime 1.24, mean pooling, and the model's `query: ` and `passage: ` prefixes. The explicit HTTPS downloader verifies artifact sizes and the weights/tokenizer checksums, probes inference, and atomically publishes the revision directory. Inference initializes from local files and sends no project text over the network.

Freshness checks and polling reread eligible files. Search ranks a full linear scan of the selected vectors. These choices suit modest project corpora. Long memory entries retain their entire body, and inference truncates inputs to the model's 512-token context. The index caches project text as well as vectors.

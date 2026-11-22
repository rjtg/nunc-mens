# CLI reference

| Command | Behavior |
| --- | --- |
| `nunc init [--source PATH]...` | Creates local storage and initializes config. |
| `nunc log [--tag TAG]... [--repo LABEL] TEXT...` | Saves a durable Markdown file and prints its ID and path. |
| `nunc log [--tag TAG]... [--repo LABEL] --stdin` | Reads a note from redirected stdin through EOF and saves it directly. |
| `nunc TEXT...` | Shorthand for `log` when the first argument is ordinary text and does not name a command. |
| `nunc write TEXT...` or `nunc write --stdin` | Queues capture via a live FIFO or saves directly when the server is unavailable. |
| `nunc serve [--boundary TOKEN] [--source PATH]...` | Runs the foreground FIFO and refreshes the index from local files. |
| `nunc duplicates [ID] [--threshold N] [--limit N] [--exact] [--raw] [--json\|--forget-ids]` | Reviews duplicate groups or emits verified cleanup candidates. |
| `nunc diff OLD NEW` | Compares the original Markdown bodies of two selected memories. |
| `nunc git ingest\|import\|sync\|hook` | Captures selected commit history or manages the post-commit hook. |
| `nunc consolidate candidates\|apply\|undo` | Retrieves full candidate entries, commits an external summary, or undoes an archive. |
| `nunc archive ID... [--dry-run] [--json]` | Hides selected memories and keeps their Markdown. |
| `nunc unarchive ID... [--dry-run] [--json]` | Restores manually archived memories. |
| `nunc forget ID... [--dry-run\|--yes] [--json]` | Deletes only the specified entries after warning about dependencies. |
| `nunc reindex [--full] [--source PATH]...` | Reconciles changes or publishes a separately staged full rebuild. |
| `nunc search QUERY... [--limit N] [--max-tokens N] [--json\|--ids\|--format markdown] [--type memory\|source] [--source PATH]... [--source-file FILE]...` | Ranks compatible indexed vectors by cosine similarity and optionally caps context. |
| `nunc models download` | Explicitly downloads and validates the pinned model and tokenizer. |
| `nunc status` | Shows repository, server, sources, cache, and index paths. |
| `nunc doctor` | Also compares index freshness against the current repository snapshot. |

Positional capture and query words are joined with spaces. Use `nunc log -- TEXT` for text beginning with a hyphen or matching a subcommand. For exact multiline capture use `--stdin` with piped input. Notes must be nonblank UTF-8, contain no NUL and fit within 256 KiB. `--repo` sets a metadata label, with the Git worktree basename as the default when available.

## Scope and configuration

The Git worktree root is discovered from the invocation directory. Outside Git, the current directory is the standalone root. `NUNC_REPO_ROOT` is an optional explicit root directory.

`init`, `serve`, and `reindex` accept repeatable literal repository-relative source paths. A supplied list replaces the registered list in `.nunc/config.toml`. Without flags these commands retain the current list. To clear it, save `sources = []` in that config. Source registration on `reindex` persists before inference, so a missing model can leave a new source list pending. Sources are scanned as read-only text. See [source selection](REPO_FIFO.md#source-selection).

```toml
sources = ["docs", "src", "AGENTS.md"]
```

| Setting | Default |
| --- | --- |
| `NUNC_STATE_DIR` | `$XDG_STATE_HOME/nunc-mens` or `~/.local/state/nunc-mens` |
| `NUNC_MODEL_CACHE_DIR` | `$XDG_CACHE_HOME/nunc-mens/models` or `~/.cache/nunc-mens/models` |

Use absolute overrides outside the worktree. The index is `<state>/<repo-id>/index.sqlite3`. A clone, linked worktree, or moved worktree uses its own index. The model cache is shared across worktrees using the same cache root. Only the download command fetches model artifacts. A missing model leaves capture usable and makes search or reindex return an actionable error. After model setup, run `nunc reindex` or start the watcher with `nunc serve`.

## Search output

Default limit is 5, with a range of 1–1000. `--type` selects memories or sources. An empty result set is successful.

### Ranked IDs for shell pipelines

```sh
nunc search "SQLite locks" --ids --type memory
nunc search "SQLite locks" --ids --type memory | xargs -r nunc archive --dry-run
```

`--ids` emits one parent ID per line in rank order, with each parent's best-ranked eligible chunk determining its position. The limit counts unique parents. Captured and Git-derived memories both use UUIDs. Project sources use stable `source:<hash>` parent IDs. Use `--type memory` when passing IDs to a command that accepts only memory UUIDs. Existing source, sort, recency, and visibility rules apply. `--json` and `--explain` conflict with `--ids`. Empty searches succeed with empty stdout. Errors and index-freshness warnings go to stderr.

### Search selected project documents

Register a documentation folder with `nunc init --source docs` or `nunc reindex --source docs`. Organize architecture decisions, specifications, RFCs, plans, incidents and research as ordinary files under that folder.

```sh
nunc search "SQLite concurrency" --source docs/decisions/ --source docs/design/
nunc search "SQLite concurrency" --source docs/incidents/ --source-file docs/rfcs/sqlite.md
nunc search "SQLite concurrency" --source .nunc/entries/
```

Paths are relative to the worktree root, even from a subdirectory. A directory passed to `--source` matches recursively by path components. An existing file passed to `--source` or `--source-file` matches exactly. Repeated directory and file filters form a union applied before ranking and limiting. Use `--source .nunc/entries/` to include memories alongside selected project documents. `--type memory` still works on its own.

Choose paths inside a registered source. Absolute paths, parent traversal, symlinks, special files and directories passed to `--source-file` are rejected. An existing empty directory yields no results. A deleted path remains usable while matching rows exist in the stale index, with a freshness warning. Once reindex removes those rows, that missing path produces an error.

- `--context chunk` returns the matched slice and is the default. `section` returns its enclosing Markdown section up to the next heading. `parent` returns the original full note body or source file. For plain text, section context is the full file. These options change the returned text while ranking the same chunks.
- `--sort semantic` orders by the final similarity score. `--sort recent` uses the original timestamp first, then score, with undated sources last. Exact ties use the stable chunk ID.
- `--recency-boost` enables a maximum contribution of 0.05. Specify `--recency-boost=0.1` for a different maximum, with 0 disabling it and 0.2 as the limit. The contribution is `maximum × 2^(-age_days / half_life_days)`, with a default half-life of 90 days. Future timestamps are treated as age zero. Missing timestamps contribute zero. The final score is cosine plus the contribution and can exceed 1.
- `--explain` exposes the matched text, provenance, model ID/revision/dimensions, chunk strategy, context, sort, cosine, recency contribution and final score. The CLI takes one reference time per search.
- `--raw` includes archived and undone memories. Forgotten entries are excluded in every context.

`--json` returns `mode_used`, `repo_id`, `warnings` and `results`. The original hit fields are retained. `matched_chunk` adds its ID, parent ID, chunk and parent SHA-256, heading hierarchy, one-based inclusive lines and zero-based half-open byte range. Memory locations are relative to the body after front matter. Memory rows keep their parent UUID as `id`. Git results include original commit provenance under `git` and `entry_kind: "git-commit"`. Memory rows expose status, optional archive target and originating source IDs. Search reports stale or unverifiable snapshots in `warnings`.

### Context budget

```sh
nunc search "FIFO concurrency" --context parent --max-tokens 2000 --json
```

Use a positive `--max-tokens` with chunk, section, or parent context. Search considers the highest-scoring chunk per parent, up to `--limit`. It reserves a matched excerpt for ranked parents before expanding nearby context with the remaining budget. A result that cannot fit can leave a gap in the original ranks. Type and source filters, archive visibility, and `--sort` still apply. `--ids` conflicts with `--max-tokens`.

JSON and ordinary console output use the sum of `ceil((UTF-8 bytes in the compact JSON result + 1) / 4)` for each returned result. This includes text, provenance, window fields, and any explanation. The extra byte allows for a result separator. It excludes the response envelope, warnings, and console presentation. The estimate will differ from the count reported by an LLM tokenizer.

A budgeted JSON response adds a `budget` object with `max_tokens`, `estimated_tokens`, `truncated_results`, `omitted_candidates`, and the estimate identifier. The omitted count refers to candidates considered after ranking and the unique-parent limit. Each result adds a `context_window` with the returned text's original byte and line ranges and a `truncated` flag. `matched_chunk` continues to describe the original match. With `--explain`, the matched text is the intersection of the original chunk and returned window. Text mode labels shortened results and reports aggregate budget usage on stderr.

Markdown windows use complete top-level blocks and may extend a partial chunk to include its enclosing block. The budget still applies. A Markdown block that cannot fit is skipped whole, including oversized fenced code or very long paragraphs. Other sources use complete lines. When provenance and a matched excerpt cannot fit for any candidate, search succeeds with empty results and reports the omission count.

### Markdown context bundle

```sh
nunc search "SQLite concurrency" --format markdown --context section --max-tokens 2000 > context.md
```

The bundle uses one section per ranked parent and accepts the same context, visibility, and source filters as ordinary search. A section gives the path, parent and matched chunk IDs, matched lines, heading path, and available timestamp or Git origin. Retrieved text is a literal fenced excerpt. Its outer fence is sized to preserve any fences and headings from the original. Budgeted sections include returned line ranges and label shortened context.

This format estimates usage as the sum of `ceil(UTF-8 bytes in the rendered section / 4)`. It charges every byte on stdout, including result headings, metadata, delimiters, separators, and requested explanation. Budget totals and warnings go to stderr. If no result fits, stdout is empty. `--format markdown` conflicts with `--json` and `--ids`.

### Configuration

```toml
sources = ["docs", "src"]

[chunking]
strategy = "structural" # or "semantic"
tokens = 384             # 32–480; leaves room for model framing
semantic_threshold = 0.80

[search]
recency_boost = 0.0
half_life_days = 90.0

[git]
message_bytes = 16384
max_paths = 64
diff_context_bytes = 0
```

Both strategies preserve CommonMark heading paths and group blocks within a section. Semantic splitting checks cosine between adjacent blocks and adds a boundary below the configured threshold. Use `nunc reindex` after changing chunk settings.

### Duplicate discovery

`nunc duplicates [ID] --json` groups exact matches by whitespace-normalized, case-sensitive SHA-256. Each group includes its stable ID, a reference entry, individual members and evidence, and suggested cleanup IDs. The reference is the oldest timestamped member by absolute time, with the ID breaking ties and untimestamped sources sorted last. The original `matches` array remains available for pairwise consumers. Passing an ID selects its exact group and restricts semantic pairs to that parent. An explicit ID serves as the semantic reference for those pairs.

With the default 0.90 threshold, indexed passage comparisons can find paraphrases and similar passages. Each semantic member has direct evidence against its reference, with both matched chunk locations where applicable. A parent can appear in more than one group because two matches through a shared parent do not establish a third match. High similarity can flag conflicting revisions. Semantic matches are for review. `--limit` caps both group count and the retained pair list. `--exact` works without an index or model. Default discovery omits archived and undone entries. `--raw` includes them for review. Forgotten entries are always omitted.

```sh
nunc duplicates --exact --forget-ids | xargs -r nunc forget --dry-run
```

Cleanup candidates require an active ordinary journal entry with a body byte-identical to the active ordinary reference. References, source files, Git records, archived entries, and entries referenced by a consolidation are excluded from suggestions. The IDs output requires `--exact` and emits only UUIDs, one per line, with no output when none qualify. Read the preview before issuing an explicit forget. Even byte-identical bodies can have different capture metadata, and forgetting still uses its own dependency checks and confirmation.

`nunc write --check-duplicates [--threshold N]` reports candidates on stderr before normal capture. Capture continues if the model or index is unavailable. Neither duplicate command suppresses entries. Semantic all-pairs comparisons scale quadratically with indexed chunks.

## Git history

```sh
nunc git ingest HEAD
nunc git import --all --dry-run
nunc git import --limit 100 --since 2026-01-01
nunc git import --range HEAD~50..HEAD
nunc git sync
nunc git hook install
nunc git hook status
nunc git hook uninstall
```

Default import and sync walk current HEAD ancestry in Git's descendants-first topological order. `--all` considers commits reachable from all refs. `--range A..B` takes commits reachable from B and excludes those reachable from A. Range and all are mutually exclusive. Date (UTC midnight, Git committer date) and positive limit can narrow any scope. Merges produce their own memory and use first-parent changes. Sync walks HEAD but opens commit details only for unseen IDs. Rewritten commits with new hashes are new memories. Existing Markdown from earlier commits remains available. Interrupted imports can be rerun and dry runs create no durable files.

The optional managed post-commit hook invokes the installed CLI path and leaves Git commit success independent of ingestion. An existing or shared hook is preserved, with instructions to chain `nunc git ingest HEAD` explicitly. The command honors worktree-local `core.hooksPath`. Entries include full SHA, author, both dates, bounded messages and filtered changed paths/statistics. Binary bodies and patch hunks are omitted by default. Enabling `git.diff_context_bytes` adds bounded, heuristically redacted text for included paths. Inspect imported content before sharing it. Capture works without a model or server. Cached inference reconciles after runs selecting up to 256 commits. For larger runs, use the watcher or run `nunc reindex` after capture.

## Reversible archive

```sh
nunc archive "$first" "$second" --dry-run --json
nunc archive "$first" "$second"
nunc search --raw "FIFO concurrency"
nunc unarchive "$first"
```

Archive and unarchive validate all specified IDs before publishing one atomic ledger update. Repeating an operation reports unchanged IDs. Dry runs leave Markdown, the ledger, and SQLite unchanged. Changes take effect for subsequent retrieval without inference or reindex. Raw search labels manually archived memories with `archive_reason: "manual"`. A forgotten memory cannot be restored.

An entry hidden by consolidation remains owned by that operation. Use `nunc consolidate undo` to restore its originals. A manual archive of an active summary hides the summary while preserving its existing sources and associations. Undo leaves the summary raw-only with status `undone`.

## Compare memories

```sh
nunc diff "$old_id" "$new_id"
```

Pass two journal UUIDs. The command reads the original Markdown bodies, including archived memories, and omits generated metadata. A change prints a unified diff with stable UUID headers, three lines of context, and plain text suitable for a pipe or patch file. Extremely large changed regions may be represented as a single replacement hunk. Identical bodies produce empty stdout and exit 0. Differences exit 1. Invalid, missing, forgotten, corrupt or empty entry files and operational failures exit 2 with diagnostics on stderr. The operation leaves the entries, ledger and index unchanged and works without a model.

## Consolidation and forget

```sh
nunc consolidate candidates "FIFO concurrency" --json
nunc consolidate apply --entries "$first,$second" --summary "An external summary."
nunc consolidate apply --entries "$first,$second" --summary-from-file summary.md
nunc consolidate apply --entries "$first,$second" --summary-stdin < summary.md
nunc consolidate undo "$summary"
nunc forget "$first" --dry-run --json
nunc forget "$first" "$second" --yes
```

The three summary inputs are alternatives for a single operation. Generate the summary text externally. Candidates return one best match per active parent with full body and revision. Pass repeating `--revision ID=HASH` preconditions using the candidate parent hash for stale-write protection. Applying an identical summary to unchanged entries can safely be retried. A nested summary must be undone from the outside inward. Undo restores surviving originals and leaves the old summary in `--raw` with status `undone`.

Forget validates every target, warns about direct and transitive dependent summaries, and requires typing `yes` in an interactive terminal or using `--yes`. Both modes show dependency warnings. A dry run writes nothing. Forget deletes only explicitly named UUIDs and retains no deleted body in its tombstone. Surviving summaries remain byte-for-byte unchanged and display forgotten-source markers. Git backfill respects tombstones. Forgetting a summary explicitly preserves its archive associations until that consolidation is undone. Git history, backups, filesystem remnants, and text copied into surviving summaries may still retain the information.

## Receipts and exit status

`Saved <id> <path>` means the Markdown file was durably published. `Queued for FIFO delivery` confirms the client wrote its frame to the FIFO. Use `log` when the caller requires a durable receipt.

Exit status is 0 on success, 2 for CLI syntax or argument parsing errors, and 1 for invalid input detected after parsing or operational failures. `diff` uses its own comparison exit statuses described above. Model, database, and filesystem errors go to stderr.

## Diagnose and repair the search index

```sh
nunc doctor --deep
nunc doctor --deep --json
nunc repair --dry-run --json
nunc repair
```

Deep diagnostics and repair dry runs open the index read-only and do not download the model. They compare the durable snapshot with indexed parents, chunk identities and hashes, path and line provenance, and embedding shape and validity. The audit also checks ledger lineage and reports leftover files from interrupted forget operations. JSON includes stable finding codes, severities, counts, sample identifiers, and the proposed action. Findings are a report and can be present when the command exits successfully.

Repair rebuilds the disposable SQLite index atomically when the cached model is available. When inference is unavailable, it can remove demonstrably stale, orphaned, invalid, or forgotten rows from a compatible index. Run `nunc models download` if needed, then retry repair to restore missing vectors. A malformed ledger or a missing consolidation target requires manual restoration or correction. The index uses rollback journaling. Unexpected WAL or shared-memory files require inspection before repair will replace the index. Repair does not edit or delete memory Markdown, including files left by interrupted forget. Retry the explicit forget operation to finish its durable file cleanup.

The audit detects wrong model metadata, mismatches between indexed text and its durable source, and invalid vector sizes or values. A finite vector with the correct size and matching provenance cannot be authenticated by this audit. No keyword index is present in the current schema.

## Read-after-write across processes

Use `nunc log` or a `nunc write` that reports `Saved` when a caller needs a durable Markdown receipt. The receipt includes the new entry UUID. Follow it with a successful `nunc reindex` in the same checkout before starting a semantic search that must consider that entry. Normal ranking, filters, archive state, and the result limit still apply. The running watcher can catch up automatically, and a search may warn about a stale snapshot while it does.

A FIFO receipt reports that the client wrote its frame. Wait until the server publishes the Markdown or confirms the saved entry before reindexing for read-after-write visibility. After a stopped server, direct capture still works via `nunc log` or the direct fallback in `nunc write`.

Independent processes in one worktree share Markdown, the ledger, and the index. Search and rebuild coordinate with local advisory locks. A search already in progress may observe the prior state. A new search after a completed archive or forget observes the ledger update, including when older rows remain in SQLite. Worktrees and separate clones have separate local journals and index paths. Intentional Git distribution can share tracked entries and configuration, and each checkout builds its own index.

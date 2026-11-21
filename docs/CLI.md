# CLI reference

| Command | Behavior |
| --- | --- |
| `nunc init [--source PATH]...` | Creates local storage and initializes config. |
| `nunc log [--tag TAG]... [--repo LABEL] TEXT...` | Saves a durable Markdown file and prints its ID and path. |
| `nunc log [--tag TAG]... [--repo LABEL] --stdin` | Reads a note from redirected stdin through EOF and saves it directly. |
| `nunc TEXT...` | Shorthand for `log` when the first argument is ordinary text and does not name a command. |
| `nunc write TEXT...` or `nunc write --stdin` | Queues capture via a live FIFO or saves directly when the server is unavailable. |
| `nunc serve [--boundary TOKEN] [--source PATH]...` | Runs the foreground FIFO and refreshes the index from local files. |
| `nunc reindex [--full] [--source PATH]...` | Reconciles changes or publishes a separately staged full rebuild. |
| `nunc search QUERY... [--limit N] [--json] [--type memory\|source] [--source PATH]...` | Ranks compatible indexed vectors by cosine similarity. |
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

Default limit is 5, with a range of 1–1000. `--type` selects memories or sources. Repeatable `--source` filters by exact repository-relative paths or directory prefixes and selects source results. Deleted paths remain usable as filters while the existing index is stale. An empty result set is successful.

`--json` prints one object with `mode_used` (always `semantic`), `repo_id`, `warnings`, and `results`. Result fields are `id`, `kind`, `path`, `timestamp`, `line_start`, `line_end`, `text`, `score`, and `rank`. Source ranges are one-based and inclusive. Missing provenance values are null. Memory text is the original body and source text is the original chunk. Rank starts at one, with descending cosine and ascending ID for exact ties. Search reports stale or unverifiable snapshots in `warnings` (stderr in human mode). Use `reindex` for fresh offline results.

## Receipts and exit status

`Saved <id> <path>` means the Markdown file was durably published. `Queued for FIFO delivery` confirms the client wrote its frame to the FIFO. Use `log` when the caller requires a durable receipt.

Exit status is 0 on success, 2 for CLI syntax or argument parsing errors, and 1 for invalid input detected after parsing or operational failures. Model, database, and filesystem errors go to stderr.

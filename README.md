# nunc-mens

`nunc` captures project notes as local Markdown and retrieves notes and explicitly selected files through semantic search. Run it from any directory in your Git worktree.

Install a published release from crates.io, or build the current version from GitHub.

```sh
cargo install nunc-mens --locked
# Install directly from GitHub:
cargo install --git https://github.com/rjtg/nunc-mens --locked
```

Both commands install the `nunc` executable. From a local checkout, use `cargo install --path . --locked`.

```sh
nunc init --source docs --source src
nunc log "Deploy tokens expired because the staging host's clock drifted."
nunc models download
nunc reindex
nunc search "Why did authentication fail in staging?"
nunc search "authentication" --context parent --max-tokens 2000 --json
nunc search "authentication" --format markdown --max-tokens 2000 > context.md
nunc search --json --type source --source docs "time synchronization"
nunc timeline --yesterday --group-by day
nunc topics --since 7d
nunc search --similar-to "$memory_id" --ids
nunc search "token rotation" --related-by time --window 2h --json
```

Capture works as soon as the binary is installed. `nunc log` (or `nunc "a note"`) saves a durable Markdown file and returns its ID and path. `nunc serve` optionally accepts multiline messages from a repository FIFO and keeps the index current. In another shell, use `nunc write --stdin` to send a message or `nunc write "a note"` for a one-liner.

For repeated searches from the same checkout, run `nunc serve --search-socket` and add `--server` to search requests. The endpoint shares the loaded model with indexing and uses the same search filters and formats. [Persistent search](docs/CLI.md#persistent-search)

Semantic search is the default. For exact terms such as error codes, paths and flags, try `nunc search --mode keyword SQLITE_BUSY`. Use `nunc reindex --keyword-only` to prepare a searchable keyword index without the model. `nunc search --mode hybrid` combines keyword and semantic ranks with the cached model.

```sh
nunc serve --source docs --source src
# In another shell in the same worktree:
printf '%s\n' 'Keep the old key available for sixty seconds.' '' 'Allow in-flight requests to finish.' | nunc write --stdin
nunc status
nunc doctor
```

Markdown originals live at `.nunc/entries/<uuid>.md`. The index lives under XDG state and can be rebuilt with `nunc reindex --full`. Model files live under XDG cache. The model download prepares about 487 MB of data. Search inference runs locally with FastEmbed, SQLite stores the vectors, and Rust ranks them by cosine similarity. Keep private notes and indexed source text out of published artifacts. Local Git ignore rules exclude new memory files by default.

## Work with project history

Version 0.2.0 adds semantic retrieval of Markdown chunks with section and parent context, optional recency, and ranking explanations. It also adds duplicate discovery, Git import, and explicit memory lifecycle operations. Run `nunc reindex` to rebuild a version 0.1.0 index for chunked retrieval.

```sh
nunc search --context section --explain "Why did authentication fail?"
nunc duplicates --json
nunc duplicates --exact --forget-ids | xargs -r nunc forget --dry-run
nunc diff "$old_id" "$new_id"
nunc git import --all --dry-run
nunc git import --limit 100
nunc consolidate candidates "deploy clock synchronization" --json
```

Use `consolidate apply` to save a summary you provide and archive its originals. Use `archive` and `unarchive` to change retrieval visibility while preserving original Markdown. Use `search --raw` to inspect archived history. `forget --dry-run` shows dependencies before an explicit deletion. To back up these relationships, retain both your Markdown entries and `.nunc/ledger.json`. [CLI reference](docs/CLI.md)

If the disposable index is out of sync, run `nunc doctor --deep` or preview repair with `nunc repair --dry-run`.

## Build and test

Use Rust 1.89 or newer, Git, a C/C++ toolchain, and the system TLS development libraries (OpenSSL on Linux). The default ONNX Runtime build targets are glibc Linux on x86-64 or AArch64 and macOS on Apple Silicon. The first build may retrieve the CPU ONNX Runtime distribution. Runtime model downloads are initiated separately by `nunc models download`.

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The default test suite runs without a downloaded embedding model. See [CLI reference](docs/CLI.md), [FIFO protocol](docs/REPO_FIFO.md), [architecture](docs/ARCHITECTURE.md), and [verified development usage](docs/DOGFOODING.md).

## License

This project is available under the [MIT License](LICENSE). The pinned E5 model is also MIT-licensed. [Model card](https://huggingface.co/intfloat/multilingual-e5-small)

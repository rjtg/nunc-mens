# nunc-mens

**Instant CLI journal and searchable memory for humans and coding agents.**

`nunc-mens` is a small, local-first Rust command-line tool. Its executable is **`nunc`**. Capture free-text notes immediately in human-readable Markdown, then retrieve relevant entries from the shell using exact keyword search and local semantic embeddings. **No MCP server, Ollama, cloud account or always-on daemon is required.**

> Status: specification and agent-ready bootstrap. Application code has not yet been implemented.

## Intended CLI

```sh
nunc log "Fixed staging deploy: token refresh failed because of clock skew"
nunc "Remember to investigate the intermittent CI timeout" # shorthand for log
nunc search "What caused the staging authentication failure?"
nunc search --mode keyword "clock skew"
nunc search --json --limit 5 "auth deploy"
nunc reindex
nunc reindex --full
nunc doctor
```

The CLI examples describe the **target interface**, not currently working commands. See [docs/CLI.md](docs/CLI.md) for precise behavior.

## Design principles

- **Write-first, offline-capable:** `nunc log` writes directly to Markdown without any model or network request.
- **Plain text is canonical:** individual Markdown entry files are the source of truth; the search database is disposable and reconstructible.
- **Small and understandable:** one Rust binary, small dependencies, no mandatory daemon or MCP requirement.
- **Search that works for agents:** machine-readable JSON, stable entry identifiers, timestamps, provenance and useful exit codes.
- **Hybrid retrieval:** SQLite FTS5 keyword search plus cosine similarity of locally generated embeddings; exact identifiers should not disappear in semantic scores.
- **In-process embeddings:** use the Rust `fastembed` crate to run a small Hugging Face-hosted ONNX embedding model locally, without Ollama or any HTTP inference service. Explicit `nunc models download` prepares the local cache; indexing/search never downloads models implicitly.
- **Safe and private:** don't log secrets; enforce local-only defaults; never commit private notes, index files, runtime artifacts, or credentials.
- **Dogfood the CLI:** contributors and coding agents must use `nunc` to capture work notes and retrieve prior decisions once commands are implemented, and verify actual output rather than pretending tests succeeded.

## Proposed architecture

```text
nunc log / nunc "..."
    -> .nunc/entries/<id>.md  (one file per entry; source of truth)

nunc reindex
    -> parse Markdown entries -> stable IDs -> fastembed embeddings -> SQLite index

nunc search
    -> FTS5 keyword retrieval + cosine vector retrieval -> merged ranked results
    -> readable text or --json (CLI and shell-capable agents)
```

Recommended defaults: repository-local `.nunc/entries/`, with repository-scoped index in the XDG state directory and models in the XDG cache directory. All paths configurable via env/flags. The app must never assume it owns arbitrary user Markdown files.

## Development

The first deliverable is a working, tested MVP, not an MCP integration or a sophisticated reranker. Follow [AGENTS.md](AGENTS.md), [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), and [docs/ROADMAP.md](docs/ROADMAP.md).

Planned Rust stack: `clap`, `serde`, `serde_json`, `rusqlite` (bundled SQLite with FTS5), `fastembed` (in-process ONNX embeddings), `sha2`, and standard-library file operations. Add dependencies only when justified. Initialize the crate and its `Cargo.lock` during the MVP implementation.

```sh
# Once the CLI has been implemented:
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo run -- --help
```

## Agent onboarding

If you are a coding agent, **read `AGENTS.md` first**. Implement the milestones in `docs/ROADMAP.md` in order; create real runnable code and tests, document deviations, and use the resulting tool as part of your own workflow. Do not replace the requested small CLI with QMD, a mandatory system-wide server, or a hosted memory SaaS.

## License

No license has been selected yet. Do not add one without a project-owner decision.

## Repository-local FIFO mode (new target architecture)

**The repo-first design in [docs/REPO_FIFO.md](docs/REPO_FIFO.md) supersedes the older global daily-journal layout above.** This repository still contains specifications, not a working CLI.

```sh
cd my-repo
nunc serve --source ./src --source ./docs --source ./AGENTS.md
# Prints .nunc/input.fifo and a random session boundary; --boundary can override
nunc write "Cache invalidation uses versioned keys"
nunc search --type memory "Why did caching change?"
nunc search --type source --source ./docs "cache"
nunc reindex
```

The foreground `serve` process accepts boundary-framed messages through a repo-local FIFO. Each complete message becomes one `.nunc/entries/<id>.md` file with metadata and is indexed immediately. Repeatable `--source` flags register read-only files/directories for initial indexing and watching. `nunc log`/`nunc write` must also work without a running server, persisting entries directly for later indexing. The SQLite index is repo-scoped in XDG state; the model cache is outside Git. Project entry files may be intentionally Git-tracked; FIFO, session files, indexes and secrets must not be committed. No Ollama or mandatory daemon. See [docs/REPO_FIFO.md](docs/REPO_FIFO.md) for framing, concurrency and security guarantees.

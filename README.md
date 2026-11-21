# nunc-mens

**Instant CLI journal and searchable memory for humans and coding agents.**

`nunc-mens` is a small, local-first Rust command-line tool. Its executable is **`nunc`**. Capture free-text notes immediately in human-readable Markdown, then retrieve relevant entries from the shell using exact keyword search and local semantic embeddings. **No MCP server, cloud account or always-on daemon is required.**

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
- **Plain text is canonical:** daily journal files are the source of truth; the search database is disposable and reconstructible.
- **Small and understandable:** one Rust binary, small dependencies, no daemon or MCP requirement.
- **Search that works for agents:** machine-readable JSON, stable entry identifiers, timestamps, provenance and useful exit codes.
- **Hybrid retrieval:** SQLite FTS5 keyword search plus cosine similarity of locally generated embeddings; exact identifiers should not disappear in semantic scores.
- **Opt-in embedding backend:** initial implementation talks to a locally running Ollama embedding API. No automatic remote upload, no downloaded models without explicit user action.
- **Safe and private:** don't log secrets; enforce local-only defaults; never commit journals, index files, or credentials.
- **Dogfood the CLI:** contributors and coding agents must use `nunc` to capture work notes and retrieve prior decisions once commands are implemented, and verify actual output rather than pretending tests succeeded.

## Proposed architecture

```text
nunc log / nunc "..."
    -> $NUNC_JOURNAL_DIR/YYYY-MM-DD.md  (append-only Markdown; source of truth)

nunc reindex
    -> parse Markdown entries -> stable IDs -> Ollama embeddings -> SQLite index

nunc search
    -> FTS5 keyword retrieval + cosine vector retrieval -> merged ranked results
    -> readable text or --json (CLI and shell-capable agents)
```

Recommended defaults: journal in `$HOME/.local/share/nunc-mens/journal`, index in `$HOME/.local/state/nunc-mens/index.sqlite3`, optionally use XDG environment variables. All paths configurable via env/flags. The app must never assume it owns arbitrary user Markdown files.

## Development

The first deliverable is a working, tested MVP, not an MCP integration or a sophisticated reranker. Follow [AGENTS.md](AGENTS.md), [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), and [docs/ROADMAP.md](docs/ROADMAP.md).

Planned Rust stack: `clap`, `serde`, `serde_json`, `rusqlite` (bundled SQLite with FTS5), `ureq` (HTTP to Ollama), `sha2`, and standard-library file operations. Add dependencies only when justified. Initialize the crate and its `Cargo.lock` during the MVP implementation.

```sh
# Once the CLI has been implemented:
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo run -- --help
```

## Agent onboarding

If you are a coding agent, **read `AGENTS.md` first**. Implement the milestones in `docs/ROADMAP.md` in order; create real runnable code and tests, document deviations, and use the resulting tool as part of your own workflow. Do not replace the requested small CLI with QMD, a persistent server, or a hosted memory SaaS.

## License

No license has been selected yet. Do not add one without a project-owner decision.

# Development dogfooding

The executable ran from the working tree against a temporary Git repository. Its Markdown, index, and model cache lived outside this checkout. The evidence below uses sanitized examples and keeps conversation entries out of Git.

## Direct capture and the switch to FIFO

- Phase D made the executable usable. From a nested fixture directory, `nunc init` initialized the Git root. A multiline development decision piped to `nunc log --stdin --tag architecture` created one Markdown file. A byte comparison passed, including the blank line and omitted final LF. The metadata ID matched the filename and the tag was present. The fixture had no embedding model.
- Phase E passed `cargo test --locked --offline` with three unit tests and three executable tests. The FIFO tests delivered six simultaneous CLI records alongside raw frames, compared exact bodies, and exercised a forced exit, stale-session fallback and restart.
- The ongoing conversation journal began in the same fixture immediately after the request for progressive adoption arrived. The already verified direct CLI imported the original goal as a labeled bootstrap summary, a SQLite architecture decision, diagnostics and checkout recovery findings, and milestone notes. Then `nunc serve --boundary conversation-dev` published the FIFO. Subsequent conversation entries used `nunc write --stdin`; the client returned `Queued for FIFO delivery`, and the files were separately verified. The opening message, the visible progressive-adoption request, and two visible assistant messages were copied verbatim. The earlier implementation request and other progress were labeled summaries. The thread reader returned only a truncated assistant view, so active conversation notes were captured explicitly.

## Initial index and semantic adoption

- With a separate empty cache, `nunc search --json "transactional recovery"` exited 1, printed the model download command on stderr, and produced no JSON. The FIFO stayed usable.
- The first `nunc models download` hit sandbox DNS restrictions. The same explicit command with network permission downloaded the pinned 470,268,510-byte ONNX file and four tokenizer/config files, validated a real query embedding, and published the cache. Capture remained independent of the transfer.
- `nunc reindex` printed `Indexed 15 documents (15 embedded, 0 deleted)`. `nunc doctor` reported current. A Python SQLite query confirmed all 15 memory vectors were 1,536 bytes (384 little-endian floats).
- `nunc search --json --type memory --limit 3 QUERY` was used immediately to retrieve earlier development context. Each returned empty warnings.

| Query | Observed result |
| --- | --- |
| Why keep the search database disposable while retaining original notes? | The desired SQLite decision was at rank 3, score 0.8433. The broad wording ranked a different note about Markdown first. |
| Which persistence engine holds rebuildable dense representations while Markdown remains authoritative? | The same SQLite decision was at rank 1, score 0.8183. |
| Why were the model files initially unreachable and how was setup resumed? | The sandbox DNS failure and approved retry were at rank 1, score 0.8408. |
| How should an agent build a habit of using this tool before the project is finished? | The progressive-adoption user message was at rank 1, score 0.8361. |
| What keeps simultaneous senders from mixing bytes on the pipe? | The cooperative FIFO-writer milestone was at rank 1, score 0.8053, before implementing further server changes. |

The rank-three result was a useful reminder that cosine ranking is approximate and broad queries can favor nearby decisions. A more specific paraphrase recovered the intended record without changing the ranking algorithm. The opt-in executable test also passed using a German query against an English note, with separate source-filter and JSON provenance assertions.

While logging one development finding, a command used the implementation checkout instead of the fixture. I moved the generated runtime directory out of the checkout, re-sent the note through the fixture FIFO, and pinned subsequent operations to the fixture working directory.

## Watching, recovery and rebuild

A real FIFO repro kept a raw writer open with an unfinished frame, then invoked `nunc write`. The previous parser combined the abandoned bytes with the next note because it had not observed EOF. Session version 2 added a client-start control line and a byte that breaks an unfinished delimiter. Regression tests cover both the parser and two real pipe writers. Repeating the repro against the current server reported the expected incomplete-frame diagnostic, then saved only the new note. Its Markdown body matched byte-for-byte, including a blank line and missing final LF, and its metadata UUID matched the filename.

- Installed the watcher in the development loop with `nunc serve --boundary development-watch --source docs --source src`. The fixture used copies of the implementation's architecture document and vector codec, plus two disposable source fixtures.
- The SQLite architecture query returned both kinds in a combined search. Applying `--type memory --limit 5` found the original decision at rank 2 among newer conversation entries. Applying `--type source --source docs` put the architecture document at rank 1 with lines 1–14. `nunc search --json --source src/vectors.rs --limit 1 "How are 32-bit embeddings represented byte by byte in storage?"` returned the actual vector codec, lines 1–44.
- Edited `docs/lease.md` to three lines and removed `src/retired.rs`. The watcher reported `Indexed 31 documents (1 embedded, 1 deleted)`. `nunc search --json --source docs/lease.md --source src/retired.rs "How long do credentials keep working after a key rollover?"` returned exactly one source, the changed `docs/lease.md` at lines 1–3, with the new text and empty warnings.
- SIGTERM removed the FIFO and session. `nunc write --stdin` returned a durable file receipt while stopped, with exact bytes checked separately. The next cached search reported a stale index. Search with an empty cache exited 1 without producing JSON.
- Restarted with `nunc serve` and recovered both registered source directories. `nunc reindex --full` reported `Indexed 32 documents (32 embedded, 0 deleted)`. The original SQLite decision retained its ID and rank 2 with no freshness warnings. `nunc doctor` reported 32 current documents. The restarted server shut down cleanly.
- Both later user status questions and their immediate assistant replies were captured verbatim via the FIFO. Other development commentary was recorded as labeled summaries or structured milestone entries. The watcher indexed the final handoff summary after it was sent through the FIFO and its persisted bytes were verified. The first technical request remains a labeled bootstrap summary.

## Verification

Executed on Linux x86-64 with Rust 1.98.1.

| Command | Result |
| --- | --- |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --all-targets -- -D warnings` | Passed |
| `cargo test` | 10 library tests and 5 executable tests passed. The 2 real-model tests were opt-in. |
| `NUNC_TEST_MODEL_CACHE=<prepared-cache> cargo test --locked --offline --test cli -- --ignored --test-threads=1` | Both tests passed in 43.44 s with the explicitly downloaded E5 model. They verified multilingual retrieval and watcher updates, deletion and restart. |
| `cargo build --locked --offline` | Passed and produced the binary used for the final conversation-fixture run. |
| `git diff --check` | Passed |

The Rust LSP intermittently timed out and returned an older diagnostic after the underlying code was fixed. Cargo compilation, lint and tests checked the current source. The default watcher polls and the search performs a linear scan. Platform behavior was exercised on Linux x86-64. The rest of the supported ONNX build matrix is documented in the README.

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

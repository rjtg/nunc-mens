# Development dogfooding

The executable ran from the working tree against a temporary Git repository. Its Markdown, index, and model cache lived outside this checkout. The evidence below uses sanitized examples and keeps conversation entries out of Git.

## Direct capture and the switch to FIFO

- Phase D made the executable usable. From a nested fixture directory, `nunc init` initialized the Git root. A multiline development decision piped to `nunc log --stdin --tag architecture` created one Markdown file. A byte comparison passed, including the blank line and omitted final LF. The metadata ID matched the filename and the tag was present. The fixture had no embedding model.
- Phase E passed `cargo test --locked --offline` with three unit tests and three executable tests. The FIFO tests delivered six simultaneous CLI records alongside raw frames, compared exact bodies, and exercised a forced exit, stale-session fallback and restart.
- The ongoing conversation journal began in the same fixture immediately after the request for progressive adoption arrived. The already verified direct CLI imported the original goal as a labeled bootstrap summary, a SQLite architecture decision, diagnostics and checkout recovery findings, and milestone notes. Then `nunc serve --boundary conversation-dev` published the FIFO. Subsequent conversation entries used `nunc write --stdin`; the client returned `Queued for FIFO delivery`, and the files were separately verified. The opening message, the visible progressive-adoption request, and two visible assistant messages were copied verbatim. The earlier implementation request and other progress were labeled summaries. The thread reader returned only a truncated assistant view, so active conversation notes were captured explicitly.

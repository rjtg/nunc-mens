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

## M5–M9 development loop

- Before editing, the installed binary recorded the new milestone request as a labeled summary in the isolated conversation fixture. A semantic search for why the database is disposable retrieved the earlier authoritative-Markdown decision at rank 2. Baseline tests passed 15 cases.
- The compiled milestone binary accepted a real development note with nested headings for durability and ranking. `nunc reindex` upgraded its schema-1 database and reported 67 chunks with 66 embeddings. `nunc search --type memory --context section --explain --json --limit 2 "How does archival survive a process dying mid-publication?"` returned **Reliability milestones / Crash recovery**, beginning at body line 5, at rank 1. Cosine and final score were both 0.845357 with default recency disabled. A second paraphrase recovered the same section at rank 1.
- `nunc write --check-duplicates` reported a whitespace-normalized exact match for a repeated development note and persisted the new entry anyway. `nunc duplicates --exact --json --limit 2` showed the two different entry IDs as an entry-level match at 1.0. During schema migration the semantic check reported the incompatible legacy index while exact detection and capture kept working.
- The opt-in real-model suite first passed all three tests in 178.34 s. It exercised multilingual retrieval, automatic source edits/deletions and restart, and the combined chunk, explanation, duplicate and memory lifecycle test. The combined test used a 0.86 cutoff to find a paraphrase at chunk scope and a stricter 0.96 cutoff to screen a distinct revision. A high similarity remains an advisory signal.

### Chunking benchmark

Four labeled queries ran against the repository's pre-milestone architecture, CLI, FIFO, and implementation plan Markdown copied from commit `ac0abe6` to an isolated Git worktree. Each strategy rebuilt from scratch with the same cached multilingual E5 model and a 160-token budget. The semantic boundary threshold was 0.80. A hit counted when its path and heading ancestry matched the label. Query wall times include model initialization and the freshness check. Other verification ran on the machine during part of the measurement, so the wall times are descriptive.

| Strategy | Chunks | Index bytes | Full rebuild | Median query | Expected ranks | Recall@5 | MRR@5 |
| --- | ---: | ---: | ---: | ---: | --- | ---: | ---: |
| Structural | 63 | 356,352 | 25.454 s | 8.283 s | 1, miss, 5, 1 | 0.75 | 0.55 |
| Semantic | 63 | 356,352 | 26.595 s | 5.516 s | 1, miss, 5, 1 | 0.75 | 0.55 |

The questions covered FIFO serialization, the distinction between durable Markdown and the vector cache, program-readable provenance, and milestones that run without a model. Both strategies missed the expected architecture heading on the second query. These measurements support retaining structural chunking as the default for this release. Chunk context keeps the response focused on the ranked passage. Switching to section or parent context changes returned text while preserving the ranked passage.

This benchmark measured two `nunc` configurations. The comparison with QMD is tracked separately in [issue #8](https://github.com/rjtg/nunc-mens/issues/8).

### Git and durable lifecycle

The separate benchmark worktree also verified history import, hook capture and sync. With no hook installed, `nunc git import --all --dry-run` reported one commit and `nunc git import --all` saved it. After `nunc git hook install`, a normal Git commit captured the decision to keep archive relationships durable. `nunc git sync` reported `2 selected, 0 saved, 2 existing, 0 forgotten`. A paraphrased query found the hook-captured commit at rank 1 with its original committer timestamp and `choice.md` path.

Destructive tests used disposable fixture notes. The test suite covers all three summary input modes, stale revision rejection, nested archival, competing apply calls, idempotent retries, a simulated interrupted ledger publication, and logical forget recovery. A staged summary remained invisible until a retry committed its ledger association. The unrelated fixture note remained present.


A separate executable run found both logged candidate IDs with `nunc consolidate candidates … --json`, committed an inline summary, and checked that normal search hid the originals while raw search returned their archive target. The dry run reported the dependent summary. Explicit forget and a full rebuild removed the selected disposable memory, retained the other entries, and showed the forgotten-source marker without changing the summary bytes.

After an incremental refresh of the conversation fixture, `nunc duplicates --threshold 0.88 --limit 5 --json` reported one exact pair and four entry-level semantic pairs with cosine scores 0.9885, 0.9788, 0.9745 and 0.9702. The results were advisory and no note was suppressed or removed.

### Final checks

- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and `git diff --check` passed. The default suite passed 28 tests, 14 library and 14 executable tests. The three real-model tests are opt-in.
- `NUNC_TEST_MODEL_CACHE=<prepared-cache> cargo test --locked --offline --test cli -- --ignored --test-threads=1` passed all three earlier in the final verification pass. After adding a simulated interruption with stale SQLite rows still present, the expanded `real_context_explanations_duplicates_and_lifecycle` test passed separately in 120.53 s. The additional default tests verified concurrent forget, actual hook re-execution after forgetting a commit, sanitized bounded diffs, and migration of the default local ignore file.
- The Rust LSP intermittently returned stale responses or timed out for touched files. Targeted Cargo builds and the full checks above verified the current source. Runtime results were observed on Linux x86-64.

## Index health and repair

In an isolated Git worktree, the built binary logged a real implementation decision about reusing chunk identity and staged SQLite publication. After indexing with the cached model, a fixture removed one chunk row directly from SQLite. `nunc repair --dry-run --json` reported exactly one `MISSING_CHUNK` finding and left the index bytes unchanged. `nunc repair --json` rebuilt the index, and a repeat reported no work. A paraphrased search found the original parent ID with byte-identical Markdown.

Formatting, Clippy, and 30 default tests passed. The opt-in `audit_and_repair_disposable_index_while_preserving_durable_visibility` test passed with the cached ONNX model. It exercised missing and stale chunks, invalid vectors, deleted sources, stale forgotten rows, offline pruning, concurrent durable logging, archive and raw visibility, and a corrupt SQLite file. The Rust LSP returned clean results for some files and stale or timed-out responses for others. Cargo supplied the final verification.

## Reversible archive

The built binary logged a real implementation decision about reversible archive in an isolated Git worktree. After archiving one of two intentionally matching notes, ordinary `duplicates --exact --json` returned no pairs and `duplicates --exact --raw --json` returned one. Unarchive restored the ordinary pair. The original entry bytes matched throughout.

Formatting, Clippy, and 32 default tests passed. The opt-in `manual_archive_changes_search_visibility_without_reembedding` test passed with real ONNX inference. It confirmed immediate default and raw visibility changes, full rebuild persistence, unarchive, and no raw leakage after explicit forget. Additional tests verified multi-entry changes, dry-run, idempotency, nested consolidation and an interrupted ledger publication. Some LSP calls timed out, so Cargo supplied final verification.

## Standalone multi-process consistency

Formatting, Clippy, and 34 default tests passed. Twelve independent `nunc write` processes received unique durable receipts and saved all original bodies. A linked Git worktree had its own journal and distinct disposable index path.

The opt-in `standalone_read_after_write_and_concurrent_visibility_follow_the_contract` test passed with real ONNX inference in 26.98 s. It observed a stale result immediately after durable capture, then held the actual index lock until a search process was confirmed to hold a shared memory lock. It launched competing reindex, archive and forget processes before releasing the reader. That in-flight search returned the original snapshot. A successful reindex then made the new entry searchable, ordinary search excluded archived content, and raw search retained only the archived content among the two hidden IDs. Persistent-transport and cache coverage remain open in #37. The LSP timed out on the test file, so Cargo verified it.

## Registered source filtering

In an isolated Git worktree, the compiled binary logged a real decision about filtering candidates before scoring and registered two sibling documentation paths. From a nested working directory, `nunc search --json --limit 1 "what should constrain search candidates early" --source-file ./docs/decisions/source.md` returned exactly that file, with matched-chunk line provenance.

Formatting, Clippy, and 35 default tests passed. The opt-in `exact_source_filters_union_and_apply_before_ranking` test used real ONNX inference and covered ADRs, design files, RFCs, plans, incidents, research, recursive paths, multi-chunk documents, empty directories, and both directory and direct-file registration. A deleted file remained selectable until reindex removed its stale rows. Invalid paths were rejected without producing search output. The LSP timed out on the edited files, so Cargo supplied verification.

## Ranked shell output

The compiled CLI recorded and indexed a two-chunk development note about ranking parents for shell selection. `nunc search --type memory --ids --limit 1 "How can I send a unique memory selection into another command?" | xargs -r nunc archive --dry-run --json` passed exactly the UUID from the capture receipt to the archive preview. The first run placed the index inside Git and failed with the expected path error. Moving the index to a sibling directory fixed the fixture.

Formatting, Clippy, and 36 default tests passed. The opt-in `search_ids_orders_unique_parent_ids_and_preserves_visibility` test passed with the cached ONNX model in 34.91 s. It covered ranked IDs, chunk deduplication, Git entries, path filtering, empty output, archive visibility and forgotten entries. The LSP timed out on the edited files, so Cargo supplied verification.

## Model-free memory diff

In a temporary Git repository, the compiled CLI logged two revisions of a real duplicate-review decision. `nunc diff "$old_id" "$new_id" > review.patch` exited 1 with two UUID headers and the expected changed line. GNU `patch` applied the output to the first body, and `cmp` confirmed byte-for-byte agreement with the second body. The fixture never created an index or model cache.

Formatting, Clippy, and 39 default tests passed. Diff coverage includes identical entries, archived entries, empty and missing files, restored files with forgotten tombstones, Unicode, long lines, fenced code, CRLF, missing final LF, and large changed regions. Some Rust LSP responses were stale or timed out, so Cargo verified the final sources.

## Duplicate group review and cleanup preview

An isolated Git repository captured a real cleanup-policy note twice and a whitespace variant once. `nunc duplicates --exact` printed one group with the first capture as `KEEP`, the second as `DUPLICATE`, and the variant as `REVIEW`. Piping `nunc duplicates --exact --forget-ids` into `xargs -r nunc forget --dry-run --json` sent exactly the second UUID. All three files remained after the preview.

Formatting, Clippy, and 41 default tests passed. Tests exercised timestamp ordering, normalized-only matches, raw archive visibility, forgotten entries, consolidation lineage, and overlapping semantic groups with direct evidence. The opt-in `real_context_explanations_duplicates_and_lifecycle` test passed in 73.77 s with the cached ONNX model. It confirmed that the actual paraphrase appeared as a review-only semantic match with provenance on both sides and no suggested cleanup IDs. Several Rust LSP responses timed out, and Cargo supplied final verification.

## QMD comparison

The release binary logged the decision to compare byte-identical bodies and measure both finding a source and returning its evidence. The benchmark used isolated journal and QMD indexes and real ONNX and GGUF models. The first harness run exposed a QMD path-normalization bug when it emitted a relative Markdown path. Regression tests cover relative paths, absolute paths, and empty responses. The corrected matrix finished 306 query invocations. A separate run exercised the literal flag `--from-checkpoint` in four modes. Python LSP diagnostics were unavailable because pyright was missing. Four focused Python tests, Rust formatting, Clippy, and 41 default Rust tests passed. [Full methodology, results, and raw records](BENCHMARK_QMD.md)

## Budgeted attributed search

In the first measured run, admitting more parents at 2,000 estimated tokens crowded out some text from the top matches. Keeping the complete matched chunk where it fits and reserving space for local expansion restored all three benchmark evidence passages at 2,000 tokens. The 800-token trial still lost the backlog threshold, which a lower budget could reasonably omit. [Budget benchmark](BENCHMARK_QMD.md#follow-up-on-context-budgets)

In a separate Git fixture, the real executable logged the decisions about ranked allocation and returned offsets, then indexed both entries. The paraphrase “How do we stop a large result from crowding out other documents?” retrieved the allocation decision at rank 1 and the attribution decision at rank 2, using 460 of 1,000 estimated tokens. Formatting, Clippy, 46 default Rust tests, four benchmark tests, and the targeted ONNX integration test passed. Rust LSP diagnostics timed out on several edited files, and Cargo verified those sources.

## Markdown context bundle

The compiled CLI captured a development note containing a Unicode heading and a fenced Rust example. After indexing, `nunc search 'How do we preserve captured code blocks when exporting context?' --format markdown --context parent --max-tokens 600 > bundle.md` produced 1,557 bytes of attributed Markdown and included the original `render_source_excerpt();` line. Stderr reported 390 of 600 estimated tokens. No diagnostic appeared in the bundle.

Formatting, Clippy, 48 default Rust tests, and the targeted ONNX integration test passed. The integration parsed the bundle with CommonMark, verified that nested source headings and fences stayed literal, and checked normal, raw, forgotten, and one-token results. The first test incorrectly expected quiet stderr after a forget operation. The corrected fixture checks the freshness warning on stderr and confirms that reindex leaves the bundle's stdout unchanged. Rust LSP diagnostics timed out on some files, and Cargo verified those sources.

## Temporal search and activity timeline

In an isolated Git repository, the compiled CLI recorded decisions about local calendar bounds and explicit anchor deltas. `nunc search --since 1h --ids` returned both IDs in original event order. `nunc timeline --around "$anchor" --window 1h --json` returned only the other decision and recorded its signed delta as +24 ms. The repository used empty state and model paths, and neither directory was created. The CLI suite also exercised New York's 23-hour and 25-hour days and an imported Git commit dated in 2024.

Formatting, Clippy, and all 51 default Rust tests passed. The targeted real ONNX test passed in 20.19 s. It found two SQLite anchors with a date filter, reported one other activity from both overlapping windows, and preserved the ID-based timeline through a full rebuild. Cargo verified sources where Rust LSP diagnostics timed out.

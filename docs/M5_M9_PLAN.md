# Milestones M5–M9

Baseline ac0abe6. All 15 default tests passed on Linux. The existing binary captured the new request and retrieved the earlier SQLite decision in an isolated development journal.

1. #2. Parse Markdown block offsets with CommonMark, use the existing tokenizer for budgets and embeddings for optional adjacent-block boundaries. Index parents separately from chunks, keep stable content-based chunk IDs, and cache unchanged layouts and vectors. Compare structural and semantic results on labeled local queries.
2. #3–#4. Rank by cosine plus an optional decaying timestamp contribution, expose the calculation and matched chunk, and use the same vector operations for near-duplicate discovery. Normalized exact discovery reads durable parents and works without embeddings.
3. #5. Parse and stream Git history through one ingestion path. Persist immutable commit provenance and use deterministic commit IDs, bounded sanitized diff metadata and a managed opt-in hook. Exercise retry and coexistence using real temporary repositories.
4. #6–#7. Store lineage and content-free tombstones in one atomic worktree ledger. Publish a gated summary before committing its archive associations. Persist forget intent before unlinking bodies and purging the index. Serialize mutations and hold a read guard while consulting the index. Durable metadata gates ordinary and raw search even when reconciliation is pending.
5. Verify migrations, interruption and concurrency, run default and actual-model tests, record dogfooding evidence, and update each issue only to the extent its acceptance criteria are verified.

The six workflows and tests are implemented. See the [M5–M9 dogfooding report](DOGFOODING.md#m5m9-development-loop) for benchmark measurements, actual query results, offline and real-model verification, and limits.

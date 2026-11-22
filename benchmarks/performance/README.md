# Search performance fixtures

This fixture is for the standalone scale experiments in [#29](https://github.com/rjtg/nunc-mens/issues/29). For labeled quality judgments and comparisons with QMD, see the [retrieval benchmark](../retrieval/README.md).

```sh
scratch=$(mktemp -d)
python3 -B benchmarks/performance/generate.py "$scratch/repo" 1000
(
  cd "$scratch/repo"
  NUNC_STATE_DIR="$scratch/state" nunc reindex --source docs
)
```

Generate 1,000, 10,000, 50,000, and 100,000 files in separate scratch repositories for a scaling run. Each source contains one short paragraph. The generator cycles through 256 distinct bodies with four distributed topics so embedding reuse keeps initial corpus construction practical. Report this as a search-scaling fixture with embedding reuse. `--unique-text` gives each document its own indexed body and is suitable for measuring indexing cost with much less reuse.

Every generated repository includes `queries.json` with conceptual prompts and exact ticket identifiers. Both query kinds have unfiltered, directory, and exact-file variants. At 1,000 chunks, the conceptual prompt “parallel writes” found a runbook in semantic and hybrid modes and had zero keyword matches. Exact identifiers exercise keyword scoring on the same fixture. The query file is outside the indexed `docs` tree. Peak memory, per-stage profiling, and the scaling baseline are still tracked in #29.

## Standalone scale benchmark

Build with the optional profiling feature and point the runner to a cache prepared using `nunc models download`. Use a scratch location with room for four generated repositories and their SQLite indexes. Each repository and state directory is isolated.

```sh
cargo build --release --locked --features search-profile
scratch=$(mktemp -d target/nunc-scale.XXXXXX)
python3 -B -m benchmarks.performance.run --root "$scratch/fixtures" \
  --output "$scratch/scale.jsonl" --nunc target/release/nunc \
  --model-cache "$NUNC_MODEL_CACHE_DIR"
python3 -B -m benchmarks.performance.summarize "$scratch/scale.jsonl"
```

The runner defaults to all four sizes, all three modes, four topics, and two rounds. Use repeated `--size` or `--mode` flags for targeted runs. Use `--topics 1` for one concept and one exact identifier per filter and mode. Increase `--rounds` for more stable percentile estimates. The summarizer reports nearest-rank p95 and p99, so tails from small samples are observed maxima. Throughput is serial queries divided by summed request time.

Each request starts a new process. The first pass follows indexing. The repeated pass follows one recorded warm-up request per mode. The operating-system page cache stays in place throughout. These are first and repeated standalone samples. A persistent ONNX session, verified cache hits and misses, and concurrent throughput require the persistent endpoint from #30 and any later cache from #31.

The profiling feature adds no CLI arguments. Setting `NUNC_SEARCH_PROFILE=1` emits one diagnostic JSON object tagged `nunc-profile` on stderr. Ordinary JSON and other stdout formats retain their existing contract. Scan spans include SQL retrieval, decoding or lexical scoring, and shortlist ranking. The source and visibility filter spans are measured inside them. `outside_cli` is the difference between process wall time and the post-argument-parse CLI span. It includes process startup, argument parsing and final shutdown. The remaining CLI time also includes cleanup between timed stages. The feature takes timestamps for each examined filter and adds some overhead. Default builds elide the timing spans.

Measurements include the machine, platform, model revision, index cost recorded separately from search, phase, mode, filter, latency, memory, and raw timings. The small-scale retrieval comparison is documented separately. [Retrieval comparison](../../docs/BENCHMARK_QMD.md)

The [recorded baseline](../../docs/BENCHMARK_SCALE.md) links to the raw 1,000–100,000 chunk measurements. Recreate all of its tables with `python3 -B -m benchmarks.performance.summarize benchmarks/performance/results/standalone-linux-x86_64.jsonl.gz`.

## Resident endpoint comparison

Use a fresh scale fixture whose index has already been built with the pinned model. The comparator starts its own foreground server, requires byte-identical JSON from both transports, and checks that warm vector requests do not reload ONNX.

```sh
python3 -B -m benchmarks.performance.compare_socket \
  --repo "$scratch/fixtures/1000/repo" --state "$scratch/fixtures/1000/state" \
  --nunc target/release/nunc --model-cache "$NUNC_MODEL_CACHE_DIR" \
  --query "Verifying download integrity" --samples 5 --output "$scratch/socket.jsonl"
```

The output includes separate client wall times and server stages. The first socket request is marked cold. Socket publication happens before a model load necessarily finishes, so the first query includes any wait for model initialization. Later samples each start a fresh CLI client while the server stays alive. Use a fixture path short enough for the platform's Unix socket pathname limit.

Add `--measure-cache` for a vector-mode run that starts with a distinct primer, then measures one warm miss and repeated exact hits. The comparator validates cache events against the embedding timing stage and compares the measured query's JSON against standalone output. Server profiling emits one `nunc-cache` record per embedding lookup with boolean `hit` and `cacheable` fields. The record contains no query text.

## Source freshness comparison

Build both revisions with `search-profile`. Use the same isolated, already indexed scale fixture for both executables.

```sh
python3 -B -m benchmarks.performance.compare_freshness \
  --repo "$scratch/fixtures/100000/repo" --state "$scratch/fixtures/100000/state" \
  --before /path/to/before/nunc --after target/release/nunc \
  --model-cache "$NUNC_MODEL_CACHE_DIR" --source-file docs/decisions/note-000000.md \
  --query SPW-000000 --output "$scratch/freshness.jsonl"
```

The comparator seeds the new additive fingerprint via a keyword-only reindex, then requires current and byte-identical results from both executables in all three modes. It records total wall time and timed stages. Increase `--samples` for a larger comparison. [Measured result](../../docs/BENCHMARK_SCALE.md#source-freshness-shortcut-at-100000-chunks)

Pass `--touch-source` to test one metadata-only mismatch on the isolated fixture. The harness temporarily changes and restores the selected file's mtime. The filesystem's ctime records the change. The query must still return current, identical JSON. [Recorded comparison](../../docs/BENCHMARK_SCALE.md#changed-source-metadata)

Use `--source docs/decisions --query staging` in place of the exact-file arguments to reproduce the directory probe. [Directory measurements](../../docs/BENCHMARK_SCALE.md#indexed-directory-and-mixed-scopes)

## Journal freshness probe

Use the before binary to build the same fixture's initial index with a prepared model, then make a separate state copy. The comparator updates only the new state copy. Both binaries must have the `search-profile` feature.

```sh
scratch=$(mktemp -d target/nunc-journal.XXXXXX)
python3 -B -m benchmarks.performance.generate_journal "$scratch/repo" 2000
env NUNC_REPO_ROOT="$(realpath "$scratch/repo")" \
  NUNC_STATE_DIR="$(realpath -m "$scratch/baseline-state")" \
  NUNC_MODEL_CACHE_DIR="$NUNC_MODEL_CACHE_DIR" /path/to/before/nunc reindex
cp -a "$scratch/baseline-state" "$scratch/state"
python3 -B -m benchmarks.performance.compare_freshness \
  --repo "$scratch/repo" --before-state "$scratch/baseline-state" --state "$scratch/state" \
  --before /path/to/before/nunc --after target/release/nunc \
  --model-cache "$NUNC_MODEL_CACHE_DIR" --source .nunc/entries \
  --query WriterLock --samples 3 --output "$scratch/journal.jsonl"
```

[Recorded journal comparison](../../docs/BENCHMARK_SCALE.md#journal-body-validation)

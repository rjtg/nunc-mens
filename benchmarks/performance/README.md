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

Every generated repository includes `queries.json` with the same conceptual prompts and unfiltered, directory, and exact-file variants. The fixture puts this file outside `docs`, so query text is never indexed as a source. Peak memory, per-stage profiling, and the scaling baseline are still tracked in #29.

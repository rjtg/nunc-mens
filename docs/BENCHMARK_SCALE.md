# Search performance baselines

This run measures search at 1,000, 10,000, 50,000, and 100,000 indexed chunks. Each size tests semantic, keyword, and hybrid retrieval with no source filter, a directory filter, and an exact-file filter. A conceptual question and an exact ticket identifier exercise each combination in two passes. The second pass follows one warm-up per mode. All requests launch a fresh process.

The release executable uses the optional profiling feature from `66aa281`. The host ran Linux x86-64 on an Intel Core i7-4710MQ with eight logical CPUs and 15.3 GiB RAM. Rust 1.98.1 built the binary. Both vector modes use the pinned multilingual E5-small ONNX model. The benchmark used a prepared model cache and left the system page cache undisturbed. Index construction was measured separately. The fixture repeats 256 distinct passage texts to make large index setup practical.

The complete matrix contains 144 query samples, 12 warm-ups, and four index builds. The release binary SHA-256 was `e46aab32e957f531deceb84dec655e79d6382c6108906c12053d160b906b057f`. Some build and test activity overlapped the smaller fixtures. The measurements describe this host and workload. [Raw measurements](../benchmarks/performance/results/standalone-linux-x86_64.jsonl.gz) · [Reproduction instructions](../benchmarks/performance/README.md#standalone-scale-benchmark)

## Standalone latency and index cost

Search entries below show first-pass and repeated-pass median wall time in seconds. Each median uses six requests spanning the three filter scopes. The summarizer emits the separate scope and phase tables with p50, p95, p99, serial throughput, RSS, and stage times. With two requests per scope and phase, the two high percentiles are observed maxima.

| Chunks | Semantic first / repeat | Keyword first / repeat | Hybrid first / repeat | Build s | Index MiB |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1,000 | 3.688 / 3.189 | 0.064 / 0.070 | 4.720 / 4.673 | 20.943 | 4.9 |
| 10,000 | 7.428 / 4.108 | 0.718 / 0.602 | 4.142 / 4.926 | 33.493 | 48.3 |
| 50,000 | 8.339 / 7.365 | 3.597 / 3.999 | 7.724 / 7.106 | 52.007 | 241.6 |
| 100,000 | 11.972 / 11.851 | 7.045 / 7.290 | 11.693 / 12.517 | 87.359 | 483.2 |

Peak RSS at 100,000 chunks was 1,448.6 MiB for semantic, 117.6 MiB for keyword, and 1,448.4 MiB for hybrid. The index size excludes model artifacts. Startup and teardown outside the CLI timer are approximate and include process launch and argument parsing.

## Where time went at 100,000 chunks

The table shows median stage times in seconds on the repeated pass. Scan timers include filtering and ranking. Medians are computed independently, so columns do not sum to the median wall time.

| Mode | Model load | Query embedding | Vector scan | Keyword scan | Fusion | Snapshot freshness |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Semantic | 3.344 | 0.016 | 0.433 | — | — | 7.275 |
| Keyword | — | — | — | 0.138 | — | 7.117 |
| Hybrid | 4.127 | 0.012 | 0.372 | 0.097 | <0.001 | 7.184 |

The snapshot check revisits registered sources even when result eligibility is limited to one file. An exact-ticket keyword query restricted to one file at 10,000 chunks returned one match and spent 809 ms in freshness validation and 63 ms in the lexical scan. At the larger sizes snapshot validation dominates all three modes on this corpus. Model initialization also remains a substantial cost in standalone semantic and hybrid search. The measurements give [#30](https://github.com/rjtg/nunc-mens/issues/30) a baseline for model reuse and [#32](https://github.com/rjtg/nunc-mens/issues/32) a baseline for reducing snapshot work. They do not establish a workload that needs ANN.

The repeated pass warms filesystem caches across independent invocations. The resident comparisons below measure ONNX reuse and application cache hits and misses. [Performance issue](https://github.com/rjtg/nunc-mens/issues/29)

## Resident semantic search at 1,000 chunks

A profiling-enabled release binary ran the unfiltered query “Verifying download integrity” against the same 1,000-chunk fixture on Linux x86-64. Five standalone invocations used independent processes. The socket series started a fresh `nunc serve --search-socket`, sent its first request, then sent five more requests from fresh CLI clients. Each response matched the standalone JSON byte for byte. [Raw measurements](../benchmarks/performance/results/socket-1000-linux-x86_64.jsonl) · [Reproduction instructions](../benchmarks/performance/README.md#resident-endpoint-comparison)

| Transport | Samples | Median wall time |
| --- | ---: | ---: |
| Standalone | 5 | 2.016 s |
| First socket request | 1 | 1.968 s |
| Warm socket requests | 5 | 0.050 s |

Socket publication took 21 ms. The first request waited for the watcher to initialize the shared model. Standalone model load took 1.803 s at the median. The warm server traces have no model-load stage, and they still record query embedding and freshness checks. The same process retained its SQLite connection across requests. These timings cover one query at one size, with warmed operating-system caches. The client peak RSS in the socket records excludes the resident server.

The profiling binary SHA-256 was `cd973ad777bbb1a307491ffade103e1022ef75f626bec7ea24f03cd855635e4d`.

### Query-vector cache probe

The 1,000-chunk semantic comparator used `--measure-cache` and sent a distinct primer before the first measured query. The primer and the warm miss each embedded a query. The next five exact repetitions returned the same standalone JSON and skipped embedding. The warm miss spent 8.312 ms in inference and 38.803 ms in the server. Hit median server time was 30.816 ms. Client wall time was 69.150 ms for the single warm miss and 49.901 ms at the hit median. One miss is enough to show that inference ran and gives a limited latency comparison. This workload exercises a query-vector cache. The server still performs index eligibility, visibility and snapshot validation on every request. [Raw cache comparison](../benchmarks/performance/results/cache-1000-linux-x86_64.jsonl)

The profiling binary SHA-256 was `18bde79beb4388a1a9a77f013e2a6af90d0f58d6c5ba02dcf142ced1b1cfb1c4`.

## Paired standalone and resident scale matrix

The paired run reused the generated repositories and their indexes. It refreshed each index separately, then ran one concept and one identifier in each of three source scopes. Each size and mode launched a fresh server. A distinct primer measured its first request. Each subsequent vector case had one warm cache miss and two exact hits. The keyword cases used three warm requests each and skipped query inference. Requests ran serially with the operating-system page cache left in place. The server's normal background index watcher remained active.

The profiling binary was built at `838c675` with SHA-256 `56b4f67b02f6f44d1cf996f3e1dc1eaf3408c3c5eb73ecc55ecf70218cd6a3d4`. It used the pinned multilingual E5-small model at `614241f622f53c4eeff9890bdc4f31cfecc418b3` on Linux 6.6.158, glibc 2.44, and the same eight-thread i7-4710MQ host with 15.3 GiB RAM. Cold first-request wall time includes any wait for the watcher to load the shared model. Standalone searches time model loading directly. Server peak RSS includes the watcher and its model even in the keyword cases.

Unfiltered median wall times are in seconds. The vector columns list standalone, warm miss, and warm hit. Keyword lists standalone and warm. Each standalone or miss median uses two requests, each hit median uses four, and each warm keyword median uses six.

| Chunks | Semantic | Keyword | Hybrid |
| ---: | ---: | ---: | ---: |
| 1,000 | 3.610 / 0.076 / 0.050 | 0.019 / 0.042 | 4.046 / 0.068 / 0.048 |
| 10,000 | 3.113 / 0.675 / 0.263 | 0.236 / 0.340 | 4.482 / 0.332 / 0.192 |
| 50,000 | 5.479 / 4.593 / 2.842 | 1.777 / 0.889 | 4.717 / 2.070 / 2.348 |
| 100,000 | 6.597 / 8.826 / 7.497 | 2.563 / 7.143 | 5.594 / 7.689 / 7.323 |

At 100,000 chunks, an unfiltered warm semantic hit spent a median 464 ms scanning vectors and 1.80 seconds checking freshness. Exact-file warm hits scanned vectors in about 0.11 ms and still took 7.11 seconds wall time. Warm keyword requests also show the larger latency. Four of six unfiltered requests spent 4.45–6.68 seconds in the index-read stage that acquires the ledger and index locks. The watcher takes both write locks and the shared model mutex while refreshing; the benchmark identifies contention around that path as the next investigation. Cache hits cut inference, while their wall times still vary with snapshot and refresh work.

The matrix recorded 72 standalone requests, 216 warm requests with byte-identical JSON to their standalone references, and 12 cold primers. Warm vector requests produced 48 misses and 96 exact hits. At 100,000 chunks, the three servers peaked at 1,451–1,474 MiB RSS. The 509 MiB index refreshed in 27.04 seconds with zero new embeddings. The full report separates these measurements, breaks latency and serial throughput down by scope, and includes p50, p95, p99, client RSS, startup, and stage medians. With this sample count, p95 and p99 report the observed maxima. [Raw measurements](../benchmarks/performance/results/paired-linux-x86_64.jsonl.gz) · [Reproduction instructions](../benchmarks/performance/README.md#resident-endpoint-comparison)

## Source freshness shortcut at 100,000 chunks

A controlled comparison ran the exact-file query `SPW-000000` against `docs/decisions/note-000000.md` on the same index. Each binary ran a fresh process per mode. An unchanged keyword-only reindex installed the source fingerprint used by the new binary. Both versions returned byte-identical JSON in every mode. [Raw measurements](../benchmarks/performance/results/freshness-100000-linux-x86_64.jsonl) · [Reproduction instructions](../benchmarks/performance/README.md#source-freshness-comparison)

| Mode | Search before | Search after | Freshness before | Freshness after |
| --- | ---: | ---: | ---: | ---: |
| Semantic | 5.097 s | 3.043 s | 2.957 s | 0.877 s |
| Keyword | 2.969 s | 1.032 s | 2.819 s | 0.885 s |
| Hybrid | 5.180 s | 3.160 s | 2.874 s | 0.886 s |

These are individual before/after samples from a later run of the same fixture, with both paths measured together. The new path still enumerates eligible source names and checks file metadata. A mismatch falls back to comparing the full content digest. Ranking and SQL plans stayed the same for this change.

The exact-path predicate `kind=? AND path=?` planned as a scan of `documents` before the path index. The chunk-ID lookup uses the primary-key index, the parent lookup uses `chunk_parent`, and the lexical `MATCH` uses the FTS virtual-table index. The next comparison measures exact-file filter pushdown.

The profiling binary SHA-256 values were `18bde79beb4388a1a9a77f013e2a6af90d0f58d6c5ba02dcf142ced1b1cfb1c4` before and `cfed3a68eca97171b08280e8ab1a5156d496a8cedc792a21a9b0e11edd645efc` after.

### Indexed exact-file scope

The next revision adds an index on `documents(path)` and binds explicit file unions before ranking. The same 100,000-chunk query returned byte-identical before and after JSON in three pairs per mode. The baseline already had the freshness shortcut. [Raw measurements](../benchmarks/performance/results/exact-file-100000-linux-x86_64.jsonl)

| Mode | Median total before / after | Vector scan before / after | Keyword scan before / after |
| --- | ---: | ---: | ---: |
| Semantic | 3.085 / 2.814 s | 192.329 / 0.115 ms | — |
| Keyword | 1.086 / 0.933 s | — | 146.088 / 2.484 ms |
| Hybrid | 3.203 / 2.863 s | 186.553 / 0.115 ms | 142.685 / 2.205 ms |

`EXPLAIN QUERY PLAN` on the fixture changed the exact equality predicate from `SCAN documents` to `SEARCH documents USING INDEX chunk_path (path=?)`. The bound file-list predicate also uses `chunk_path`, and lexical matching retains the FTS virtual-table plan. `--explain` still uses full type-scope traversal for its precise filter counts. Directory and mixed scopes keep the component-aware filter. Freshness still checks about 0.9 seconds of source metadata at this size. Reads after a change still fall back to the content digest.

The after binary SHA-256 was `f44b47301a0d84cfe767ab01b889111b20b6ac3d35e249210c6207ac2d951c13`.

### Changed source metadata

A follow-up touched the exact source file's mtime while preserving its contents. The previous build would confirm the unchanged result by rereading the full corpus. The source manifest allowed the new build to confirm it by rereading the file with the changed stamp. A unit test counts the bodies reopened during a manifest comparison. [Touch samples](../benchmarks/performance/results/source-manifest-touch-100000-linux-x86_64.jsonl) · [Unchanged samples](../benchmarks/performance/results/source-manifest-steady-100000-linux-x86_64.jsonl)

| Mode | Freshness after touch, prior build | Freshness after touch, manifest |
| --- | ---: | ---: |
| Semantic | 3.166 s | 1.307 s |
| Keyword | 3.078 s | 1.415 s |
| Hybrid | 3.184 s | 1.426 s |

All three modes returned byte-identical JSON for both the touch and unchanged comparisons. Unchanged-source freshness moved from 0.863–0.888 s to 0.890–0.947 s in these single pairs. The SQLite manifest occupied 22.2 MiB of pages on this 100,000-chunk fixture and is decoded only when the aggregate fingerprint cannot confirm freshness. Search still hashes the journal. These measurements are from the same host and corpus as the earlier comparisons.

The after binary SHA-256 was `f558d0d8f1cf0a0219b7e65680cf7c8312e2df6dec2cde66ccc8c55173a9df05`.

### Indexed directory and mixed scopes

The directory probe searched for `staging` inside `docs/decisions` in the same 100,000-chunk index. The new scope combines indexed equality and range lookups across file, directory and memory filters. The range plan reports `SEARCH d USING COVERING INDEX chunk_path (path>? AND path<?)`. The bounds include descendants after the slash and exclude neighboring names with a shared prefix. The eligibility list is shared by the vector and FTS branches. Both versions returned byte-identical JSON in three pairs per mode. [Directory samples](../benchmarks/performance/results/directory-scope-100000-linux-x86_64.jsonl)

| Mode | Median total before / after | Vector scan before / after | Keyword scan before / after |
| --- | ---: | ---: | ---: |
| Semantic | 3.109 / 3.009 s | 221.214 / 100.733 ms | — |
| Keyword | 1.288 / 1.164 s | — | 261.636 / 176.483 ms |
| Hybrid | 3.562 / 3.416 s | 236.898 / 108.099 ms | 273.983 / 179.793 ms |

A separate exact-file control still returned byte-identical JSON across all modes. Its semantic vector scan took 0.113 ms before and 0.224 ms after. Its keyword scan took 2.479 ms before and 4.471 ms after. These are individual samples of the added scope-union overhead. [Exact-file control](../benchmarks/performance/results/directory-exact-control-100000-linux-x86_64.jsonl)

The profiling binary SHA-256 was `7336a6de7517ebdac91bb8cad8e576c612b42e37da52ed517233a3800d0c3c9d`.

### Journal body validation

The journal-only fixture contains 2,000 deterministic Markdown notes and 10,000 indexed chunks. Each binary used its own state copy and searched the same journal for `WriterLock`. Three before/after pairs per mode returned byte-identical JSON. This comparison isolates the journal stamp change from the earlier source optimizations. [Raw measurements](../benchmarks/performance/results/journal-2000-linux-x86_64.jsonl) · [Reproduction instructions](../benchmarks/performance/README.md#journal-freshness-probe)

| Mode | Median freshness before / after | Median total before / after |
| --- | ---: | ---: |
| Semantic | 134.416 / 18.035 ms | 2.165 / 2.092 s |
| Keyword | 146.324 / 16.751 ms | 0.213 / 0.082 s |
| Hybrid | 137.946 / 23.669 ms | 2.238 / 2.221 s |

Freshness still enumerates and checks entry metadata. The body and front matter are parsed only for new, changed or unreliable entries. Summary eligibility is checked against the publish ledger. Older indexes keep the full digest comparison until reindexing refreshes the auxiliary metadata.

Both lookup paths use the existing read-only SQLite connection and repository locks. Temporal filtering and archive/forget visibility run before limiting, with the ledger authoritative for visibility. Parent and chunk retrieval use their existing indexes. The per-stage data point to snapshot work and eligible scans ahead of SQLite pragma changes.

The profiling binary SHA-256 was `56b4f67b02f6f44d1cf996f3e1dc1eaf3408c3c5eb73ecc55ecf70218cd6a3d4`.

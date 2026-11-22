# Standalone search performance baseline

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

The repeated pass warms filesystem caches across independent invocations. Persistent-process latency and application cache hits and misses remain unmeasured until those paths exist. [Performance issue](https://github.com/rjtg/nunc-mens/issues/29)

## Resident semantic search at 1,000 chunks

A profiling-enabled release binary ran the unfiltered query “Verifying download integrity” against the same 1,000-chunk fixture on Linux x86-64. Five standalone invocations used independent processes. The socket series started a fresh `nunc serve --search-socket`, sent its first request, then sent five more requests from fresh CLI clients. Each response matched the standalone JSON byte for byte. [Raw measurements](../benchmarks/performance/results/socket-1000-linux-x86_64.jsonl) · [Reproduction instructions](../benchmarks/performance/README.md#resident-endpoint-comparison)

| Transport | Samples | Median wall time |
| --- | ---: | ---: |
| Standalone | 5 | 2.016 s |
| First socket request | 1 | 1.968 s |
| Warm socket requests | 5 | 0.050 s |

Socket publication took 21 ms. The first request waited for the watcher to initialize the shared model. Standalone model load took 1.803 s at the median. The warm server traces have no model-load stage, and they still record query embedding and freshness checks. The same process retained its SQLite connection across requests. These timings cover one query at one size, with warmed operating-system caches. The client peak RSS in the socket records excludes the resident server.

The profiling binary SHA-256 was `cd973ad777bbb1a307491ffade103e1022ef75f626bec7ea24f03cd855635e4d`.

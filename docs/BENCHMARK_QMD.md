# Retrieval comparison with QMD

This evaluation asks which retrieval modes find the relevant project record and return the evidence a caller needs. The [reproduction instructions](../benchmarks/retrieval/README.md) include the fixture, judgments, commands, and scoring rules.

## Corpus and experiment

Sparrow is a fictional queue and deployment service. The corpus contains 16 Markdown bodies, including a Git-imported commit, a duplicate, an archived prototype, and a long runbook. Seventeen queries cover paraphrases, identifiers and paths, historical revisions, long-document context, and lifecycle behavior. Each query has one or more judged documents. Most also identify an exact evidence phrase. The two tools see the same bodies. For ordinary retrieval, the QMD collection excludes the archived prototype. The raw probe uses an independent QMD collection containing it and uses `nunc search --raw`.

| Configuration | Retrieval path |
| --- | --- |
| nunc default | Structural chunks with a 384-token budget, E5 cosine, chunk output, recency disabled |
| nunc context ablations | Same index and ranks with section or full-parent output |
| nunc recency | Default search plus a maximum score contribution of 0.05 and a 90-day half-life |
| nunc chunking ablations | Semantic boundary splitting at 384 tokens, or structural splitting at 128 |
| QMD lexical | `qmd search`, the native BM25 path |
| QMD vector | Typed `vec:` query with the tool's native vector ranking and no expansion or rerank |
| QMD hybrid | Typed `lex:` and `vec:` queries with native fusion and no expansion or rerank |

Each query returns up to 10 slots. A second chunk from the same document consumes a slot but does not earn a second relevance gain. Evidence@5 checks the delivered text for a judged phrase, so a correct document can rank first without supplying the required passage. Recall counts grades 1 and 3 equally, and nDCG weights them differently.

The performance sample uses Linux x86-64 (Manjaro kernel 6.6.158-1), an Intel Core i7-4710MQ with eight logical CPUs and approximately 15.3 GiB of RAM. Both tools ran on CPU. nunc was a release build at `0ab8ec4`, built using Rust 1.98.1 and the locked FastEmbed 5.15.0 dependency. QMD was 2.9.0 (`aa247d0`) under Node 26.10.0 with node-llama-cpp 3.22.3. The E5 and EmbeddingGemma weights, tokenizers, chunking, and native runtimes differ. The model files were prepared before timing. nunc's model cache occupied about 465 MiB. QMD's pinned Q8_0 embedding file occupied about 318 MiB.

First-pass and repeated timings use fresh CLI processes after indexing, with the OS page cache undisturbed. Each QMD case starts in a separate copy of the index to isolate persistent query caches. Wall times include CLI startup, inference when applicable, retrieval, and output, and RSS is the Linux wait4 high-water value. Database sizes count the main files and exclude model artifacts. First-pass median and p95 describe this workload on this host. Server warm-path and scale tests belong to [#29](https://github.com/rjtg/nunc-mens/issues/29).

QMD's free-text `query` can use optional local generation and reranker weights. Those paths were outside this measurement. The runner accepts local models for expanded and reranked experiments.

## Retrieval results

The matrix finished 306 invocations across nine modes and two rounds. All 153 mode/query pairs returned the same ordered document labels on repeat.

| Mode | Recall@1 | Recall@5 | Recall@10 | MRR@10 | nDCG@10 | Evidence@5 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| nunc default | 0.618 | 1.000 | 1.000 | 0.882 | 0.910 | 1.000 |
| nunc section | 0.618 | 1.000 | 1.000 | 0.882 | 0.910 | 1.000 |
| nunc parent | 0.618 | 1.000 | 1.000 | 0.882 | 0.910 | 1.000 |
| nunc recency | 0.559 | 1.000 | 1.000 | 0.824 | 0.841 | 0.875 |
| nunc semantic chunks | 0.618 | 1.000 | 1.000 | 0.882 | 0.910 | 1.000 |
| nunc 128-token chunks | 0.618 | 1.000 | 1.000 | 0.882 | 0.910 | 0.938 |
| QMD lexical | 0.412 | 0.412 | 0.412 | 0.412 | 0.412 | 0.312 |
| QMD vector | 0.824 | 1.000 | 1.000 | 1.000 | 0.997 | 0.812 |
| QMD hybrid | 0.824 | 1.000 | 1.000 | 1.000 | 0.997 | 0.812 |

Each cell below shows Recall@1 and nDCG@10 in that order. The full category tables for every metric and configuration can be rendered from the [raw measurements](../benchmarks/retrieval/results/sparrow-v1.jsonl.gz) with `summarize.py`.

| Category | Queries | nunc default | QMD lexical | QMD vector | QMD hybrid |
| --- | ---: | ---: | ---: | ---: | ---: |
| Semantic | 3 | 0.333 / 0.863 | 0.000 / 0.000 | 0.667 / 1.000 | 0.667 / 1.000 |
| Exact | 5 | 0.600 / 0.852 | 1.000 / 1.000 | 1.000 / 1.000 | 1.000 / 1.000 |
| History | 3 | 0.500 / 0.881 | 0.000 / 0.000 | 0.667 / 0.994 | 0.667 / 0.994 |
| Long context | 3 | 0.833 / 0.991 | 0.333 / 0.333 | 0.833 / 0.991 | 0.833 / 0.991 |
| Lifecycle | 3 | 0.833 / 1.000 | 0.333 / 0.333 | 0.833 / 1.000 | 0.833 / 1.000 |

- The filename and full commit SHA each landed at rank two in nunc and at rank one in all three QMD modes. The QMD vector result also ranked the intended record first for every conceptual query. These observations include differences in models, metadata, chunking, and ranking, so the gap cannot be assigned to lexical indexing alone.
- QMD vector and hybrid returned the correct long runbook first for all three long-context queries. Their returned snippets supplied the judged evidence for one of those queries. Both nunc configurations with the default structural budget or semantic splitting supplied it for all three. In particular, the first QMD runbook hit for the replay question omitted the reset command. A caller could fetch the larger document using its provenance.
- For the first trial policy, nunc put a TLS issuer document ahead of the correct API-token history. QMD vector and hybrid ranked the original policy first. Explicit history filters in [#14](https://github.com/rjtg/nunc-mens/issues/14) are a separate way to make older records easier to target.
- In the [supplemental exact-flag probe](../benchmarks/retrieval/results/sparrow-technical.jsonl.gz), QMD lexical returned no result for `--from-checkpoint`. The other three modes ranked the runbook first. The QMD vector and hybrid snippets included the reset command, while the nunc chunks omitted it in the first five slots. Exact-term support should cover punctuation as well as filenames and IDs.

## nunc ablations

- Section and parent contexts preserved ranking and did not improve Evidence@5 over the default chunks. For the three long-context queries, median JSON size rose from 11,721 bytes with chunk output to 51,716 bytes with parent output. The ten-slot response can return a long parent multiple times.
- Semantic splitting at 384 tokens and structural splitting at 128 tokens had the same aggregate ranking metrics as the default. Reducing the budget to 128 lost the long-runbook backlog evidence from the first five slots. This run does not justify smaller default chunks or semantic splitting by default.
- The 0.05 recency boost raised history Recall@1 from 0.500 to 0.667. It lowered overall nDCG@10 from 0.910 to 0.841 and long-context nDCG@10 from 0.991 to 0.868. The boost was evaluated against the host's wall clock on 2026-11-21. Keep recency opt-in.

## Runtime and storage

| Mode | First p50 s | First p95 s | Repeat p50 s | Repeat p95 s | Max RSS MiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| nunc default | 1.972 | 2.144 | 1.997 | 2.224 | 1,448.3 |
| nunc section | 2.036 | 3.100 | 2.103 | 3.213 | 1,448.1 |
| nunc parent | 2.313 | 3.154 | 2.129 | 2.869 | 1,448.1 |
| nunc recency | 2.145 | 3.391 | 2.220 | 3.394 | 1,448.3 |
| nunc semantic chunks | 2.098 | 3.373 | 2.122 | 3.265 | 1,448.1 |
| nunc 128-token chunks | 2.135 | 3.616 | 2.268 | 3.705 | 1,448.1 |
| QMD lexical | 0.311 | 0.413 | 0.183 | 0.333 | 78.4 |
| QMD vector | 3.140 | 4.112 | 3.141 | 3.950 | 636.5 |
| QMD hybrid | 3.084 | 3.915 | 3.221 | 3.885 | 637.0 |

| Layout | Full index s | One-document update s | Full index peak RSS MiB | Full database MiB |
| --- | ---: | ---: | ---: | ---: |
| nunc default | 2.876 | 1.963 | 1,447.6 | 0.176 |
| nunc semantic chunks | 6.519 | 2.324 | 1,447.6 | 0.176 |
| nunc 128-token chunks | 3.553 | 2.233 | 1,447.4 | 0.180 |
| QMD lexical, active | 0.210 | 0.368 | 81.4 | 0.246 |
| QMD lexical and vector, active | 11.143 | 3.632 | 656.2 | 3.281 |

One-document update appends the same sentence in both tools. QMD measurements separate `collection add` or `update` from `embed`. The combined row adds wall times and uses the larger measured RSS of those stages. Its incremental lexical command ran against an already embedded collection. The nunc index includes the archived document and QMD's active index does not. The raw QMD index also occupied 3.281 MiB after embedding. These numbers can be sensitive to native startup, allocator overhead, and host load at this corpus size.

## Recommendation

Keep semantic search and structural chunks as nunc defaults. For this repository-sized fixture, default nunc found every judged document within five slots and returned evidence for every evidence-judged query. Exact rank-one misses and the large latency gap between lexical and inference modes justify prototyping optional keyword and literal metadata retrieval in [#9](https://github.com/rjtg/nunc-mens/issues/9). Prioritize path, SHA, and punctuation-heavy terms. QMD vector and controlled hybrid tied across every measured quality metric, so the study does not justify a hybrid default or a reranker.

Measure attribution-aware context budgets next in [#11](https://github.com/rjtg/nunc-mens/issues/11). Retrieved text size can grow more than fourfold with parent expansion even when evidence coverage is unchanged. Use [#29](https://github.com/rjtg/nunc-mens/issues/29) to determine which startup, snapshot, and retrieval costs dominate at scale before adding optimizations. The corpus is small, fictional, and English. Quantization, model, snippet defaults, and index implementations differ. Further multilingual or large-corpus evaluation is needed before making broader quality or performance claims.

## Follow-up on context budgets

For [#11](https://github.com/rjtg/nunc-mens/issues/11), the release CLI ran the three long-context queries twice on the same host and fixture, using the same index for each mode. All searches requested parent context and ten result slots. The budgeted runs returned at most one window per parent.

| Mode | Median JSON bytes | Recall@5 | Judged evidence in top 5 | First p50 | Repeat p50 |
| --- | ---: | ---: | ---: | ---: | ---: |
| Unbudgeted | 51,716 | 1.000 | 3 of 3 | 2.075 s | 2.070 s |
| 800 estimated tokens | 3,383 | 0.833 | 2 of 3 | 3.024 s | 2.292 s |
| 2,000 estimated tokens | 8,221 | 1.000 | 3 of 3 | 2.186 s | 2.167 s |

The 2,000-token run kept all judged evidence and reduced median JSON bytes by 84%. It returned five distinct parents for each query. The 800-token run missed the backlog threshold. Its response used more bytes than four times the estimated budget because the policy excludes the JSON response envelope. All budgeted results stayed within their charged estimate. The 2,000-token sample took about 0.1 s longer at the median. There were only three timings per pass, and startup dominated, so the sample is too small to isolate the allocator's latency. [Raw budget runs](../benchmarks/retrieval/results/sparrow-context-budget.jsonl.gz)

## Follow-up on keyword and hybrid retrieval

Issue [#9](https://github.com/rjtg/nunc-mens/issues/9) compares the three nunc retrieval modes on the same 17 labeled Sparrow queries, with a fourth configuration that has no model cache. The regular semantic, keyword, and hybrid cases share one structural, tokenizer-based index. The offline keyword case uses byte-budget structural chunks and its own index. The supplemental flag uses the same separately versioned query as the original QMD run.

The runner uses a fresh CLI process for each query following index construction. This follow-up measures the local feature and uses the earlier QMD run as reference. The release build, exact binary hash, per-query results, and platform are recorded in the raw benchmark data. [Reproduction commands](../benchmarks/retrieval/README.md)

The first pass exposed a gap in keyword provenance lookup. The prompt was `design/patch-review.md`, relative to QMD's collection root. nunc indexed the file as `docs/design/patch-review.md`. After adding component-aligned source path suffix matching, the same prompt resolved to that canonical path. The final comparison uses the original judgments and query text.

The release build from `b5b4be0` ran 68 judged queries and four supplemental flag queries. The standalone repeats from the first pass used the prototype with the path gap. The final results below use one corrected pass on Linux x86-64 with the same Intel i7-4710MQ CPU, Rust 1.98.1, and pinned E5 model as the original comparison. [Main measurements](../benchmarks/retrieval/results/sparrow-nunc-modes.jsonl.gz) · [Flag measurements](../benchmarks/retrieval/results/sparrow-nunc-modes-technical.jsonl.gz)

| Mode | Recall@1 | Recall@5 | Recall@10 | MRR@10 | nDCG@10 | Evidence@5 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Semantic | 0.618 | 1.000 | 1.000 | 0.882 | 0.910 | 1.000 |
| Keyword | 0.735 | 0.941 | 0.941 | 0.917 | 0.853 | 0.938 |
| Hybrid | 0.765 | 0.941 | 1.000 | 0.971 | 0.918 | 0.938 |
| Keyword, model-free index | 0.735 | 0.941 | 0.941 | 0.917 | 0.853 | 0.938 |

- The default returned the same ordered document labels for all 17 queries as the earlier comparison. Keyword and hybrid ranked all five exact targets first. Semantic ranked three first and placed the path and full SHA second.
- All four configurations ranked the right runbook first for `--from-checkpoint`. Keyword and hybrid supplied the reset command in their first hit, at lines 37–48. Semantic's first hit pointed to lines 5–8 and the first five results omitted the command.
- Hybrid improved overall MRR. Semantic retained perfect Recall@5 and Evidence@5. Hybrid lost part of the judged auth incident evidence and returned several chunks of the same runbook before a supporting record on a long-context query. Keep semantic as the default on the current evidence.

| Mode | Process p50 s | Process p95 s | Max RSS MiB |
| --- | ---: | ---: | ---: |
| Semantic | 3.978 | 5.721 | 1448.5 |
| Keyword | 0.014 | 0.058 | 25.1 |
| Hybrid | 3.637 | 5.737 | 1450.4 |
| Keyword, model-free index | 0.019 | 0.115 | 25.1 |

The timings include CLI startup and use a fresh process per query with the system page cache undisturbed. This small serial run cannot resolve a steady-state latency difference between semantic and hybrid. Keyword queries skip model initialization.

| Index | Full rebuild s | One-file refresh s | SQLite size KiB |
| --- | ---: | ---: | ---: |
| Shared vectors and keyword terms | 6.388 | 5.338 | 220 |
| Offline keyword | 0.030 | 0.139 | 132 |

Both layouts stored 32 chunks. The shared layout embedded 31 unique passages on build and one on refresh. The keyword-only layout embedded zero. SQLite's `dbstat` attributed 32 KiB in each final file to FTS5 tables and the readiness table (`name GLOB 'chunk_terms*' OR name = 'retrieval_state'`). The original vector-only comparison recorded a 180 KiB full database on the same corpus. File sizes include page allocation and unused space. The timings and sizes exclude model artifacts.

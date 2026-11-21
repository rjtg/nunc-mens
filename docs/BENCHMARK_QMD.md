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

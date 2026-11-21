# Retrieval benchmark

The fixtures describe Sparrow, a fictional queue and deployment service. Version 1 has 16 documents and 17 labeled queries covering concepts, exact terms, history, long context, duplicates, and archive visibility. Grades 3 and 1 denote directly relevant and supporting documents. Unlisted documents have grade 0. Optional evidence strings identify text that can be checked in returned context.

The fixture builder writes an isolated Git repository and two QMD collections. It seeds Markdown memories with fixed IDs and timestamps, imports a real deterministic Git commit through `nunc git ingest`, and archives the retired prototype. QMD receives the same Markdown bodies, including the body captured by Git ingestion. Its ordinary collection omits the archived document, and its raw collection includes it. `paths.json` maps both tools' paths to the shared judgment labels and records the SHA used in the exact-commit query.

The comparison targets QMD 2.9.0 (`aa247d0`) and the evaluated nunc source revision. The fixture builder needs Python 3.9+ and Git. Timing uses Linux's `wait4`. QMD needs Node 22 or newer. Prepare the models in explicit, separate steps before timing inference.

- nunc uses the project's pinned multilingual E5-small ONNX model. Prepare it with `nunc models download`.
- QMD's default embedding weights are [EmbeddingGemma Q8_0](https://huggingface.co/ggml-org/embeddinggemma-300M-GGUF/tree/0f741b5a6585bd53aeb15cd1372c56f2a0f65e12) at revision `0f741b5a6585bd53aeb15cd1372c56f2a0f65e12`. The downloaded file had SHA-256 `b5ce9d77a3fc4b3b39ccb5643c36777911cc4eb46a66962eadfa3f5f60490d63`. QMD can load it from an absolute path given in `QMD_EMBED_MODEL`.

```sh
cargo build --release --locked
scratch=$(mktemp -d)
export NUNC_MODEL_CACHE_DIR="$scratch/e5"
target/release/nunc models download
npm install --prefix "$scratch/qmd" @tobilu/qmd@2.9.0
curl --fail --location --output "$scratch/embeddinggemma.gguf" \
  'https://huggingface.co/ggml-org/embeddinggemma-300M-GGUF/resolve/0f741b5a6585bd53aeb15cd1372c56f2a0f65e12/embeddinggemma-300M-Q8_0.gguf'
python3 -B benchmarks/retrieval/benchmark.py \
  --root "$scratch/fixture" --output "$scratch/results.jsonl" \
  --nunc target/release/nunc --nunc-cache "$NUNC_MODEL_CACHE_DIR" \
  --qmd "$scratch/qmd/node_modules/.bin/qmd" --qmd-embed-model "$scratch/embeddinggemma.gguf"
python3 -B benchmarks/retrieval/summarize.py "$scratch/results.jsonl"
python3 -B -m unittest discover -s benchmarks/retrieval -v
```

Run the comparison on an otherwise quiet host. The harness serializes runs, uses a fresh fixture and isolated index directories, forces QMD to use CPU inference, and checks its version and embedding weights. Add `--case` and `--query` repeatedly for smaller smoke runs. `prepare.py --root PATH --nunc BINARY` materializes the fixture without timing it.

The main matrix includes nunc's default semantic search, section and parent output, a 0.05 recency boost, semantic splitting, and a smaller 128-token structural layout. QMD runs BM25 (`search`), a structured `vec:` query, and a structured `lex:` plus `vec:` query, with query expansion and reranking disabled for the two structured cases. The QMD and nunc vector cases use their native models, indexes, and snippets. They control the presence of lexical retrieval and reranking only. The measured QMD hybrid is fusion of the two supplied expressions, with no model-generated query expansion. To measure the optional structured reranker, pass an absolute path to a local QMD reranker with `--qmd-rerank-model`. Also pass `--qmd-generate-model` to include QMD's full free-text `query` pipeline. The latter can have substantial additional cost on CPU.

Each CLI returns ten ranked slots. Repeated chunks from the same parent still occupy slots, and only the first slot earns relevance gain. Recall measures the fraction of judged relevant documents retrieved, MRR counts the first relevant hit, and nDCG gives more credit to grade 3 than grade 1. Evidence@5 is the fraction of evidence-judged queries where a checked phrase appears verbatim in one of the first five responses. This measures whether the response makes the specific fact available to a caller. It cannot measure understanding or credit paraphrases. QMD's default snippets and nunc's chosen context mode determine the returned text.

First-pass timing starts after index construction, with a fresh CLI process for every query and no prior benchmark query cache for that mode. Repeat timing uses the same queries and database with another fresh CLI process for each request. The operating-system page cache is undisturbed. Repeats can benefit from filesystem and any persistent query caches. Neither measures a resident server or a machine boot. Raw JSONL includes timings, peak RSS, result labels and lines, response size, metrics, and commands. Database bytes count main SQLite files after the relevant command. Model bytes, download times, initialization of the fixture, and fetch or build costs are separate from search and indexing times.

The [report](../../docs/BENCHMARK_QMD.md) gives the recorded results. Render the full category tables with `python3 -B benchmarks/retrieval/summarize.py benchmarks/retrieval/results/sparrow-v1.jsonl.gz`. Pass `--query-file benchmarks/retrieval/queries-technical.json --rounds 1 --case nunc-default --case qmd-lexical --case qmd-vector --case qmd-hybrid` in a separate scratch directory to reproduce the supplemental exact-flag probe.

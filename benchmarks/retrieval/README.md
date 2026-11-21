# Retrieval benchmark fixtures

The fixtures describe Sparrow, a fictional queue and deployment service. Version 1 has 16 documents and 17 labeled queries covering concepts, exact terms, history, long context, duplicates, and archive visibility. Grades 3 and 1 denote directly relevant and supporting documents. Unlisted documents have grade 0. Optional evidence strings identify text that can be checked in returned context.

The fixture builder writes an isolated Git repository and two QMD collections. It seeds Markdown memories with fixed IDs and timestamps, imports a real deterministic Git commit through `nunc git ingest`, and archives the retired prototype. QMD receives the same Markdown bodies, including the body captured by Git ingestion. Its ordinary collection omits the archived document, and its raw collection includes it. `paths.json` maps both tools' paths to the shared judgment labels and records the SHA used in the exact-commit query.

```sh
cargo build --release --locked
scratch=$(mktemp -d)
python3 benchmarks/retrieval/prepare.py --root "$scratch/corpus" --nunc target/release/nunc
```

The comparison targets QMD 2.9.0 (`aa247d0`) and the evaluated nunc source revision. The fixture builder needs Python 3.9+ and Git. QMD needs Node 22 or newer. Prepare the models in explicit, separate steps before timing inference.

- nunc uses the project's pinned multilingual E5-small ONNX model. Prepare it with `nunc models download`.
- QMD's default embedding weights are [EmbeddingGemma Q8_0](https://huggingface.co/ggml-org/embeddinggemma-300M-GGUF/tree/0f741b5a6585bd53aeb15cd1372c56f2a0f65e12) at revision `0f741b5a6585bd53aeb15cd1372c56f2a0f65e12`. The downloaded file had SHA-256 `b5ce9d77a3fc4b3b39ccb5643c36777911cc4eb46a66962eadfa3f5f60490d63`. QMD can load it from an absolute path given in `QMD_EMBED_MODEL`.

This directory currently contains the corpus and setup command. The full comparison, timing harness and results are tracked in [issue #8](https://github.com/rjtg/nunc-mens/issues/8).

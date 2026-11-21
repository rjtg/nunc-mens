# Implementation roadmap

The MVP implementation follows [the implementation plan](IMPLEMENTATION_PLAN.md). The verified command outputs and progressive development use are in [the dogfooding report](DOGFOODING.md).

| Milestone | Deliverable |
| --- | --- |
| C–D | Git/standalone scope, configuration and durable one-file Markdown capture |
| E | Foreground FIFO, byte-preserving framing, cooperative clients and restart recovery |
| F | Read-only source snapshots, incremental SQLite vectors and staged full rebuild |
| G | Explicit pinned ONNX download, local FastEmbed inference and cosine-ranked search |
| H | Automatic polling refresh, index diagnostics, CLI documentation and CI |
| I | Real executable verification with a development conversation journal and source changes |

Subsequent milestones are tracked in [the M5–M9 plan](M5_M9_PLAN.md).

| Issue | Deliverable |
| --- | --- |
| [#2](https://github.com/rjtg/nunc-mens/issues/2) | Structural and optional semantic chunks with section and parent context |
| [#3](https://github.com/rjtg/nunc-mens/issues/3) | Recency ranking and explanations |
| [#4](https://github.com/rjtg/nunc-mens/issues/4) | Exact and semantic duplicate discovery |
| [#5](https://github.com/rjtg/nunc-mens/issues/5) | Shared Git commit ingestion, history import and an optional hook |
| [#6](https://github.com/rjtg/nunc-mens/issues/6) | Recoverable consolidation, archived search and undo |
| [#7](https://github.com/rjtg/nunc-mens/issues/7) | Explicit forget with dependency warnings and tombstones |

Possible later work includes an event-driven watcher for larger repositories, packaged binaries and larger-corpus performance measurements.

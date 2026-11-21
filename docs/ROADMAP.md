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

Possible later work includes an event-driven watcher for larger repositories, context-aware splitting for long memories, packaged binaries and measured search/indexing scale limits. Alternative retrieval modes and additional adapters are outside this MVP.

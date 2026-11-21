# Repository-local FIFO and sources

## Lifecycle

```text
.nunc/
  .gitignore    # local collection and runtime ignore rules
  config.toml   # optional repository-relative source registrations
  entries/      # authoritative Markdown, one file per complete note
  input.fifo    # owner-only live endpoint
  session.json  # version, ID, boundary, PID, FIFO device and inode
  *.lock        # advisory lock files
```

A single server holds a worktree ownership lock for its whole run. It verifies the type and ownership of stale runtime artifacts before removing them. Runtime objects use owner-only permissions and the runtime directories reject symbolic links and directories writable by other users. The foreground server prints the FIFO path and boundary on startup. SIGINT or SIGTERM unpublishes the endpoint, drains the already open pipe for up to one second, and lets an active index transaction finish. A forced exit can leave endpoints and locks on disk, which the next server handles. Existing session data has no effect when its server lock is free.

## Framing

The default boundary is a random UUID v4 rendered as ASCII without hyphens. `--boundary` accepts 1–70 ASCII letters, digits, hyphens or underscores. The token is a delimiter. Filesystem permissions control access.

A complete line `--<boundary>` with LF or CRLF ends a frame. Neither the delimiter nor its line ending enters the saved body. Every other byte in a raw input frame is preserved, including the LF or CRLF preceding the delimiter. Multiple records and arbitrary read splits are accepted. Start `nunc serve --boundary demo` from the repository before sending this example.

````sh
cat > .nunc/input.fifo <<'NOTE'
Versioned cache keys let old deployments finish.

```text
A fenced block is still part of the note.
```
--demo
NOTE
````

Empty, invalid UTF-8, NUL-containing or oversized frames are rejected. An oversized frame is discarded up to the next delimiter. Writer EOF discards an unfinished frame when the reader observes the end of the stream. A line exactly equal to the delimiter always ends a raw frame, even inside a code fence. A CLI frame starts with `!` plus LF and a reserved line `nunc-client-frame:<boundary>`. The first byte breaks a possible partial delimiter and the control line discards any abandoned record even if the FIFO reader has yet to observe EOF. The CLI refuses note text that collides with a reserved control line. The first body line `nunc-no-final-lf:<boundary>` is also reserved for the CLI. It indicates that the added LF just before the delimiter should be removed to preserve the note's original final byte.

## Delivery

`nunc write` validates framing, acquires the cooperative cross-process writer lock and opens the FIFO nonblocking. It checks the current session and the opened FIFO identity before sending. If a reader is unavailable and no bytes were sent, it falls back to direct capture. It reports a bounded delivery failure if a partial transfer cannot complete. A current client falls back to direct capture while a version 1 server still owns the endpoint. `nunc log` always saves directly.

A FIFO is a byte stream. Raw concurrent writers must coordinate externally unless each entire frame is sent with one write within the platform's `PIPE_BUF`. Raw writers share the parser and must finish a record before releasing their writer slot. Client delivery confirms a pipe write. A durable receipt comes from direct capture or a separately observed saved file. An indexing failure leaves saved Markdown in place.

## Source selection

Register files and directories with `nunc init --source`, `nunc serve --source`, `nunc reindex --source`, or the config file. The settings affect the repository regardless of the directory from which `nunc` is invoked. Git worktrees use tracked and nonignored untracked paths according to Git's exclusion rules. Standalone roots apply repo-local ignore rules. Overlapping sources are deduplicated. Symlinks, special files, internal paths (`.git`, `.nunc`, `target`, `node_modules`, `.venv`), binary/NUL-containing content, non-UTF-8 text or paths, and files larger than 2 MiB are skipped. Explicit source paths must stay inside the worktree and must exist when registered. A tracked file remains eligible when matched by a later Git ignore rule.

The server performs an initial refresh and polls for modifications and deletions every two seconds after the preceding pass. It rereads the config, source files and entries. Inference runs separately from FIFO capture. With the model absent, indexing is pending and capture continues. `nunc reindex` is available when the server is stopped.

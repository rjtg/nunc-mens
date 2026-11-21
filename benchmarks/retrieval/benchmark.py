"""Run the shared, locally indexed nunc and QMD retrieval corpus on Linux."""

import argparse
import contextlib
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tempfile
import time

from prepare import HERE, prepare


NUNC_CASES = {
    "nunc-default": [],
    "nunc-section": ["--context", "section"],
    "nunc-parent": ["--context", "parent"],
    "nunc-recency": ["--recency-boost=0.05"],
    "nunc-semantic-chunks": [],
    "nunc-small-chunks": [],
}
QMD_CASES = (
    "qmd-lexical", "qmd-vector", "qmd-hybrid", "qmd-hybrid-reranked", "qmd-default",
)
QMD_MODEL_SHA256 = "b5ce9d77a3fc4b3b39ccb5643c36777911cc4eb46a66962eadfa3f5f60490d63"


def read_json(path):
    return json.loads(path.read_text(encoding="utf-8"))


def measure(command, cwd, env):
    """Capture Linux wait4 peak RSS for each CLI invocation."""
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        started = time.perf_counter()
        with subprocess.Popen(
            command, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr,
        ) as child:
            try:
                _, status, usage = os.wait4(child.pid, 0)
            except KeyboardInterrupt:
                child.terminate()
                raise
            child.returncode = os.waitstatus_to_exitcode(status)
            seconds = time.perf_counter() - started
        stdout.seek(0)
        stderr.seek(0)
        output = stdout.read()
        if child.returncode:
            raise RuntimeError(
                f"{command!r} exited {child.returncode}\n"
                f"{stderr.read().decode('utf-8', errors='replace')}\n"
                f"{output.decode('utf-8', errors='replace')}"
            )
        return output, {"seconds": seconds, "peak_rss_kib": usage.ru_maxrss}


def total_bytes(root):
    return sum(
        path.stat().st_size for path in root.rglob("*")
        if path.is_file() and path.suffix in {".sqlite", ".sqlite3", ".db"}
    )


def duplicate_safe_metrics(hits, grades):
    # Duplicate chunks consume result slots, but only the first hit earns relevance gain.
    gains = []
    seen = set()
    for hit in hits[:10]:
        doc = hit["document"]
        gains.append(0 if doc in seen else grades.get(doc, 0))
        seen.add(doc)

    def dcg(values):
        return sum((2 ** grade - 1) / math.log2(rank + 2) for rank, grade in enumerate(values))

    ideal = dcg(sorted(grades.values(), reverse=True)[:10])
    return {
        **{f"recall@{k}": sum(grade > 0 for grade in gains[:k]) / len(grades)
           for k in (1, 5, 10)},
        "mrr@10": next((1 / rank for rank, grade in enumerate(gains, 1) if grade), 0),
        "ndcg@10": dcg(gains) / ideal,
        "evidence@5": float(any(hit["contains_evidence"] for hit in hits[:5])),
    }


def nunc_env(args, fixture, state):
    return {
        **os.environ,
        "NUNC_REPO_ROOT": str(fixture / "repo"),
        "NUNC_STATE_DIR": str(state),
        "NUNC_MODEL_CACHE_DIR": str(args.nunc_cache),
    }


def qmd_env(args, state):
    env = {
        **{key: value for key, value in os.environ.items() if not key.startswith("QMD_")},
        "XDG_CACHE_HOME": str(state / "cache"),
        "XDG_CONFIG_HOME": str(state / "config"),
        "XDG_DATA_HOME": str(state / "data"),
        "NO_COLOR": "1",
        "QMD_FORCE_CPU": "1",
        "QMD_EMBED_MODEL": str(args.qmd_embed_model),
    }
    if args.qmd_rerank_model:
        env["QMD_RERANK_MODEL"] = str(args.qmd_rerank_model)
    if args.qmd_generate_model:
        env["QMD_GENERATE_MODEL"] = str(args.qmd_generate_model)
    return env


def nunc_index(case, args, fixture, emit):
    layout = case if case in ("nunc-semantic-chunks", "nunc-small-chunks") else "nunc-default"
    state = fixture / f"state-{layout}"
    env = nunc_env(args, fixture, state)
    repo = fixture / "repo"
    config = repo / ".nunc/config.toml"
    strategy = "semantic" if layout == "nunc-semantic-chunks" else "structural"
    tokens = 128 if layout == "nunc-small-chunks" else 384
    config.write_text(
        f'sources = ["docs"]\n[chunking]\nstrategy = "{strategy}"\ntokens = {tokens}\n',
        encoding="utf-8",
    )
    if not state.exists():
        invoke_record("full", layout, [args.nunc, "reindex", "--full"], repo, env, state, emit)
        target = repo / "docs/design/agent-context.md"
        with change_doc(target):
            invoke_record("incremental", layout, [args.nunc, "reindex"], repo, env, state, emit)
        # Restore the original definition before any judged queries execute.
        measure([args.nunc, "reindex"], repo, env)
    return env


@contextlib.contextmanager
def change_doc(path):
    before = path.read_bytes()
    path.write_bytes(before + b"\nA one-document incremental indexing change.\n")
    try:
        yield
    finally:
        path.write_bytes(before)


def invoke_record(operation, case, cmd, cwd, env, state, emit):
    output, timing = measure(cmd, cwd, env)
    emit({
        "record": "index", "operation": operation, "case": case,
        "command": cmd, "output": output.decode(errors="replace").strip(),
        **timing, "database_bytes": total_bytes(state),
    })


def qmd_indexes(args, fixture, emit):
    for scope in ("active", "raw"):
        state = fixture / f"state-qmd-{scope}"
        env = qmd_env(args, state)
        root = fixture / f"qmd-{scope}"
        invoke_record(
            "full-lexical", f"qmd-{scope}",
            [args.qmd, "collection", "add", str(root), "--name", scope],
            fixture, env, state, emit,
        )
        invoke_record(
            "full-embedding", f"qmd-{scope}", [args.qmd, "embed"],
            fixture, env, state, emit,
        )
        if scope == "active":
            with change_doc(root / "design/agent-context.md"):
                invoke_record(
                    "incremental-lexical", f"qmd-{scope}", [args.qmd, "update"],
                    fixture, env, state, emit,
                )
                invoke_record(
                    "incremental-embedding", f"qmd-{scope}", [args.qmd, "embed"],
                    fixture, env, state, emit,
                )
            measure([args.qmd, "update"], fixture, env)
            measure([args.qmd, "embed"], fixture, env)


def qmd_command(case, query, binary):
    flags = ["--json", "-n", "10", "--full-path"]
    if case == "qmd-lexical":
        return [binary, "search", *flags, "--", query]
    if case == "qmd-default":
        return [binary, "query", *flags, "--", query]
    spec = f"vec: {query}"
    if case != "qmd-vector":
        spec = f"lex: {query}\n{spec}"
    if case != "qmd-hybrid-reranked":
        flags.append("--no-rerank")
    return [binary, "query", *flags, spec]


def query_results(output, mode, reverse, root, evidence):
    payload = json.loads(output)
    if mode == "nunc":
        if payload["warnings"]:
            raise RuntimeError(f"stale nunc benchmark index: {payload['warnings']}")
        rows = payload["results"]
    else:
        rows = payload

    results = []
    for row in rows:
        if mode == "nunc":
            path, text = row["path"], row["text"]
            chunk = row.get("matched_chunk") or {}
            lines = [chunk.get("line_start"), chunk.get("line_end")]
        else:
            path = Path(row["file"])
            path = (path.relative_to(root) if path.is_absolute() else path).as_posix()
            text = row.get("snippet", "")
            lines = [row.get("line"), None]
        doc = reverse[path]
        results.append({
            "document": doc, "score": row["score"], "lines": lines,
            "contains_evidence": doc in evidence and evidence[doc] in text,
        })
    return results


def evaluate(case, args, fixture, run_env, queries, mapping, emit):
    nunc = case in NUNC_CASES
    mode = "nunc" if nunc else "qmd"
    reverse = {paths[mode]: doc for doc, paths in mapping["paths"].items()}
    for round_index in range(args.rounds):
        for query in queries:
            scope = query.get("scope", "active")
            prompt = query["query"].replace("${commit_sha}", mapping["commit_sha"])
            root = fixture / ("repo" if nunc else f"qmd-{scope}")
            if nunc:
                env = run_env
                command = [args.nunc, "search", "--json", "--limit", "10", *NUNC_CASES[case]]
                if scope == "raw":
                    command.append("--raw")
                command.extend(["--", prompt])
            else:
                env = qmd_env(args, fixture / f"state-{case}-{scope}")
                command = qmd_command(case, prompt, args.qmd)
            output, timing = measure(command, root, env)
            hits = query_results(output, mode, reverse, root, query.get("evidence", {}))
            if scope == "active" and any(hit["document"] in mapping["archived"] for hit in hits):
                raise RuntimeError(f"{case} returned an archived fixture during ordinary search")
            emit({
                "record": "query", "case": case, "round": round_index,
                "id": query["id"], "category": query["category"], "scope": scope,
                "command": command, "hits": hits, "response_bytes": len(output), **timing,
                "metrics": duplicate_safe_metrics(hits, query["grades"]),
                "has_evidence_judgment": bool(query.get("evidence")),
            })


def digest(path):
    sha = hashlib.sha256()
    with path.open("rb") as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b""):
            sha.update(chunk)
    return sha.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, type=Path, help="new scratch directory")
    parser.add_argument("--output", required=True, type=Path, help="new JSONL results file")
    parser.add_argument("--nunc", required=True, type=Path)
    parser.add_argument("--nunc-cache", required=True, type=Path)
    parser.add_argument("--qmd", required=True, type=Path)
    parser.add_argument("--qmd-embed-model", required=True, type=Path)
    parser.add_argument("--qmd-rerank-model", type=Path)
    parser.add_argument("--qmd-generate-model", type=Path)
    parser.add_argument("--rounds", type=int, default=2)
    parser.add_argument("--case", action="append", choices=(*NUNC_CASES, *QMD_CASES))
    parser.add_argument("--query", action="append", help="optional judged query ID")
    parser.add_argument("--query-file", type=Path, default=HERE / "queries.json")
    args = parser.parse_args()
    for name in vars(args):
        value = getattr(args, name)
        if isinstance(value, Path):
            setattr(args, name, value.absolute())
    if args.rounds < 1 or platform.system() != "Linux":
        parser.error("at least one round and a Linux host are required")
    if digest(args.qmd_embed_model) != QMD_MODEL_SHA256:
        parser.error("QMD embedding model checksum differs from the pinned benchmark model")
    if args.qmd_generate_model and not args.qmd_rerank_model:
        parser.error("the full QMD default also needs a local reranker model")
    cases = args.case or [*NUNC_CASES, *QMD_CASES[:3]]
    if args.case is None:
        if args.qmd_rerank_model:
            cases.append("qmd-hybrid-reranked")
        if args.qmd_generate_model:
            cases.append("qmd-default")
    if "qmd-hybrid-reranked" in cases and not args.qmd_rerank_model:
        parser.error("qmd-hybrid-reranked needs --qmd-rerank-model")
    if "qmd-default" in cases and not args.qmd_generate_model:
        parser.error("qmd-default needs --qmd-generate-model and --qmd-rerank-model")
    queries = read_json(args.query_file)["queries"]
    if args.query:
        if set(args.query) - {query["id"] for query in queries}:
            parser.error("unknown judged query ID")
        queries = [query for query in queries if query["id"] in args.query]
    with args.output.open("x", encoding="utf-8") as out:
        def emit(row):
            # Record pasteable commands while removing machine-specific scratch paths.
            if "command" in row:
                substitutions = [(str(args.root), "$FIXTURE"), (str(args.nunc), "nunc"), (str(args.qmd), "qmd")]
                row["command"] = [
                    substitute(argument, substitutions) for argument in map(str, row["command"])
                ]
            if row.get("record") == "index":
                row["output"] = re.sub(
                    r"(?m)^Model: /[^\r\n]+", "Model: $QMD_EMBED_MODEL",
                    row["output"].replace(str(args.root), "$FIXTURE"),
                )
            print(json.dumps(row, ensure_ascii=False), file=out, flush=True)

        prepare(args.root, str(args.nunc))
        versions = {
            name: measure([binary, "--version"], args.root, os.environ)[0].decode().strip()
            for name, binary in (("nunc", args.nunc), ("qmd", args.qmd))
        }
        if versions["qmd"] != "qmd 2.9.0 (aa247d0)":
            parser.error("expected pinned QMD 2.9.0")
        emit({
            "record": "metadata", "versions": versions, "cases": cases, "rounds": args.rounds,
            "started_utc": datetime.now(timezone.utc).isoformat(), "platform": platform.platform(),
            "nunc_binary_sha256": digest(args.nunc),
            "corpus_sha256": digest(HERE / "corpus.json"), "queries_sha256": digest(args.query_file),
            "query_ids": [query["id"] for query in queries],
            "qmd_embed_sha256": QMD_MODEL_SHA256,
            "qmd_rerank_sha256": digest(args.qmd_rerank_model) if args.qmd_rerank_model else None,
            "qmd_generate_sha256": digest(args.qmd_generate_model) if args.qmd_generate_model else None,
        })
        mapping = read_json(args.root / "paths.json")
        mapping["archived"] = {
            doc["id"] for doc in read_json(HERE / "corpus.json")["documents"] if doc.get("archived")
        }
        qmd_ready = False
        for case in cases:
            print(f"Running {case}", flush=True)
            if case in NUNC_CASES:
                env = nunc_index(case, args, args.root, emit)
            elif not qmd_ready:
                qmd_indexes(args, args.root, emit)
                qmd_ready = True
            if case not in NUNC_CASES:
                for scope in ("active", "raw"):
                    shutil.copytree(args.root / f"state-qmd-{scope}", args.root / f"state-{case}-{scope}")
            evaluate(case, args, args.root, env if case in NUNC_CASES else None, queries, mapping, emit)


def substitute(argument, substitutions):
    for old, new in substitutions:
        argument = argument.replace(old, new)
    return argument


if __name__ == "__main__":
    main()

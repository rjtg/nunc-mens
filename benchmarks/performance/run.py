"""Measure standalone and resident nunc retrieval across reproducible scale fixtures on Linux."""

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import platform

from benchmarks.performance.generate import generate
from benchmarks.performance.resident import PROFILE_PREFIX, resident
from benchmarks.process import measure_command, total_bytes


MODES = ("semantic", "keyword", "hybrid")
SIZES = (1000, 10000, 50000, 100000)


def read_profile(stderr):
    reports = [line[len(PROFILE_PREFIX):] for line in stderr.splitlines() if line.startswith(PROFILE_PREFIX)]
    if len(reports) != 1:
        raise ValueError("expected one profile on stderr from a search-profile build")
    stages = json.loads(reports[0])
    if "cli" not in stages or "format" not in stages:
        raise ValueError("incomplete search profile")
    return stages


def execute(binary, repo, env, mode, query, remote=False):
    command = [binary, "search", "--mode", mode, "--json", *query["args"]]
    if remote:
        command.append("--server")
    command.extend(["--", query["query"]])
    output, stderr, timing = measure_command(command, repo, env)
    response = json.loads(output)
    if response["mode_used"] != mode or response["warnings"]:
        raise ValueError(f"unexpected benchmark response in {mode}: {response['warnings']}")
    if query["filter"] == "file" and any(hit["path"] != query["args"][1] for hit in response["results"]):
        raise ValueError("file-filtered search returned a foreign path")
    return output, stderr, {**timing, "hits": len(response["results"])}


def probe(binary, repo, env, mode, query):
    _, stderr, timing = execute(binary, repo, env, mode, query)
    stages = read_profile(stderr)
    return {
        **timing, "stages_ns": stages,
        "outside_cli_ns": round(timing["seconds"] * 1e9) - stages["cli"],
    }


def paired_requests(args, chunks, repo, env, queries, emit):
    # Distinct whitespace creates independent exact cache keys for one prompt
    # used with several scopes. The standalone reference uses the same bytes.
    cases = [{**query, "query": query["query"] + " " * index}
             for index, query in enumerate(queries)]
    primer = {**cases[0], "query": queries[0]["query"] + " " * len(cases)}
    for mode in args.mode:
        references = []
        for query in cases:
            output, stderr, timing = execute(args.nunc, repo, env, mode, query)
            references.append(output)
            stages = read_profile(stderr)
            emit({
                "record": "paired_query", "chunks": chunks, "mode": mode, "phase": "standalone",
                "sample": 0, **query, **timing, "stages_ns": stages,
                "outside_cli_ns": round(timing["seconds"] * 1e9) - stages["cli"],
            })

        measured = []
        with resident(args.nunc, repo, env) as server:
            _, _, timing = execute(args.nunc, repo, env, mode, primer, remote=True)
            measured.append({
                "record": "primer", "chunks": chunks, "mode": mode, "phase": "cold-server",
                "startup_to_socket_seconds": server.startup_seconds, **primer, **timing,
            })
            for query, expected in zip(cases, references):
                for sample in range(args.rounds + 1):
                    output, _, timing = execute(args.nunc, repo, env, mode, query, remote=True)
                    if output != expected:
                        raise ValueError(f"{chunks}/{mode}/{query['filter']} socket output differs")
                    phase = "warm" if mode == "keyword" else ("hit" if sample else "miss")
                    measured.append({
                        "record": "paired_query", "chunks": chunks, "mode": mode,
                        "phase": phase, "sample": sample, **query, **timing,
                    })
        emit({
            "record": "resident", "chunks": chunks, "mode": mode,
            "peak_rss_kib": server.peak_rss_kib,
        })
        for item, (stages, cache) in zip(measured, server.profiles(mode, len(measured))):
            if cache is not None and (not cache["cacheable"] or cache["hit"] != (item["phase"] == "hit")):
                raise ValueError("unexpected query embedding cache sequence")
            emit({
                **item, "stages_ns": stages, "query_cache": cache,
                "outside_cli_ns": round(item["seconds"] * 1e9) - stages["cli"],
            })


def run_size(args, chunks, emit):
    root = args.root / str(chunks)
    repo = root / "repo"
    state = root / "state"
    if not args.reuse_fixtures:
        root.mkdir()
        generate(repo, chunks, args.unique_text)
    elif not (repo / "queries.json").is_file() or not state.is_dir():
        raise ValueError(f"missing existing isolated benchmark fixture for {chunks} chunks")
    env = {
        **os.environ,
        "NUNC_REPO_ROOT": str(repo),
        "NUNC_STATE_DIR": str(state),
        "NUNC_MODEL_CACHE_DIR": str(args.model_cache),
        "NUNC_SEARCH_PROFILE": "1",
    }
    output, _, timing = measure_command(
        [args.nunc, "reindex", *(["--full"] if not args.reuse_fixtures else []), "--source", "docs"],
        repo, env,
    )
    index_message = output.decode().strip()
    if not index_message.startswith(f"Indexed {chunks} chunks "):
        raise ValueError(f"wrong index size: {index_message}")
    emit({
        "record": "index", "chunks": chunks, "message": index_message,
        "operation": "refresh" if args.reuse_fixtures else "full",
        "database_bytes": total_bytes(state), **timing,
    })
    queries = json.loads((repo / "queries.json").read_text())[:6 * args.topics]
    if args.transport == "paired":
        paired_requests(args, chunks, repo, env, queries, emit)
        return
    for round_index in range(args.rounds):
        for mode in args.mode:
            if round_index == 1:
                emit({
                    "record": "warmup", "chunks": chunks, "mode": mode,
                    **probe(args.nunc, repo, env, mode, queries[0]),
                })
            for query in queries:
                emit({
                    "record": "query", "chunks": chunks, "mode": mode, "round": round_index,
                    **query, **probe(args.nunc, repo, env, mode, query),
                })


def environment():
    cpu = next(
        (line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines()
         if line.startswith("model name")),
        platform.machine(),
    )
    return {
        "platform": platform.platform(), "cpu": cpu, "logical_cpus": os.cpu_count(),
        "memory_bytes": os.sysconf("SC_PHYS_PAGES") * os.sysconf("SC_PAGE_SIZE"),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True, type=Path, help="new directory for generated repositories and indexes")
    parser.add_argument("--output", required=True, type=Path, help="new measurements file")
    parser.add_argument("--nunc", required=True, type=Path, help="release executable built with search-profile")
    parser.add_argument("--model-cache", required=True, type=Path)
    parser.add_argument("--size", action="append", type=int)
    parser.add_argument("--mode", action="append", choices=MODES)
    parser.add_argument("--rounds", type=int, default=2)
    parser.add_argument("--topics", type=int, choices=range(1, 5), default=4)
    parser.add_argument("--unique-text", action="store_true")
    parser.add_argument("--transport", choices=("standalone", "paired"), default="standalone")
    parser.add_argument("--reuse-fixtures", action="store_true", help="refresh existing isolated scale fixtures")
    args = parser.parse_args()
    for name in ("root", "output", "nunc", "model_cache"):
        setattr(args, name, getattr(args, name).resolve())
    args.mode = args.mode or MODES
    sizes = args.size or SIZES
    if platform.system() != "Linux" or args.rounds < 1 or min(sizes) < 1:
        parser.error("Linux, positive sizes, and at least one round are required")
    if not args.nunc.is_file() or not args.model_cache.is_dir():
        parser.error("the profiling executable and existing model cache are required")
    if len(set(sizes)) != len(sizes) or len(set(args.mode)) != len(args.mode):
        parser.error("sizes and modes must be unique")
    if not args.reuse_fixtures:
        args.root.mkdir()
    elif args.unique_text or not args.root.is_dir():
        parser.error("--reuse-fixtures needs an existing root and reads its existing journal corpus")
    version, _, _ = measure_command([args.nunc, "--version"], args.root, os.environ)
    with args.output.open("x", encoding="utf-8") as out:
        def emit(record):
            print(json.dumps(record, ensure_ascii=False), file=out, flush=True)

        emit({
            "record": "metadata", "version": version.decode().strip(),
            "started_utc": datetime.now(timezone.utc).isoformat(), **environment(),
            "sizes": sizes, "modes": args.mode, "rounds": args.rounds,
            "topics": args.topics, "unique_text": args.unique_text,
            "model": "intfloat/multilingual-e5-small@614241f622f53c4eeff9890bdc4f31cfecc418b3",
            "transport": args.transport, "concurrency": 1, "reused_fixtures": args.reuse_fixtures,
        })
        for size in sizes:
            print(f"Measuring {size:,} chunks", flush=True)
            run_size(args, size, emit)


if __name__ == "__main__":
    main()

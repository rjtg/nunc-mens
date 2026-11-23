"""Compare standalone and resident retrieval with the same existing index."""

import argparse
import json
import os
from pathlib import Path
from benchmarks.performance.resident import resident
from benchmarks.performance.run import read_profile
from benchmarks.process import measure_command


def compare(args, emit):
    env = {
        **os.environ,
        "NUNC_REPO_ROOT": str(args.repo),
        "NUNC_STATE_DIR": str(args.state),
        "NUNC_MODEL_CACHE_DIR": str(args.model_cache),
        "NUNC_SEARCH_PROFILE": "1",
    }
    command = [args.nunc, "search", "--mode", args.mode, "--json", "--", args.query]
    reference = None
    for step in range(args.samples):
        out, stderr, measure = measure_command(command, args.repo, env)
        result = json.loads(out)
        if not result["results"] or result["warnings"]:
            raise ValueError("expected matches from a current benchmark index")
        if reference is not None and out != reference:
            raise ValueError("standalone query results changed")
        reference = out
        emit({"transport": "standalone", "sample": step, **measure, "stages_ns": read_profile(stderr)})

    with resident(args.nunc, args.repo, env) as server:
        remote = [*command[:2], "--server", *command[2:]]
        for step in range(args.samples + 1 + int(args.measure_cache)):
            primer = args.measure_cache and step == 0
            invocation = [*remote[:-1], args.query + " "] if primer else remote
            out, _, measure = measure_command(invocation, args.repo, env)
            if not primer and out != reference:
                raise ValueError("socket output differs from standalone")
            emit({
                "transport": "socket", "sample": step, "cold_server": step == 0, **measure,
                **({"primer": primer} if args.measure_cache else {}),
                **({"startup_to_socket_seconds": server.startup_seconds} if step == 0 else {}),
            })

    profiles = server.profiles(args.mode, args.samples + 1 + int(args.measure_cache))
    if args.measure_cache:
        expected_hits = [False, False] + [True] * args.samples
        if [event["hit"] for _, event in profiles] != expected_hits:
            raise ValueError("unexpected query embedding cache sequence")
    for i, (stages, event) in enumerate(profiles):
        record = {"transport": "server-profile", "sample": i, "stages_ns": stages}
        if args.measure_cache:
            record["query_cache"] = event
        emit(record)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for arg in ("repo", "state", "model-cache", "nunc"):
        parser.add_argument(f"--{arg}", required=True, type=Path)
    parser.add_argument("--query", required=True)
    parser.add_argument("--mode", choices=("semantic", "keyword", "hybrid"), default="semantic")
    parser.add_argument("--samples", type=int, default=5)
    parser.add_argument("--measure-cache", action="store_true", help="compare a warm miss with exact hits")
    parser.add_argument("--output", type=Path, help="write measurements to a new JSONL file")
    args = parser.parse_args()
    if args.measure_cache and args.mode == "keyword":
        parser.error("keyword search does not embed queries")
    if args.samples < 1:
        parser.error("--samples must be positive")
    for key in ("repo", "state", "model_cache", "nunc"):
        setattr(args, key, getattr(args, key).resolve())
    if args.output:
        with args.output.open("x", encoding="utf-8") as output:
            compare(args, lambda record: print(json.dumps(record), file=output, flush=True))
    else:
        compare(args, lambda record: print(json.dumps(record), flush=True))


if __name__ == "__main__":
    main()

"""Compare standalone and resident retrieval with the same existing index."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import stat
import tempfile
import time

from benchmarks.performance.run import PROFILE_PREFIX, read_profile
from benchmarks.process import measure_command


def published(socket):
    try:
        mode = socket.lstat().st_mode
    except FileNotFoundError:
        return False
    return stat.S_ISSOCK(mode) and stat.S_IMODE(mode) == 0o600


def compare(args, emit):
    env = {
        **os.environ,
        "NUNC_REPO_ROOT": str(args.repo),
        "NUNC_STATE_DIR": str(args.state),
        "NUNC_MODEL_CACHE_DIR": str(args.model_cache),
        "NUNC_SEARCH_PROFILE": "1",
    }
    socket = args.repo / ".nunc/search-runtime/search.sock"
    if socket.exists():
        raise ValueError("use an isolated repository with no existing search socket")
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

    with tempfile.TemporaryFile() as diagnostics:
        started = time.perf_counter()
        with subprocess.Popen(
            [args.nunc, "serve", "--search-socket"], cwd=args.repo, env=env,
            stdout=subprocess.DEVNULL, stderr=diagnostics, stdin=subprocess.DEVNULL,
        ) as process:
            try:
                ready_by = started + 10
                while not published(socket):
                    if process.poll() is not None or time.perf_counter() >= ready_by:
                        raise RuntimeError("could not start search socket")
                    time.sleep(.01)
                admitted_at = time.perf_counter()
                remote = [*command[:2], "--server", *command[2:]]
                for step in range(args.samples + 1):
                    out, _, measure = measure_command(remote, args.repo, env)
                    if out != reference:
                        raise ValueError("socket output differs from standalone")
                    emit({
                        "transport": "socket", "sample": step, "cold_server": step == 0, **measure,
                        **({"startup_to_socket_seconds": admitted_at - started} if step == 0 else {}),
                    })
            finally:
                process.terminate()
                try:
                    process.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                    raise
        diagnostics.seek(0)
        server_samples = [
            json.loads(line[len(PROFILE_PREFIX):])
            for line in diagnostics if line.startswith(PROFILE_PREFIX)
        ]
    if len(server_samples) != args.samples + 1:
        raise ValueError("server profile count does not match the completed requests")
    if args.mode != "keyword" and any("model_load" in row for row in server_samples[1:]):
        raise ValueError("a warm search reloaded the model")
    if socket.exists():
        raise RuntimeError("server did not remove its socket")
    for i, stages in enumerate(server_samples):
        emit({"transport": "server-profile", "sample": i, "stages_ns": stages})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for arg in ("repo", "state", "model-cache", "nunc"):
        parser.add_argument(f"--{arg}", required=True, type=Path)
    parser.add_argument("--query", required=True)
    parser.add_argument("--mode", choices=("semantic", "keyword", "hybrid"), default="semantic")
    parser.add_argument("--samples", type=int, default=5)
    parser.add_argument("--output", type=Path, help="write measurements to a new JSONL file")
    args = parser.parse_args()
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

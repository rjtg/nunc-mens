"""Measure a freshness optimization on one index with output parity."""

import argparse
import json
import os
from pathlib import Path

from benchmarks.performance.run import MODES, read_profile
from benchmarks.process import measure_command


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ("repo", "state", "model-cache", "before", "after", "output"):
        parser.add_argument(f"--{key}", required=True, type=Path)
    parser.add_argument("--query", required=True)
    parser.add_argument("--source-file", required=True)
    parser.add_argument("--samples", type=int, default=1)
    args = parser.parse_args()
    if args.samples < 1:
        parser.error("--samples must be positive")
    env = {
        **os.environ,
        "NUNC_REPO_ROOT": str(args.repo.resolve()),
        "NUNC_STATE_DIR": str(args.state.resolve()),
        "NUNC_MODEL_CACHE_DIR": str(args.model_cache.resolve()),
        "NUNC_SEARCH_PROFILE": "1",
    }
    after = args.after.resolve()
    # Seed the auxiliary freshness metadata for existing indexes. The old
    # binary ignores the additive table and checks the same content digest.
    measure_command([after, "reindex", "--keyword-only"], args.repo, env)
    with args.output.open("x", encoding="utf-8") as output:
        for mode in MODES:
            command = ["search", "--mode", mode, "--source-file", args.source_file,
                       "--json", "--", args.query]
            for sample in range(args.samples):
                baseline = None
                for tag, binary in (("before", args.before.resolve()), ("after", after)):
                    stdout, stderr, timing = measure_command([binary, *command], args.repo, env)
                    result = json.loads(stdout)
                    if not result["results"] or result["warnings"]:
                        raise ValueError("expected matches from a current index")
                    if baseline is None:
                        baseline = stdout
                    elif stdout != baseline:
                        raise ValueError(f"{mode} results differ between versions")
                    print(json.dumps({
                        "version": tag, "sample": sample, "mode": mode,
                        "scope": "file", "path": args.source_file,
                        **timing, "stages_ns": read_profile(stderr),
                    }), file=output, flush=True)


if __name__ == "__main__":
    main()

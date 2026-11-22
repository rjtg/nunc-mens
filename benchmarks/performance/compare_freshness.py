"""Measure a freshness optimization on one index with output parity."""

import argparse
import json
import os
from pathlib import Path
from stat import S_ISREG

from benchmarks.performance.run import MODES, read_profile
from benchmarks.process import measure_command


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ("repo", "state", "model-cache", "before", "after", "output"):
        parser.add_argument(f"--{key}", required=True, type=Path)
    parser.add_argument("--before-state", type=Path, help="separate baseline state for the same fixture")
    parser.add_argument("--query", required=True)
    scope = parser.add_mutually_exclusive_group(required=True)
    scope.add_argument("--source-file")
    scope.add_argument("--source")
    parser.add_argument("--samples", type=int, default=1)
    parser.add_argument("--touch-source", action="store_true", help="trigger validation with a metadata-only change")
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
    root = args.repo.resolve()
    source = root / (args.source_file or args.source)
    stat = None
    if args.touch_source:
        if not args.source_file:
            parser.error("--touch-source requires --source-file")
        if not source.is_relative_to(root) or source.resolve() != source:
            parser.error("--touch-source requires an ordinary path inside the fixture")
        stat = source.stat()
        if not S_ISREG(stat.st_mode):
            parser.error("--touch-source requires a regular file")
    if stat:
        os.utime(source, ns=(stat.st_atime_ns, stat.st_mtime_ns + 1_000_000_000))
    try:
        with args.output.open("x", encoding="utf-8") as output:
            for mode in MODES:
                flag = "--source-file" if args.source_file else "--source"
                command = ["search", "--mode", mode, flag, args.source_file or args.source,
                           "--json", "--", args.query]
                for sample in range(args.samples):
                    baseline = None
                    for tag, binary in (("before", args.before.resolve()), ("after", after)):
                        version_env = env
                        if tag == "before" and args.before_state:
                            version_env = {**env, "NUNC_STATE_DIR": str(args.before_state.resolve())}
                        stdout, stderr, timing = measure_command([binary, *command], args.repo, version_env)
                        result = json.loads(stdout)
                        if not result["results"] or result["warnings"]:
                            raise ValueError("expected matches from a current index")
                        if baseline is None:
                            baseline = stdout
                        elif stdout != baseline:
                            raise ValueError(f"{mode} results differ between versions")
                        print(json.dumps({
                            "version": tag, "sample": sample, "mode": mode,
                            "scope": "file" if args.source_file else "directory",
                            "path": args.source_file or args.source,
                            **({"metadata_only_touch": True} if args.touch_source else {}),
                            **timing, "stages_ns": read_profile(stderr),
                        }), file=output, flush=True)
    finally:
        if stat:
            os.utime(source, ns=(stat.st_atime_ns, stat.st_mtime_ns))



if __name__ == "__main__":
    main()

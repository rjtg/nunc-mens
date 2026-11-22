"""Render index and standalone search timings from scale benchmark JSONL."""

import argparse
from collections import defaultdict
import gzip
import json
import math
from pathlib import Path
from statistics import median


def tail(values, fraction):
    return sorted(values)[math.ceil(fraction * len(values)) - 1]


def summarize(rows):
    if not rows or rows[0].get("record") != "metadata":
        raise ValueError("scale measurements must start with metadata")
    meta = rows[0]
    indexes = {}
    groups = defaultdict(list)
    stage_groups = defaultdict(list)
    for row in rows[1:]:
        if row["record"] == "index":
            if row["chunks"] in indexes:
                raise ValueError(f"duplicate index record for {row['chunks']}")
            indexes[row["chunks"]] = row
        if row["record"] != "query":
            continue
        phase = "first" if row["round"] == 0 else "repeat"
        groups[(row["chunks"], row["mode"], row["filter"], phase)].append(row)
        stage_groups[(row["chunks"], row["mode"], phase)].append(row)
    phases = ("first", "repeat") if meta["rounds"] > 1 else ("first",)
    for size in meta["sizes"]:
        if size not in indexes:
            raise ValueError(f"missing index record for {size}")
        for mode in meta["modes"]:
            for scope in ("unfiltered", "directory", "file"):
                for phase in phases:
                    rounds = 1 if phase == "first" else meta["rounds"] - 1
                    expected = 2 * min(meta["topics"], size) * rounds
                    samples = groups[(size, mode, scope, phase)]
                    unique = {(row["round"], row["query_kind"], row["query"]) for row in samples}
                    if len(samples) != expected or len(unique) != expected:
                        raise ValueError(f"incomplete samples for {size}/{mode}/{scope}/{phase}")

    print("| Chunks | Build s | Build RSS MiB | Index MiB |")
    print("| ---: | ---: | ---: | ---: |")
    for size, row in indexes.items():
        print(f"| {size} | {row['seconds']:.3f} | {row['peak_rss_kib']/1024:.1f} | "
              f"{row['database_bytes']/1024**2:.1f} |")

    print()
    print("| Chunks | Mode | Scope | Pass | N | p50 ms | p95 ms | p99 ms | Queries/s | Peak RSS MiB |")
    print("| ---: | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |")
    for (size, mode, scope, phase), samples in sorted(groups.items()):
        seconds = [row["seconds"] for row in samples]
        quantiles = [median(seconds), tail(seconds, .95), tail(seconds, .99)]
        times = " | ".join(f"{1000 * value:.2f}" for value in quantiles)
        throughput = len(seconds) / sum(seconds)
        rss = max(row["peak_rss_kib"] for row in samples) / 1024
        print(f"| {size} | {mode} | {scope} | {phase} | {len(seconds)} | {times} | {throughput:.2f} | {rss:.1f} |")

    print()
    print("Median stage milliseconds across all scopes. Scan spans include their filtering subspans.")
    print()
    print("| Chunks | Mode | Pass | Stage | Median ms |")
    print("| ---: | --- | --- | --- | ---: |")
    for (size, mode, phase), samples in sorted(stage_groups.items()):
        durations = defaultdict(list)
        for row in samples:
            durations["outside_cli"].append(row["outside_cli_ns"])
            for name, value in row["stages_ns"].items():
                durations[name].append(value)
        for stage, values in sorted(durations.items()):
            print(f"| {size} | {mode} | {phase} | {stage} | {median(values)/1e6:.3f} |")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("results", type=Path)
    args = parser.parse_args()
    opener = gzip.open if args.results.suffix == ".gz" else open
    with opener(args.results, "rt", encoding="utf-8") as source:
        summarize([json.loads(line) for line in source])


if __name__ == "__main__":
    main()

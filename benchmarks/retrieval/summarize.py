"""Render Markdown tables from raw retrieval benchmark JSONL."""

import argparse
from collections import defaultdict
import gzip
import json
import math
from pathlib import Path
from statistics import mean, median


METRICS = ("recall@1", "recall@5", "recall@10", "mrr@10", "ndcg@10")


def load(path):
    opener = gzip.open if path.suffix == ".gz" else open
    with opener(path, mode="rt", encoding="utf-8") as file:
        for line in file:
            yield json.loads(line)


def summarize(rows):
    if not rows or rows[0]["record"] != "metadata":
        raise ValueError("benchmark metadata must precede measurements")
    metadata = rows[0]
    queries = [row for row in rows if row["record"] == "query"]
    by_case = defaultdict(list)
    for row in queries:
        by_case[row["case"]].append(row)
    expected = {
        (query_id, round_index)
        for query_id in metadata.get("query_ids", {row["id"] for row in queries})
        for round_index in range(metadata["rounds"])
    }
    for case in metadata["cases"]:
        measured = {(row["id"], row["round"]) for row in by_case[case]}
        if not expected or measured != expected or len(by_case[case]) != len(expected):
            raise ValueError(f"missing or duplicate measurements for {case}")

    print("### Retrieval quality")
    print()
    print("| Mode | Category | N | R@1 | R@5 | R@10 | MRR@10 | nDCG@10 | Evidence@5 |")
    print("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
    for case in metadata["cases"]:
        first = [row for row in by_case[case] if row["round"] == 0]
        categories = sorted({row["category"] for row in first})
        for category in ("all", *categories):
            group = [row for row in first if category == "all" or row["category"] == category]
            values = [f"{mean(row['metrics'][key] for row in group):.3f}" for key in METRICS]
            evidence = [row["metrics"]["evidence@5"] for row in group if row["has_evidence_judgment"]]
            values.append(f"{mean(evidence):.3f}" if evidence else "—")
            print(f"| {case} | {category} | {len(group)} | {' | '.join(values)} |")

    print()
    print("### Standalone CLI latency")
    print()
    print("| Mode | First p50 s | First p95 s | Repeat p50 s | Repeat p95 s | Max RSS MiB |")
    print("| --- | ---: | ---: | ---: | ---: | ---: |")
    for case in metadata["cases"]:
        first = [row["seconds"] for row in by_case[case] if row["round"] == 0]
        repeats = [row["seconds"] for row in by_case[case] if row["round"] != 0]
        maximum = max(row["peak_rss_kib"] for row in by_case[case]) / 1024
        print(f"| {case} | {percentiles(first)} | {percentiles(repeats)} | {maximum:.1f} |")

    print()
    print("### Indexing")
    print()
    print("| Layout | Operation | Wall s | Peak RSS MiB | Database MiB |")
    print("| --- | --- | ---: | ---: | ---: |")
    for row in rows:
        if row["record"] == "index":
            maximum = row["peak_rss_kib"] / 1024
            database = row["database_bytes"] / 1024**2
            print(f"| {row['case']} | {row['operation']} | {row['seconds']:.3f} | {maximum:.1f} | {database:.3f} |")


def percentiles(values):
    if not values:
        return "— | —"
    ordered = sorted(values)
    return f"{median(ordered):.3f} | {ordered[math.ceil(0.95 * len(ordered)) - 1]:.3f}"


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("results", type=Path)
    args = parser.parse_args()
    summarize(list(load(args.results)))

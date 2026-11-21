"""Generate isolated, deterministic Markdown source trees for search profiling."""

import argparse
import json
import os
from pathlib import Path
import subprocess


# Each file contains one short Markdown paragraph, which maps to one default chunk.
TOPICS = (
    ("decisions", "Cache validation", "Use a rename after checking the weights and tokenizer hashes.",
     "Verifying download integrity", "The staging folder stays private until the new cache is validated."),
    ("incidents", "Clock drift", "Deployment tokens appear expired when the worker clock runs ahead.",
     "Troubleshooting rejected credentials", "Synchronize the staging host before minting replacement tokens."),
    ("runbooks", "Writer exclusion", "Take the repository lease before replacing the active generation.",
     "Recovering from parallel writes", "Open the previous checkpoint if publication was interrupted."),
    ("reference", "Worker termination", "Drain outstanding deliveries before recycling an active consumer.",
     "Waiting for jobs before shutdown", "SIGTERM stops acceptance and awaits the last acknowledgement."),
)
COMPONENTS = (
    "dispatcher", "scheduler", "replicator", "manifest reader",
    "agent bridge", "checkpoint writer", "model cache", "rollout service",
)
REGIONS = ("canary", "staging", "western", "eastern", "local", "European", "test", "rescue")


def generate(repo, count, unique):
    repo.mkdir()
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    subprocess.run(["git", "init", "-q", str(repo)], check=True, env=env)
    for folder, *_ in TOPICS:
        (repo / "docs" / folder).mkdir(parents=True)
    for index in range(count):
        folder, subject, first, _, second = TOPICS[index % len(TOPICS)]
        component = COMPONENTS[(index // len(TOPICS)) % len(COMPONENTS)]
        region = REGIONS[(index // (len(TOPICS) * len(COMPONENTS))) % len(REGIONS)]
        ticket = index if unique else index % (len(TOPICS) * len(COMPONENTS) * len(REGIONS))
        note = f"{subject} in the {region} {component} (SPW-{ticket:06d}). {first} {second}\n"
        (repo / "docs" / folder / f"note-{index:06d}.md").write_text(note, encoding="utf-8")
    queries = []
    for index, (folder, _, _, query, _) in enumerate(TOPICS[:count]):
        exact = f"docs/{folder}/note-{index:06d}.md"
        for kind, flags in (
            ("unfiltered", []),
            ("directory", ["--source", f"docs/{folder}"]),
            ("file", ["--source-file", exact]),
        ):
            queries.append({"query": query, "filter": kind, "args": flags})
    (repo / "queries.json").write_text(json.dumps(queries, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path, help="new fixture Git root")
    parser.add_argument("count", type=int, help="number of one-chunk Markdown sources")
    parser.add_argument("--unique-text", action="store_true", help="use a unique body in every file")
    args = parser.parse_args()
    if args.count < 1:
        parser.error("count must be positive")
    generate(args.root.absolute(), args.count, args.unique_text)

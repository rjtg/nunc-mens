"""Build an isolated journal with deterministic, repeated benchmark notes."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import uuid


PARAGRAPH = (
    "WriterLock serializes publication of the checkpoint. The service verifies the "
    "staged manifest before replacing it. Check the recorded generation and resume "
    "unpublished work from the last durable checkpoint.\n\n"
)


def generate(repo, count):
    repo.mkdir()
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    subprocess.run(["git", "init", "-q", str(repo)], env=env, check=True)
    local = repo / ".nunc"
    local.mkdir(mode=0o700)
    entries = local / "entries"
    entries.mkdir(mode=0o700)
    for index in range(count):
        entry_id = uuid.uuid5(uuid.NAMESPACE_URL, f"nunc-journal-v1/{index}")
        header = {"version": 1, "id": str(entry_id), "timestamp": "2026-01-01T00:00:00Z",
                  "repo": "journal-fixture", "tags": ["benchmark"]}
        body = f"# Checkpoint {index % 16}\n\n" + PARAGRAPH * 40
        path = entries / f"{entry_id}.md"
        path.write_text(f"---\n{json.dumps(header)}\n---\n{body}", encoding="utf-8")
        path.chmod(0o600)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo", type=Path, help="new fixture Git root")
    parser.add_argument("count", type=int)
    args = parser.parse_args()
    if args.count < 1:
        parser.error("count must be positive")
    generate(args.repo.resolve(), args.count)


if __name__ == "__main__":
    main()

"""Materialize the distributable retrieval fixture for both CLIs."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import uuid


HERE = Path(__file__).resolve().parent


def write(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(text.encode("utf-8"))


def run(repo, env, *command, **options):
    return subprocess.run(
        command, cwd=repo, env=env, check=True, text=True, **options
    )


def prepare(root, nunc):
    root.mkdir()
    repo = root / "repo"
    (repo / "docs").mkdir(parents=True)
    env = {
        **{key: value for key, value in os.environ.items() if not key.startswith("GIT_")},
        "NUNC_REPO_ROOT": str(repo),
        "NUNC_STATE_DIR": str(root / "nunc-state"),
        "NUNC_MODEL_CACHE_DIR": str(root / "unused-model-cache"),
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": os.devnull,
    }
    run(repo, env, "git", "init", "-q", "--object-format=sha1")
    run(repo, env, nunc, "init", "--source", "docs")
    corpus = json.loads((HERE / "corpus.json").read_text(encoding="utf-8"))
    paths = {}
    archived = []
    commit_sha = None
    for doc in corpus["documents"]:
        label = doc["id"]
        body = (
            (HERE / doc["file"]).read_bytes().decode("utf-8")
            if "file" in doc
            else doc["text"]
        )
        kind = doc["kind"]
        if kind == "source":
            nunc_path = Path("docs", doc["path"])
            qmd_path = Path(doc["path"])
            write(repo / nunc_path, body)
        elif kind == "memory":
            entry_id = str(uuid.uuid5(uuid.NAMESPACE_URL, f"nunc-benchmark-v1/{label}"))
            nunc_path = Path(".nunc/entries", f"{entry_id}.md")
            qmd_path = Path("memories", f"{label}.md")
            meta = {
                "version": 1,
                "id": entry_id,
                "timestamp": doc["timestamp"],
                "repo": "sparrow",
                "tags": ["benchmark"],
            }
            write(repo / nunc_path, f"---\n{json.dumps(meta)}\n---\n{body}")
            if doc.get("archived"):
                archived.append(entry_id)
        elif kind == "git":
            commit_env = {
                **env,
                "GIT_AUTHOR_NAME": "Sparrow Fixture",
                "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
                "GIT_COMMITTER_NAME": "Sparrow Fixture",
                "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
                "GIT_AUTHOR_DATE": doc["timestamp"],
                "GIT_COMMITTER_DATE": doc["timestamp"],
            }
            write(repo / doc["path"], doc["file_text"])
            run(repo, env, "git", "add", "--", doc["path"])
            run(
                repo, commit_env, "git", "-c", "core.hooksPath=/dev/null",
                "commit", "-q", "--cleanup=verbatim", "-F", "-", input=body,
            )
            commit_sha = run(
                repo, env, "git", "rev-parse", "HEAD", capture_output=True,
            ).stdout.strip()
            run(repo, env, nunc, "git", "ingest", commit_sha)
            for path in (repo / ".nunc/entries").glob("*.md"):
                captured = path.read_bytes().decode("utf-8")
                header, body = captured.removeprefix("---\n").split("\n---\n", 1)
                if json.loads(header).get("kind") == "git-commit":
                    nunc_path = path.relative_to(repo)
                    break
            else:
                raise RuntimeError(f"missing captured Git entry for {commit_sha}")
            qmd_path = Path("git", f"{label}.md")
        else:
            raise ValueError(f"unknown fixture kind {kind!r}")
        write(root / "qmd-raw" / qmd_path, body)
        if not doc.get("archived"):
            write(root / "qmd-active" / qmd_path, body)
        paths[label] = {"nunc": nunc_path.as_posix(), "qmd": qmd_path.as_posix()}
    if archived:
        run(repo, env, nunc, "archive", *archived, "--json")
    write(root / "paths.json", json.dumps({"commit_sha": commit_sha, "paths": paths}, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True, help="a new scratch directory")
    parser.add_argument("--nunc", type=Path, required=True, help="compiled nunc binary")
    args = parser.parse_args()
    prepare(args.root.resolve(), str(args.nunc.resolve()))

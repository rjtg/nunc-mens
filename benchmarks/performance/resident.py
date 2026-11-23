"""Own and instrument a private benchmark search server."""

from contextlib import contextmanager
from dataclasses import dataclass, field
import json
from pathlib import Path
import stat
import subprocess
import tempfile
import time
from typing import Optional


PROFILE_PREFIX = b"nunc-profile "
CACHE_PREFIX = b"nunc-cache "


def published(socket):
    try:
        mode = socket.lstat().st_mode
    except FileNotFoundError:
        return False
    return stat.S_ISSOCK(mode) and stat.S_IMODE(mode) == 0o600


@dataclass
class ServerRun:
    startup_seconds: float
    peak_rss_kib: Optional[int] = None
    lines: list[bytes] = field(default_factory=list)

    def profiles(self, mode, expected):
        stages = []
        cache = []
        for line in self.lines:
            if line.startswith(PROFILE_PREFIX):
                report = json.loads(line[len(PROFILE_PREFIX):])
                if "cli" not in report or "format" not in report:
                    raise ValueError("incomplete server profile")
                stages.append(report)
            elif line.startswith(CACHE_PREFIX):
                cache.append(json.loads(line[len(CACHE_PREFIX):]))
        if len(stages) != expected:
            raise ValueError("server profile count does not match the completed requests")
        if mode == "keyword":
            if cache or any("embed_query" in row for row in stages):
                raise ValueError("keyword search embedded a query")
            return [(row, None) for row in stages]
        if len(cache) != expected:
            raise ValueError("cache counters do not match vector requests")
        if any("model_load" in row for row in stages[1:]):
            raise ValueError("a warm search reloaded the model")
        for row, event in zip(stages, cache):
            if event["hit"] != ("embed_query" not in row):
                raise ValueError("embedding stage disagrees with query cache events")
        return list(zip(stages, cache))


def peak_memory(pid):
    status = Path(f"/proc/{pid}/status").read_text()
    for line in status.splitlines():
        if line.startswith("VmHWM:"):
            return int(line.split()[1])
    raise ValueError("server status lacks peak RSS")


@contextmanager
def resident(nunc, repo, env):
    socket = repo / ".nunc/search-runtime/search.sock"
    if socket.exists() or socket.is_symlink():
        raise ValueError("use an isolated repository with no existing search socket")
    with tempfile.TemporaryFile() as diagnostics:
        started = time.perf_counter()
        with subprocess.Popen(
            [nunc, "serve", "--search-socket"], cwd=repo, env=env,
            stdout=subprocess.DEVNULL, stderr=diagnostics, stdin=subprocess.DEVNULL,
        ) as process:
            try:
                while not published(socket):
                    if process.poll() is not None:
                        diagnostics.seek(0)
                        raise RuntimeError(
                            f"could not start search socket: {diagnostics.read().decode(errors='replace')}"
                        )
                    if time.perf_counter() - started >= 10:
                        raise RuntimeError("timed out waiting for socket publication")
                    time.sleep(.01)
                server = ServerRun(time.perf_counter() - started)
                yield server
                server.peak_rss_kib = peak_memory(process.pid)
            finally:
                process.terminate()
                try:
                    process.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                    raise
        if process.returncode != 0 or socket.exists():
            raise RuntimeError("server did not shut down cleanly")
        diagnostics.seek(0)
        server.lines = list(diagnostics)

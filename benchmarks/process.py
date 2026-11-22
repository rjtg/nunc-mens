"""Shared Linux CLI measurements for the benchmark scripts."""

import os
import subprocess
import tempfile
import time


def measure_command(command, cwd, env):
    """Capture Linux wait4 peak RSS for each CLI invocation."""
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        started = time.perf_counter()
        with subprocess.Popen(
            command, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr,
        ) as child:
            try:
                _, status, usage = os.wait4(child.pid, 0)
            except KeyboardInterrupt:
                child.terminate()
                raise
            child.returncode = os.waitstatus_to_exitcode(status)
            seconds = time.perf_counter() - started
        stdout.seek(0)
        stderr.seek(0)
        output = stdout.read()
        diagnostics = stderr.read()
        if child.returncode:
            raise RuntimeError(
                f"{command!r} exited {child.returncode}\n"
                f"{diagnostics.decode('utf-8', errors='replace')}\n"
                f"{output.decode('utf-8', errors='replace')}"
            )
        return output, diagnostics, {"seconds": seconds, "peak_rss_kib": usage.ru_maxrss}


def total_bytes(root):
    return sum(
        path.stat().st_size for path in root.rglob("*")
        if path.is_file() and path.suffix in {".sqlite", ".sqlite3", ".db"}
    )

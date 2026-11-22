import contextlib
import io
import unittest

from benchmarks.performance.run import read_profile
from benchmarks.performance.summarize import summarize, tail


class PerformanceTests(unittest.TestCase):
    def test_requires_one_timing_record_on_stderr(self):
        timing = b'nunc-profile {"cli":3, "format":1}'
        self.assertEqual(read_profile(b"note\n" + timing), {"cli": 3, "format": 1})
        for output in (b"", timing + b"\n" + timing, b'nunc-profile {"cli": 2}'):
            with self.assertRaises(ValueError):
                read_profile(output)

    def test_summarizes_a_complete_matrix_and_rejects_missing_queries(self):
        meta = {"record": "metadata", "sizes": [1], "modes": ["keyword"], "rounds": 1, "topics": 1}
        index = {
            "record": "index", "chunks": 1, "seconds": 1,
            "peak_rss_kib": 1024, "database_bytes": 1024**2,
        }
        samples = [
            {
                "record": "query", "chunks": 1, "mode": "keyword", "filter": scope, "round": 0,
                "seconds": 0.005, "peak_rss_kib": 1024, "outside_cli_ns": 2_000_000,
                "stages_ns": {"cli": 3_000_000}, "query_kind": kind, "query": query,
            }
            for scope in ("unfiltered", "directory", "file")
            for kind, query in (("concept", "reload"), ("identifier", "SPW-000000"))
        ]
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            summarize([meta, index, *samples])
        self.assertIn("| file | first | 2 | 5.00 | 5.00 | 5.00 | 200.00 |", output.getvalue())
        self.assertIn("| cli | 3.000 |", output.getvalue())
        with self.assertRaisesRegex(ValueError, "incomplete samples"):
            summarize([meta, index, *samples[:-1]])
        with self.assertRaisesRegex(ValueError, "incomplete samples"):
            summarize([meta, index, *samples[:-1], samples[-2]])
        self.assertEqual(tail([5, 1, 2], .99), 5)


if __name__ == "__main__":
    unittest.main()

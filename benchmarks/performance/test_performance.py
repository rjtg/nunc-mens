import contextlib
import io
import unittest

from benchmarks.performance.run import read_profile
from benchmarks.performance.resident import ServerRun
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

    def test_paired_report_requires_every_cache_phase_and_scope(self):
        meta = {
            "record": "metadata", "sizes": [1], "modes": ["semantic"],
            "rounds": 2, "topics": 1, "transport": "paired",
        }
        index = {
            "record": "index", "chunks": 1, "seconds": 1, "operation": "refresh",
            "peak_rss_kib": 1024, "database_bytes": 1024**2,
        }
        measurement = {
            "chunks": 1, "mode": "semantic", "seconds": 0.005, "peak_rss_kib": 1024,
            "outside_cli_ns": 2_000_000, "stages_ns": {"cli": 3_000_000},
        }
        resident = {**measurement, "record": "resident", "peak_rss_kib": 4096}
        primer = {**measurement, "record": "primer", "startup_to_socket_seconds": 0.02}
        samples = [
            {
                **measurement, "record": "paired_query", "filter": scope, "phase": phase,
                "sample": sample, "query_kind": kind, "query": query,
                "query_cache": None if phase == "standalone" else {
                    "hit": phase == "hit", "cacheable": True,
                },
            }
            for scope in ("unfiltered", "directory", "file")
            for kind, query in (("concept", "reload"), ("identifier", "SPW-000000"))
            for phase, sample in (("standalone", 0), ("miss", 0), ("hit", 1), ("hit", 2))
        ]
        rows = [meta, index, resident, primer, *samples]
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            summarize(rows)
        self.assertIn("| file | hit | 4 | 5.00 | 5.00 | 5.00 | 200.00 |", output.getvalue())
        self.assertIn("| 1 | semantic | 20.00 | 5.00 | — | 4.0 |", output.getvalue())
        with self.assertRaisesRegex(ValueError, "incomplete paired samples"):
            summarize(rows[:-1])
        with self.assertRaisesRegex(ValueError, "duplicate paired sample"):
            summarize([*rows, samples[-1]])
        with self.assertRaisesRegex(ValueError, "incorrect paired cache"):
            summarize([*rows[:-1], {**samples[-1], "query_cache": None}])

    def test_resident_profiles_validate_warm_inference_and_cache(self):
        cold = b'nunc-profile {"cli":3,"format":1,"model_load":1,"embed_query":1}'
        warm = b'nunc-profile {"cli":2,"format":1}'
        miss = b'nunc-cache {"cacheable":true,"hit":false}'
        hit = b'nunc-cache {"cacheable":true,"hit":true}'
        run = ServerRun(.02, lines=[miss, cold, hit, warm])
        self.assertEqual([event["hit"] for _, event in run.profiles("semantic", 2)], [False, True])
        with self.assertRaisesRegex(ValueError, "profile count"):
            run.profiles("semantic", 1)
        with self.assertRaisesRegex(ValueError, "keyword search embedded"):
            run.profiles("keyword", 2)
        run.lines[-1] = cold
        with self.assertRaisesRegex(ValueError, "reloaded"):
            run.profiles("semantic", 2)


if __name__ == "__main__":
    unittest.main()

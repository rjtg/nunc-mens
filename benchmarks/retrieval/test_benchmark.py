import json
from pathlib import Path
import unittest

from benchmark import duplicate_safe_metrics, qmd_command, query_results
from summarize import percentiles, summarize


class BenchmarkTests(unittest.TestCase):
    def test_duplicate_results_consume_slots_and_cannot_inflate_recall(self):
        hits = [
            {"document": document, "contains_evidence": document == "support"}
            for document in ("answer", "answer", "outside", "support")
        ]
        metrics = duplicate_safe_metrics(hits, {"answer": 3, "support": 1})
        self.assertEqual(metrics["recall@1"], 0.5)
        self.assertEqual(metrics["recall@5"], 1.0)
        self.assertEqual(metrics["mrr@10"], 1.0)
        self.assertLess(metrics["ndcg@10"], 1.0)
        self.assertEqual(metrics["evidence@5"], 1.0)

    def test_qmd_normalizes_relative_or_absolute_files_and_blank_searches(self):
        root = Path("/fixture")
        for location in ("docs/adr.md", "/fixture/docs/adr.md"):
            output = json.dumps(
                [{"file": location, "score": 1.0, "line": 2, "snippet": "matches evidence"}]
            )
            hit, = query_results(output, "qmd", {"docs/adr.md": "adr"}, root, {"adr": "evidence"})
            self.assertEqual(hit["document"], "adr")
            self.assertTrue(hit["contains_evidence"])
        self.assertEqual(query_results("[]", "qmd", {}, root, {}), [])

    def test_structured_hybrid_disables_expansion_and_reranking(self):
        command = qmd_command("qmd-hybrid", "Q", "qmd")
        self.assertEqual(command[-1], "lex: Q\nvec: Q")
        self.assertIn("--no-rerank", command)
        self.assertNotIn("--no-rerank", qmd_command("qmd-hybrid-reranked", "Q", "qmd"))
        self.assertEqual(qmd_command("qmd-lexical", "--flag", "qmd")[-2:], ["--", "--flag"])
        self.assertEqual(percentiles([4, 1, 3, 2]), "2.500 | 4.000")

    def test_summary_rejects_incomplete_experiments(self):
        with self.assertRaisesRegex(ValueError, "metadata"):
            summarize([])
        with self.assertRaisesRegex(ValueError, "missing or duplicate"):
            summarize([
                {"record": "metadata", "cases": ["native"], "rounds": 2, "query_ids": ["q"]},
                {"record": "query", "case": "native", "id": "q", "round": 0},
            ])


if __name__ == "__main__":
    unittest.main()

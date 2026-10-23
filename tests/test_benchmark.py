import importlib.util
import json
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("benchmark", ROOT / "benchmarks/benchmark.py")
benchmark = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(benchmark)


class BenchmarkTests(unittest.TestCase):
    def test_published_results_match_task_counts_and_summaries(self):
        path = ROOT / "benchmarks/results/aks-store-demo/results.json"
        self.assertTrue(path.exists(), "benchmarks/results/aks-store-demo/results.json must exist")
        report = json.loads(path.read_text())
        self.assertEqual(
            len(report["records"]),
            report["sessions"] * report["rounds"] * len(benchmark.KINDS),
        )
        self.assertEqual(
            report["summary"],
            benchmark.summarize(report["records"], report["sessions"]),
        )
        self.assertEqual(len(report["cold"]), report["sessions"])
        for record in report["records"]:
            self.assertGreater(record["disabled_ms"], 0)
            self.assertGreater(record["enabled_ms"], 0)

    def test_benchmark_artifacts_exist(self):
        out_dir = ROOT / "benchmarks/results/aks-store-demo"
        for name in (
            "results.json",
            "summary.csv",
            "latency-speedup.png",
            "latency-speedup.svg",
            "latency-speedup.pdf",
            "cold-start.png",
            "cold-start.svg",
            "cold-start.pdf",
        ):
            self.assertTrue((out_dir / name).is_file(), f"Missing artifact: {name}")


if __name__ == "__main__":
    unittest.main()

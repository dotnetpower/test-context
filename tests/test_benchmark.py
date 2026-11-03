import hashlib
import importlib.util
import json
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("benchmark", ROOT / "benchmarks/benchmark.py")
benchmark = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(benchmark)


class BenchmarkTests(unittest.TestCase):
    def _source_tree_digest(self):
        digest = hashlib.sha256()
        files = [ROOT / "Cargo.toml", ROOT / "Cargo.lock"]
        files.extend(sorted((ROOT / "src").glob("*.rs")))
        for path in files:
            relative = str(path.relative_to(ROOT)).encode()
            data = path.read_bytes()
            digest.update(len(relative).to_bytes(8, "little"))
            digest.update(relative)
            digest.update(len(data).to_bytes(8, "little"))
            digest.update(data)
        return digest.hexdigest()

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
        self.assertEqual(report["tctx_source_tree_sha256"], self._source_tree_digest())

    def test_published_performance_regression_gates(self):
        report = json.loads(
            (ROOT / "benchmarks/results/aks-store-demo/results.json").read_text()
        )
        summary = report["summary"]
        self.assertLess(summary["warm_suite"]["enabled_ms"]["mean"], 5.0)
        self.assertGreater(summary["warm_suite"]["speedup"]["ci95"][0], 100.0)
        self.assertGreater(summary["leaf_delta"]["speedup"]["ci95"][0], 5.0)
        self.assertGreater(summary["affected_graph"]["speedup"]["ci95"][0], 500.0)
        self.assertGreater(summary["all"]["speedup"]["ci95"][0], 20.0)

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

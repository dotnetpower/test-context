#!/usr/bin/env python3
"""Paired pytest efficacy benchmark on AKS Store Demo (src/ai-service); never touches the user's shared daemon."""

import argparse
from contextlib import contextmanager
import csv
import hashlib
import json
import os
from pathlib import Path
import random
import shutil
import socket
import statistics
import subprocess
import sys
import tempfile
import time


REPOSITORY = "https://github.com/dotnetpower/aks-store-demo.git"
KINDS = ("warm_suite", "bg_speculative", "merkle_revert", "leaf_delta", "affected_graph")
LABELS = (
    "Warm suite\n(cache hit)",
    "Post-edit\n(bg pre-run)",
    "Reverted edit\n(Merkle hit)",
    "Leaf edit\n(hybrid delta)",
    "Impact query\n(AST affected)",
    "Mixed suite",
)


def command(argv, *, cwd=None, env=None):
    result = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, timeout=120)
    if result.returncode != 0:
        raise RuntimeError(
            f"{Path(argv[0]).name} failed ({result.returncode}): "
            f"{result.stdout.decode(errors='replace')}\n{result.stderr.decode(errors='replace')}"
        )
    return result.stdout


def populate_ai_service_corpus(original_root: Path, dest_root: Path) -> dict:
    """Copy aks-store-demo/src/ai-service and attach deterministic unit/integration tests."""
    src_service = original_root / "src/ai-service"
    dst_service = dest_root / "src/ai-service"
    shutil.copytree(src_service, dst_service)

    # Add an isolated helper module imported by description_generator tests for leaf_delta testing
    (dst_service / "routers/prompt_sanitizer.py").write_text(
        '"""Tag formatting helper for AI service prompts."""\n\n'
        "SANITIZER_REVISION = 0\n\n"
        "def normalize_tags(tags: list[str]) -> list[str]:\n"
        "    return [t.strip() for t in tags if t and t.strip()]\n"
    )

    tests_dir = dst_service / "tests"
    tests_dir.mkdir(parents=True, exist_ok=True)

    (tests_dir / "conftest.py").write_text(
        "import os\n"
        "import pytest\n"
        "from fastapi.testclient import TestClient\n"
        "from main import app\n\n"
        "@pytest.fixture\n"
        "def client(monkeypatch):\n"
        "    for key in (\n"
        '        "USE_LOCAL_LLM", "USE_AZURE_OPENAI", "USE_AZURE_AD",\n'
        '        "AZURE_OPENAI_IMAGE_ENDPOINT", "AZURE_OPENAI_ENDPOINT",\n'
        '        "AZURE_OPENAI_IMAGE_DEPLOYMENT_NAME", "AZURE_OPENAI_DEPLOYMENT_NAME",\n'
        '        "OPENAI_API_KEY", "OPENAI_ORG_ID", "LOCAL_LLM_ENDPOINT", "LOCAL_LLM_MODEL",\n'
        "    ):\n"
        "        monkeypatch.delenv(key, raising=False)\n"
        "    return TestClient(app)\n"
    )

    (tests_dir / "test_health.py").write_text(
        "from main import app\n\n"
        "def test_health_default_capabilities(client):\n"
        '    res = client.get("/health")\n'
        "    assert res.status_code == 200\n"
        "    body = res.json()\n"
        '    assert body["status"] == "ok"\n'
        '    assert body["capabilities"] == ["description"]\n\n'
        "def test_health_includes_image_when_configured(client, monkeypatch):\n"
        '    monkeypatch.setenv("AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com")\n'
        '    monkeypatch.setenv("AZURE_OPENAI_IMAGE_DEPLOYMENT_NAME", "gpt-image-1")\n'
        '    res = client.get("/health")\n'
        "    assert res.status_code == 200\n"
        '    assert "image" in res.json()["capabilities"]\n\n'
        "def test_app_version_present(client):\n"
        '    res = client.get("/health")\n'
        '    assert res.json()["version"] == app.version\n\n'
        "def test_cors_middleware_headers(client):\n"
        '    res = client.options("/health", headers={"Origin": "http://localhost:8080", "Access-Control-Request-Method": "GET"})\n'
        "    assert res.status_code in (200, 400)\n"
    )

    (tests_dir / "test_description_generator.py").write_text(
        "from types import SimpleNamespace\n"
        "import pytest\n"
        "import routers.description_generator as dg\n"
        "from routers.prompt_sanitizer import normalize_tags\n\n"
        "def _fake_completion(text: str):\n"
        "    return SimpleNamespace(choices=[SimpleNamespace(message=SimpleNamespace(content=text))])\n\n"
        "def test_normalize_tags_strips_empty():\n"
        '    assert normalize_tags([" dog ", "", "  ", "toy"]) == ["dog", "toy"]\n\n'
        "def test_handle_local_llm_requires_endpoint(monkeypatch):\n"
        '    monkeypatch.delenv("LOCAL_LLM_ENDPOINT", raising=False)\n'
        "    with pytest.raises(ValueError, match='LOCAL_LLM_ENDPOINT'):\n"
        '        dg._handle_local_llm("prompt", 1.0)\n\n'
        "def test_handle_openai_requires_key_and_org(monkeypatch):\n"
        '    monkeypatch.delenv("OPENAI_API_KEY", raising=False)\n'
        '    monkeypatch.delenv("OPENAI_ORG_ID", raising=False)\n'
        "    with pytest.raises(ValueError, match='OPENAI_API_KEY'):\n"
        '        dg._handle_openai("prompt", 1.0)\n\n'
        "def test_handle_azure_openai_with_api_key(monkeypatch):\n"
        '    monkeypatch.setenv("AZURE_OPENAI_DEPLOYMENT_NAME", "gpt-5-mini")\n'
        '    monkeypatch.setenv("AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com")\n'
        '    monkeypatch.setenv("AZURE_OPENAI_API_KEY", "test-key")\n'
        "    monkeypatch.setattr(dg, 'AzureOpenAI', lambda **kw: SimpleNamespace())\n"
        "    monkeypatch.setattr(dg, '_create_completion', lambda c, m, p, t: _fake_completion('witty product copy'))\n"
        '    out = dg._handle_azure_openai("prompt", False, 0.7)\n'
        '    assert out == "witty product copy"\n'
    )

    (tests_dir / "test_image_generator.py").write_text(
        "from types import SimpleNamespace\n"
        "import pytest\n"
        "import routers.image_generator as ig\n\n"
        "def test_image_requires_endpoint(monkeypatch):\n"
        '    monkeypatch.delenv("AZURE_OPENAI_IMAGE_ENDPOINT", raising=False)\n'
        '    monkeypatch.delenv("AZURE_OPENAI_ENDPOINT", raising=False)\n'
        "    with pytest.raises(ValueError, match='AZURE_OPENAI_IMAGE_ENDPOINT'):\n"
        '        ig._handle_azure_openai("prompt", False)\n\n'
        "def test_image_requires_deployment(monkeypatch):\n"
        '    monkeypatch.setenv("AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com")\n'
        '    monkeypatch.delenv("AZURE_OPENAI_IMAGE_DEPLOYMENT_NAME", raising=False)\n'
        "    with pytest.raises(ValueError, match='AZURE_OPENAI_IMAGE_DEPLOYMENT_NAME'):\n"
        '        ig._handle_azure_openai("prompt", False)\n\n'
        "def test_image_returns_data_uri(monkeypatch):\n"
        '    monkeypatch.setenv("AZURE_OPENAI_ENDPOINT", "https://example.openai.azure.com")\n'
        '    monkeypatch.setenv("AZURE_OPENAI_IMAGE_DEPLOYMENT_NAME", "gpt-image-1")\n'
        '    monkeypatch.setenv("AZURE_OPENAI_API_KEY", "test-key")\n'
        "    fake_client = SimpleNamespace(\n"
        "        images=SimpleNamespace(generate=lambda **kw: SimpleNamespace(data=[SimpleNamespace(b64_json='aGVsbG8=')]))\n"
        "    )\n"
        "    monkeypatch.setattr(ig, 'AzureOpenAI', lambda **kw: fake_client)\n"
        '    uri = ig._handle_azure_openai("prompt", False)\n'
        '    assert uri == "data:image/png;base64,aGVsbG8="\n\n'
        "def test_extract_bad_request_moderation_blocked():\n"
        "    err = SimpleNamespace(body={'error': {'code': 'moderation_blocked', 'message': 'blocked'}})\n"
        "    code, msg = ig._extract_bad_request_details(err)\n"
        "    assert code == 422\n"
        '    assert "safety checks" in msg\n'
    )

    (tests_dir / "test_app_integration.py").write_text(
        "import routers.description_generator as dg\n"
        "import routers.image_generator as ig\n\n"
        "def test_post_description_endpoint_local_llm(client, monkeypatch):\n"
        '    monkeypatch.setenv("USE_LOCAL_LLM", "true")\n'
        '    monkeypatch.setenv("TEMPERATURE", "invalid-float")\n'
        "    monkeypatch.setattr(dg, '_handle_local_llm', lambda prompt, temp: f'desc:{temp}')\n"
        '    res = client.post("/generate/description", json={"name": "Catnip Toy", "tags": ["cat", "fun"]})\n'
        "    assert res.status_code == 200\n"
        '    assert res.json() == {"description": "desc:1.0"}\n\n'
        "def test_post_description_endpoint_error_returns_500(client, monkeypatch):\n"
        '    monkeypatch.setenv("USE_AZURE_OPENAI", "false")\n'
        '    monkeypatch.delenv("OPENAI_API_KEY", raising=False)\n'
        '    res = client.post("/generate/description", json={"name": "Bone", "tags": ["dog"]})\n'
        "    assert res.status_code == 500\n\n"
        "def test_post_image_endpoint_success(client, monkeypatch):\n"
        "    monkeypatch.setattr(ig, '_handle_azure_openai', lambda prompt, ad: 'data:image/png;base64,AAA=')\n"
        '    res = client.post("/generate/image", json={"name": "Leash", "description": "Strong nylon leash"})\n'
        "    assert res.status_code == 200\n"
        '    assert res.json() == {"image": "data:image/png;base64,AAA="}\n\n'
        "def test_post_image_endpoint_error_returns_500(client, monkeypatch):\n"
        '    monkeypatch.delenv("AZURE_OPENAI_ENDPOINT", raising=False)\n'
        '    res = client.post("/generate/image", json={"name": "Leash", "description": "Desc"})\n'
        "    assert res.status_code == 500\n"
    )

    digest = hashlib.sha256()
    files = sorted(p for p in dst_service.rglob("*") if p.is_file())
    total_bytes = 0
    for f in files:
        rel = str(f.relative_to(dest_root))
        data = f.read_bytes()
        digest.update(rel.encode())
        digest.update(data)
        total_bytes += len(data)
    return {
        "files": len(files),
        "python_files": len([f for f in files if f.suffix == ".py"]),
        "test_files": 4,
        "total_tests": 16,
        "bytes": total_bytes,
        "sha256": digest.hexdigest(),
    }


def rpc(home: Path, root: Path | None, name: str, arguments: dict) -> dict:
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(120)
        stream.connect(str(home / "daemon.sock"))
        payload = {"root": str(root) if root else None, "name": name, "arguments": arguments}
        stream.sendall(json.dumps(payload).encode() + b"\n")
        with stream.makefile("rb") as response:
            line = response.readline(4 * 1024 * 1024 + 1)
    result = json.loads(line)
    if "error" in result:
        raise RuntimeError(result["error"])
    return result["result"]


def timed(operation):
    started = time.perf_counter_ns()
    value = operation()
    return (time.perf_counter_ns() - started) / 1_000_000, value


@contextmanager
def daemon_session(executable: Path, pytest_bin: Path, root: Path, home: Path):
    env = os.environ.copy()
    env.update(TCTX_HOME=str(home), TCTX_PYTEST_BIN=str(pytest_bin))
    command(
        [
            str(executable),
            "--root",
            str(root),
            "--pytest-cmd",
            str(pytest_bin),
            "register",
        ],
        env=env,
    )
    with tempfile.TemporaryFile() as errors:
        process = subprocess.Popen(
            [str(executable), "daemon"],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=errors,
        )
        try:
            deadline = time.monotonic() + 15
            while not (home / "daemon.sock").exists():
                if process.poll() is not None or time.monotonic() > deadline:
                    errors.seek(0)
                    raise RuntimeError("Private tctx daemon startup failed: " + errors.read().decode())
                time.sleep(0.01)
            rpc(home, None, "ping", {})
            yield env
        finally:
            if process.poll() is None:
                process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


def run_direct_pytest(pytest_bin: Path, root: Path, extra_args=()):
    env = os.environ.copy()
    env["PYTHONPATH"] = f"{root}:{root / 'src/ai-service'}"
    out = command(
        [str(pytest_bin), "-q", "--tb=short", "--no-header", *extra_args, "src/ai-service/tests"],
        cwd=root,
        env=env,
    ).decode(errors="replace")
    return out


def percentile(values, fraction):
    values = sorted(values)
    position = (len(values) - 1) * fraction
    lower = int(position)
    upper = min(lower + 1, len(values) - 1)
    return values[lower] + (values[upper] - values[lower]) * (position - lower)


def summarize(records, sessions, bootstrap=5000):
    summaries = {}
    rng = random.Random(83171)
    for kind in (*KINDS, "all"):
        rows = [r for r in records if kind == "all" or r["kind"] == kind]
        groups = {s: [r for r in rows if r["session"] == s] for s in range(sessions)}
        means = {
            mode: [statistics.mean(row[mode] for row in groups[s]) for s in range(sessions)]
            for mode in ("disabled_ms", "enabled_ms")
        }
        draws = {mode: [] for mode in means}
        ratios = []
        for _ in range(bootstrap):
            selected = rng.choices(range(sessions), k=sessions)
            sampled = {
                mode: statistics.mean(values[i] for i in selected)
                for mode, values in means.items()
            }
            for mode, value in sampled.items():
                draws[mode].append(value)
            ratios.append(sampled["disabled_ms"] / sampled["enabled_ms"])
        summary = {"pairs": len(rows)}
        for mode in means:
            summary[mode] = {
                "mean": statistics.mean(means[mode]),
                "ci95": [percentile(draws[mode], q) for q in (0.025, 0.975)],
                "median": statistics.median(r[mode] for r in rows),
                "p95": percentile([r[mode] for r in rows], 0.95),
            }
        summary["speedup"] = {
            "ratio": summary["disabled_ms"]["mean"] / summary["enabled_ms"]["mean"],
            "ci95": [percentile(ratios, q) for q in (0.025, 0.975)],
        }
        summaries[kind] = summary
    return summaries


def render(report, output: Path):
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    plt.rcParams.update(
        {
            "font.family": "DejaVu Serif",
            "font.size": 10,
            "axes.spines.top": False,
            "axes.spines.right": False,
            "svg.fonttype": "none",
            "pdf.fonttype": 42,
            "savefig.dpi": 300,
        }
    )
    kinds = (*KINDS, "all")
    row_labels = [
        "Warm suite (cache hit)",
        "Post-edit (bg pre-run)",
        "Reverted edit (Merkle hit)",
        "Leaf edit (hybrid delta)",
        "Impact query (AST affected)",
        "Mixed suite (all tasks)",
    ]
    colors = ("#0072B2", "#D55E00")
    figure, (latency, speed) = plt.subplots(
        1,
        2,
        figsize=(13.4, 5.6),
        sharey=True,
        gridspec_kw={"width_ratios": [1.15, 1.0], "wspace": 0.08},
    )

    max_lat_ci = max(report["summary"][k]["disabled_ms"]["ci95"][1] for k in kinds)
    for index, (mode, label) in enumerate(
        (("disabled_ms", "TCTX off (direct pytest)"), ("enabled_ms", "TCTX on (AST + Zygote + cache)"))
    ):
        means = [report["summary"][kind][mode]["mean"] for kind in kinds]
        intervals = [report["summary"][kind][mode]["ci95"] for kind in kinds]
        offset = -0.18 if index == 0 else 0.18
        positions = [i + offset for i in range(len(kinds))]
        latency.barh(
            positions,
            means,
            height=0.32,
            color=colors[index],
            label=label,
            xerr=[
                [m - ci[0] for m, ci in zip(means, intervals)],
                [ci[1] - m for m, ci in zip(means, intervals)],
            ],
            capsize=3,
            error_kw={"linewidth": 1},
        )
        for row_idx, (m, ci) in enumerate(zip(means, intervals)):
            text_x = max(m, ci[1]) + max_lat_ci * 0.018
            val_str = f"{m:.1f} ms" if m < 100 else f"{m:.0f} ms"
            latency.text(
                text_x,
                row_idx + offset,
                val_str,
                va="center",
                ha="left",
                fontsize=8.5,
                color="#222222",
            )

    latency.set(
        yticks=range(len(kinds)),
        yticklabels=row_labels,
        xlabel="Mean wall time per operation (ms, lower is better)",
        xlim=(0, max_lat_ci * 1.26),
    )
    latency.set_title("A  Test execution & query latency", pad=26, fontsize=11, fontweight="bold")
    latency.legend(
        frameon=False,
        loc="lower center",
        bbox_to_anchor=(0.5, 1.01),
        ncol=2,
        fontsize=9.5,
    )
    latency.invert_yaxis()
    latency.grid(axis="x", alpha=0.25)
    latency.set_axisbelow(True)

    ratios = [report["summary"][kind]["speedup"]["ratio"] for kind in kinds]
    intervals = [report["summary"][kind]["speedup"]["ci95"] for kind in kinds]
    speed.errorbar(
        ratios,
        range(len(kinds)),
        xerr=[
            [r - ci[0] for r, ci in zip(ratios, intervals)],
            [ci[1] - r for r, ci in zip(ratios, intervals)],
        ],
        fmt="o",
        color=colors[0],
        markersize=6,
        capsize=4,
        elinewidth=1.4,
    )
    speed.axvline(1, color="#666666", linestyle="--", linewidth=1)
    speed.set(
        xlabel="Speedup = off / on (log scale, higher is better)",
        xscale="log",
    )
    speed.set_title("B  Paired-session speedup (95% CI)", pad=26, fontsize=11, fontweight="bold")
    speed.tick_params(axis="y", labelleft=False)
    min_ci = min(ci[0] for ci in intervals)
    max_ci = max(ci[1] for ci in intervals)
    speed.set_xlim(min(0.6, min_ci * 0.55), max_ci * 5.5)
    speed.grid(axis="x", which="both", alpha=0.25)
    for index, (value, ci) in enumerate(zip(ratios, intervals)):
        label_x = ci[1] * 1.16
        speed.text(
            label_x,
            index,
            f"{value:.1f}×",
            va="center",
            ha="left",
            fontsize=9.5,
            fontweight="bold",
            color="#111111",
        )

    figure.suptitle(
        f"AKS Store Demo (src/ai-service) · {report['sessions']} independent daemon sessions",
        fontsize=12.5,
        y=1.03,
    )
    output.mkdir(parents=True, exist_ok=True)
    for suffix in ("png", "svg", "pdf"):
        figure.savefig(
            output / ("latency-speedup." + suffix),
            bbox_inches="tight",
            pad_inches=0.16,
            metadata={"Creator": "Test Context benchmark"},
        )
    plt.close(figure)

    figure, ax = plt.subplots(1, 1, figsize=(7.2, 4.2), constrained_layout=True)
    sessions_x = list(range(1, len(report["cold"]) + 1))
    off_cold = [r["disabled_cold_ms"] for r in report["cold"]]
    on_cold = [r["enabled_cold_ms"] for r in report["cold"]]
    ax.plot(sessions_x, off_cold, "s-", color=colors[0], linewidth=1.8, label="TCTX off (direct pytest)")
    ax.plot(
        sessions_x,
        on_cold,
        "o-",
        color=colors[1],
        linewidth=1.8,
        label="TCTX on (cold AST build + initial run)",
    )
    ax.set(
        xlabel="Independent daemon session",
        ylabel="Wall time (ms)",
        title="Cold initial suite run (no prior cache)",
        xticks=sessions_x,
        ylim=(0, max(off_cold + on_cold) * 1.25),
    )
    ax.legend(frameon=False, loc="upper right")
    ax.grid(axis="y", alpha=0.25)
    for suffix in ("png", "svg", "pdf"):
        figure.savefig(
            output / ("cold-start." + suffix),
            bbox_inches="tight",
            pad_inches=0.14,
            metadata={"Creator": "Test Context benchmark"},
        )
    plt.close(figure)

    with (output / "summary.csv").open("w", newline="") as stream:
        writer = csv.writer(stream, lineterminator="\n")
        writer.writerow(
            [
                "task",
                "pairs",
                "off_mean_ms",
                "on_mean_ms",
                "speedup",
                "speedup_ci95_low",
                "speedup_ci95_high",
            ]
        )
        for kind in kinds:
            item = report["summary"][kind]
            writer.writerow(
                [
                    kind,
                    item["pairs"],
                    item["disabled_ms"]["mean"],
                    item["enabled_ms"]["mean"],
                    item["speedup"]["ratio"],
                    *item["speedup"]["ci95"],
                ]
            )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path)
    parser.add_argument("--tctx", type=Path)
    parser.add_argument("--pytest", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--sessions", type=int, default=8)
    parser.add_argument("--rounds", type=int, default=8)
    parser.add_argument("--seed", type=int, default=20261023)
    parser.add_argument("--render-only", type=Path)
    args = parser.parse_args()

    if args.render_only:
        render(json.loads(args.render_only.read_text()), args.output)
        return
    if not args.root or not args.tctx or not args.pytest:
        parser.error("--root, --tctx, and --pytest are required unless --render-only is set")

    original = args.root.resolve()
    executable = args.tctx.resolve()
    pytest_bin = args.pytest.resolve()

    if command(["git", "status", "--porcelain"], cwd=original).strip():
        parser.error("Benchmark target checkout must be clean")

    rng = random.Random(args.seed)
    records = []
    cold = []

    with tempfile.TemporaryDirectory(prefix="tctx-bench-") as temporary:
        temporary = Path(temporary)
        for session in range(args.sessions):
            root = temporary / f"corpus-{session}"
            home = temporary / f"state-{session}"
            corpus_info = populate_ai_service_corpus(original, root)
            dg_path = root / "src/ai-service/routers/description_generator.py"
            dg_baseline = dg_path.read_text()
            sanitizer_path = root / "src/ai-service/routers/prompt_sanitizer.py"
            sanitizer_baseline = sanitizer_path.read_text()

            with daemon_session(executable, pytest_bin, root, home):
                # 1. Measure cold uncached execution
                off_cold_ms, off_out = timed(lambda: run_direct_pytest(pytest_bin, root))
                assert "16 passed" in off_out, f"Unexpected direct pytest output: {off_out}"

                on_cold_ms, on_res = timed(lambda: rpc(home, root, "test_run", {}))
                assert on_res["status"] == "passed" and on_res["passed"] == 16
                assert on_res["executed_files_count"] == 4
                cold.append(
                    {
                        "session": session,
                        "disabled_cold_ms": off_cold_ms,
                        "enabled_cold_ms": on_cold_ms,
                    }
                )

                for round_num in range(args.rounds):
                    # Task 1: warm_suite (100% cache hit vs direct pytest)
                    ms_off, out_off = timed(lambda: run_direct_pytest(pytest_bin, root))
                    ms_on, res_on = timed(lambda: rpc(home, root, "test_run", {}))
                    assert "16 passed" in out_off
                    assert res_on["mode"] == "instant_cache_hit" and res_on["passed"] == 16
                    records.append(
                        {
                            "session": session,
                            "round": round_num,
                            "kind": "warm_suite",
                            "disabled_ms": ms_off,
                            "enabled_ms": ms_on,
                        }
                    )

                    # Task 2: leaf_delta (Immediate on-demand test_run right after editing leaf helper -> 3 cached + 1 executed vs full pytest)
                    nonce_off = f"off_{session}_{round_num}_{rng.randint(1, 10_000_000)}"
                    sanitizer_path.write_text(
                        sanitizer_baseline + f"\n# leaf_delta_nonce={nonce_off}\n"
                    )
                    ms_off, out_off = timed(lambda: run_direct_pytest(pytest_bin, root))
                    assert "16 passed" in out_off

                    nonce_on = f"on_{session}_{round_num}_{rng.randint(1, 10_000_000)}"
                    sanitizer_path.write_text(
                        sanitizer_baseline + f"\n# leaf_delta_nonce={nonce_on}\n"
                    )
                    ms_on, res_on = timed(lambda: rpc(home, root, "test_run", {}))
                    assert res_on["status"] == "passed" and res_on["passed"] == 16
                    assert (
                        (res_on["cached_files_count"] == 3 and res_on["executed_files_count"] == 1)
                        or (
                            res_on["cached_files_count"] == 4
                            and res_on["joined_background_files_count"] > 0
                        )
                    ), f"Expected 3 cached + 1 executed (or singleflight join), got: {res_on}"
                    records.append(
                        {
                            "session": session,
                            "round": round_num,
                            "kind": "leaf_delta",
                            "disabled_ms": ms_off,
                            "enabled_ms": ms_on,
                        }
                    )

                    # Task 3: bg_speculative (Edit router -> background worker runs during agent think window -> instant test_run)
                    bg_nonce = f"bg_{session}_{round_num}_{rng.randint(1, 10_000_000)}"
                    dg_path.write_text(dg_baseline + f"\n# bg_nonce={bg_nonce}\n")
                    # Wait for watcher/background worker to finish speculative pre-execution (simulating agent reasoning time)
                    rpc(home, root, "test_sync", {"wait": True})
                    ms_off, out_off = timed(lambda: run_direct_pytest(pytest_bin, root))
                    ms_on, res_on = timed(lambda: rpc(home, root, "test_run", {}))
                    assert "16 passed" in out_off
                    assert res_on["mode"] == "instant_cache_hit" and res_on["passed"] == 16
                    records.append(
                        {
                            "session": session,
                            "round": round_num,
                            "kind": "bg_speculative",
                            "disabled_ms": ms_off,
                            "enabled_ms": ms_on,
                        }
                    )

                    # Task 4: merkle_revert (Revert edited files back to baseline -> instant historical Merkle cache hit)
                    dg_path.write_text(dg_baseline)
                    sanitizer_path.write_text(sanitizer_baseline)
                    ms_off, out_off = timed(lambda: run_direct_pytest(pytest_bin, root))
                    ms_on, res_on = timed(lambda: rpc(home, root, "test_run", {}))
                    assert "16 passed" in out_off
                    assert res_on["mode"] == "instant_cache_hit" and res_on["passed"] == 16
                    records.append(
                        {
                            "session": session,
                            "round": round_num,
                            "kind": "merkle_revert",
                            "disabled_ms": ms_off,
                            "enabled_ms": ms_on,
                        }
                    )

                    # Task 5: affected_graph (Query affected tests for description_generator.py vs pytest --collect-only)
                    ms_off, _ = timed(
                        lambda: run_direct_pytest(pytest_bin, root, extra_args=("--collect-only",))
                    )
                    ms_on, aff_on = timed(
                        lambda: rpc(
                            home,
                            root,
                            "test_affected",
                            {"files": ["src/ai-service/routers/description_generator.py"]},
                        )
                    )
                    assert aff_on["total_affected_test_files"] == 4
                    records.append(
                        {
                            "session": session,
                            "round": round_num,
                            "kind": "affected_graph",
                            "disabled_ms": ms_off,
                            "enabled_ms": ms_on,
                        }
                    )

            print(f"session {session + 1}/{args.sessions}: parity verified", flush=True)

    summary = summarize(records, args.sessions)
    report = {
        "schema": 1,
        "repository": REPOSITORY,
        "target_commit": command(["git", "rev-parse", "HEAD"], cwd=original).decode().strip(),
        "tctx_source_commit": command(
            ["git", "rev-parse", "HEAD"], cwd=Path(__file__).resolve().parents[1]
        )
        .decode()
        .strip(),
        "tctx_binary_sha256": hashlib.sha256(executable.read_bytes()).hexdigest(),
        "python_version": ".".join(map(str, sys.version_info[:3])),
        "sessions": args.sessions,
        "rounds": args.rounds,
        "seed": args.seed,
        "corpus": corpus_info,
        "cold": cold,
        "summary": summary,
        "records": records,
    }
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output / "results.json").write_text(json.dumps(report, indent=2) + "\n")
    render(report, args.output)
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()

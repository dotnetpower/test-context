#!/usr/bin/env python3
"""Bounded daemon resource soak for cache hits, edits, and Zygote reclamation."""

from __future__ import annotations

import argparse
import os
import tempfile
import threading
import time
from pathlib import Path

from benchmark import (
    daemon_session,
    rpc,
)


def process_metrics(pid: int) -> tuple[int, int, int]:
    status = Path(f"/proc/{pid}/status").read_text(encoding="utf-8")
    values: dict[str, int] = {}
    for line in status.splitlines():
        key, separator, value = line.partition(":")
        if separator and key in {"VmRSS", "Threads"}:
            values[key] = int(value.strip().split()[0])
    fd_count = len(list(Path(f"/proc/{pid}/fd").iterdir()))
    return values["VmRSS"], fd_count, values["Threads"]


def process_cpu_ticks(pid: int) -> int:
    fields = Path(f"/proc/{pid}/stat").read_text(encoding="utf-8").split()
    return int(fields[13]) + int(fields[14])


def wait_for(predicate, timeout: float, description: str) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.1)
    raise RuntimeError(f"timed out waiting for {description}")


def populate_corpus(root: Path) -> None:
    (root / "src/checkout").mkdir(parents=True)
    (root / "tests").mkdir()
    (root / "pyproject.toml").write_text(
        "[tool.pytest.ini_options]\npythonpath = [\"src\"]\ntestpaths = [\"tests\"]\n",
        encoding="utf-8",
    )
    (root / "src/checkout/__init__.py").write_text("", encoding="utf-8")
    (root / "src/checkout/service.py").write_text(
        "def total(quantity: int, price: int) -> int:\n"
        "    return quantity * price\n",
        encoding="utf-8",
    )
    (root / "tests/test_orders.py").write_text(
        "from checkout.service import total\n\n"
        "def test_total():\n"
        "    assert total(3, 7) == 21\n",
        encoding="utf-8",
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--pytest", type=Path, required=True)
    parser.add_argument("--iterations", type=int, default=100)
    parser.add_argument("--edit-iterations", type=int, default=20)
    parser.add_argument("--cancel-iterations", type=int, default=10)
    parser.add_argument("--rss-growth-kib", type=int, default=32 * 1024)
    parser.add_argument("--idle-cpu-ticks", type=int, default=5)
    args = parser.parse_args()

    executable = Path(os.environ.get("TCTX_BIN", "target/release/tctx")).resolve()
    pytest_bin = args.pytest.resolve()
    if not executable.is_file():
        raise RuntimeError(f"missing tctx binary: {executable}")

    with tempfile.TemporaryDirectory(prefix="tctx-resource-soak-") as temporary:
        base = Path(temporary)
        corpus = base / "corpus"
        home = base / "home"
        populate_corpus(corpus)
        target = "tests/test_orders.py"
        slow_target = "tests/test_slow.py"
        (corpus / slow_target).write_text(
            "import time\n"
            "import pytest\n\n"
            "pytestmark = pytest.mark.live\n\n"
            "def test_slow():\n"
            "    time.sleep(30)\n",
            encoding="utf-8",
        )

        with daemon_session(
            executable,
            pytest_bin,
            corpus,
            home,
            extra_env={"TCTX_ZYGOTE_IDLE_SECS": "1"},
        ):
            first = rpc(home, corpus, "test_run", {"targets": [target]})
            if first["status"] != "passed":
                raise RuntimeError(f"initial run failed: {first}")
            initial_status = rpc(home, corpus, "test_status", {})
            pid = int(initial_status["daemon"]["pid"])
            baseline = process_metrics(pid)

            for _ in range(args.iterations):
                result = rpc(home, corpus, "test_run", {"targets": [target]})
                if result["mode"] != "instant_cache_hit" or result["status"] != "passed":
                    raise RuntimeError(f"cache hit regression: {result}")

            cancellation_fd_baseline = process_metrics(pid)[1]
            for index in range(args.cancel_iterations):
                outcome: list[BaseException | dict] = []

                def run_slow() -> None:
                    try:
                        outcome.append(
                            rpc(
                                home,
                                corpus,
                                "test_run",
                                {"targets": [slow_target], "no_cache": True},
                            )
                        )
                    except BaseException as error:
                        outcome.append(error)

                thread = threading.Thread(target=run_slow, daemon=True)
                thread.start()
                cancellation_delivered = False

                def cancel_started_child() -> bool:
                    nonlocal cancellation_delivered
                    cancellation_delivered = rpc(
                        home, corpus, "test_cancel", {}
                    )["cancelled"]
                    return cancellation_delivered

                wait_for(
                    cancel_started_child,
                    timeout=5.0,
                    description=f"cancellable pytest child {index}",
                )
                if not cancellation_delivered:
                    raise RuntimeError(f"cancellation was not delivered at iteration {index}")
                thread.join(timeout=5.0)
                if thread.is_alive():
                    raise RuntimeError(f"cancelled request did not finish at iteration {index}")
                if not outcome or "TEST_CANCELLED" not in str(outcome[0]):
                    raise RuntimeError(
                        f"unexpected cancellation outcome at iteration {index}: {outcome}"
                    )
            cancellation_fd_after = process_metrics(pid)[1]
            if cancellation_fd_after > cancellation_fd_baseline + 4:
                raise RuntimeError(
                    "Zygote cancellation leaked daemon FDs: "
                    f"{cancellation_fd_baseline}->{cancellation_fd_after}"
                )

            source = corpus / "src" / "checkout" / "service.py"
            original = source.read_text(encoding="utf-8")
            for index in range(args.edit_iterations):
                source.write_text(
                    original + f"\nSOAK_REVISION = {index}\n",
                    encoding="utf-8",
                )
                result = rpc(home, corpus, "test_run", {"targets": [target]})
                if result["status"] != "passed":
                    raise RuntimeError(f"delta run failed at edit {index}: {result}")
            source.write_text(original, encoding="utf-8")
            restored = rpc(home, corpus, "test_run", {"targets": [target]})
            if restored["status"] != "passed":
                raise RuntimeError(f"restored run failed: {restored}")

            after_load = process_metrics(pid)
            rss_growth = after_load[0] - baseline[0]
            if rss_growth > args.rss_growth_kib:
                raise RuntimeError(
                    f"daemon RSS grew {rss_growth} KiB "
                    f"(limit {args.rss_growth_kib} KiB)"
                )
            if after_load[1] > baseline[1] + 12:
                raise RuntimeError(
                    f"daemon FD count grew {after_load[1] - baseline[1]} "
                    "(limit 12)"
                )
            if after_load[2] > baseline[2] + 4:
                raise RuntimeError(
                    f"daemon thread count grew {after_load[2] - baseline[2]} "
                    "(limit 4)"
                )

            wait_for(
                lambda: not rpc(
                    home, corpus, "test_status", {}
                )["workspace"]["zygote"]["ready"],
                timeout=8.0,
                description="idle Zygote reclamation",
            )
            status = rpc(home, corpus, "test_status", {})
            patterns = status["workspace"]["cache_storage"]["invocation_patterns"]
            if patterns < 1:
                raise RuntimeError(f"invocation learning did not persist: {status}")

            final = process_metrics(pid)
            idle_cpu_before = process_cpu_ticks(pid)
            time.sleep(2.0)
            idle_cpu_ticks = process_cpu_ticks(pid) - idle_cpu_before
            if idle_cpu_ticks > args.idle_cpu_ticks:
                raise RuntimeError(
                    f"idle daemon consumed {idle_cpu_ticks} CPU ticks "
                    f"(limit {args.idle_cpu_ticks})"
                )
            print(
                "resource soak passed: "
                f"cache_hits={args.iterations}, cancellations={args.cancel_iterations}, "
                f"edits={args.edit_iterations}, "
                f"rss={baseline[0]}->{after_load[0]}->{final[0]} KiB, "
                f"fds={baseline[1]}->{after_load[1]}->{final[1]}, "
                f"threads={baseline[2]}->{after_load[2]}->{final[2]}, "
                f"patterns={patterns}, idle_cpu_ticks={idle_cpu_ticks}"
            )

    return 0


if __name__ == "__main__":
    raise SystemExit(main())

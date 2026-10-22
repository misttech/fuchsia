#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Benchmark comparing textpb.parse (eager) vs. textpb.parse_lazy.

Run directly via:
    PYTHONPATH=build/rbe python3 build/rbe/textpb_benchmark.py
"""

import argparse
import gc
import statistics
import sys
import time
from collections.abc import Callable, Sequence
from typing import NamedTuple

import textpb


class BenchmarkResult(NamedTuple):
    name: str
    min_ms: float
    median_ms: float
    stdev_ms: float


def measure(
    fn: Callable[[], None],
    iterations: int = 50,
    warmup: int = 5,
) -> BenchmarkResult:
    """Measures execution time of a callable with warmup and disabled GC."""
    # Warmup runs (JIT/regex caching, page cache)
    for _ in range(warmup):
        fn()

    times: list[float] = []
    gc_was_enabled = gc.isenabled()
    try:
        gc.disable()
        for _ in range(iterations):
            t0 = time.perf_counter()
            fn()
            times.append((time.perf_counter() - t0) * 1000.0)
    finally:
        if gc_was_enabled:
            gc.enable()

    return BenchmarkResult(
        name="",
        min_ms=min(times),
        median_ms=statistics.median(times),
        stdev_ms=statistics.stdev(times) if len(times) > 1 else 0.0,
    )


# --- Synthetic Payload Generators ---


def make_small_payload() -> str:
    """Trivial 5-line message."""
    return """
completion_status: STATUS_CACHE_HIT
remote_metadata: {
  action_digest: "01bdb44a.../144"
}
"""


def make_medium_payload() -> str:
    """Medium action log with ~30 compiler args."""
    args = "\n".join(
        f'  args: "-I../../path/to/include/dir_{i}"' for i in range(30)
    )
    return f"""command: {{
  identifiers: {{
    command_id: "cmd-1234"
    execution_id: "exec-5678"
  }}
  exec_root: "/path/to/root"
{args}
}}
remote_metadata: {{
  action_digest: "01bdb44ad645bb85fc57c0871c08cb278a2d103684d1783dc8896315e3be6c1a/144"
  output_file_digests: {{
    key: "obj/foo.o"
    value: "deadbeef/123"
  }}
}}
completion_status: STATUS_REMOTE_EXECUTION
"""


def make_large_payload(num_args: int = 1500) -> str:
    """Large action log with 1,500+ compiler arguments and defines."""
    args = "\n".join(
        f'  args: "-I../../some/very/long/path/to/include/header_{i}.h"'
        for i in range(num_args)
    )
    return f"""command: {{
  identifiers: {{
    command_id: "8ea55c85-b05aaed4"
    invocation_id: "user:79f6b99f-37dc-4385-a1b2-65c20b5ab92e"
    tool_name: "re-client"
    tool_version: "0.186.0.305f6525"
    execution_id: "efd87f28-74f6-4d44-8142-c3409a028aab"
  }}
  exec_root: "/usr/local/google/home/user/fuchsia/fuchsia"
  working_directory: "."
  platform: {{
    key: "Pool"
    value: "large_worker"
  }}
  platform: {{
    key: "container-image"
    value: "docker://gcr.io/cloud-marketplace/google/debian11@sha256:69e278"
  }}
{args}
}}
remote_metadata: {{
  num_input_directories: 1
  total_input_bytes: 361
  total_output_bytes: 39
  command_digest: "d3ca1601fcac6a64c6a0027704257c8157b5156cc5d0d83a715f1fdd6b01f613/217"
  action_digest: "01bdb44ad645bb85fc57c0871c08cb278a2d103684d1783dc8896315e3be6c1a/144"
  output_file_digests: {{
    key: "obj/path/to/my_cxx_target.o"
    value: "deadbeefcafef00d12345678/123456"
  }}
}}
completion_status: STATUS_REMOTE_EXECUTION
"""


# --- Benchmark Runners ---


def run_payload_benchmarks(title: str, text: str, iterations: int = 50) -> None:
    lines = text.splitlines()

    print(f"\n{'=' * 75}")
    print(f"Payload: {title} ({len(lines)} lines, {len(text):,} bytes)")
    print(f"{'=' * 75}")
    header = f"{'Benchmark':<42} {'Min (ms)':>10} {'Median (ms)':>12} {'Speedup':>10}"
    print(header)
    print("-" * len(header))

    # 1. Baseline eager parse()
    def eager_run() -> None:
        parsed = textpb.parse(lines)
        _ = parsed.get("completion_status")
        _ = parsed.get("remote_metadata")

    res_eager = measure(eager_run, iterations=iterations)
    baseline_ms = res_eager.min_ms
    print(
        f"{'1. textpb.parse (eager baseline)':<42} {res_eager.min_ms:>10.3f} {res_eager.median_ms:>12.3f} {'1.0x':>10}"
    )

    # 2. parse_lazy() - Happy Path (reads only completion_status & remote_metadata)
    def lazy_happy_path() -> None:
        lazy = textpb.parse_lazy(text)
        _ = lazy["completion_status"][0].text
        meta = lazy.get("remote_metadata")
        if meta:
            _ = meta[0]["action_digest"][0].text
            _ = meta[0].get("output_file_digests")

    res_lazy_hp = measure(lazy_happy_path, iterations=iterations)
    speedup_hp = (
        baseline_ms / res_lazy_hp.min_ms if res_lazy_hp.min_ms > 0 else 1.0
    )
    print(
        f"{'2. parse_lazy (happy path: metadata only)':<42} {res_lazy_hp.min_ms:>10.3f} {res_lazy_hp.median_ms:>12.3f} {f'{speedup_hp:.1f}x':>10}"
    )

    # 3. parse_lazy() - Failure Path (also accesses .command & execution_id)
    def lazy_failure_path() -> None:
        lazy = textpb.parse_lazy(text)
        _ = lazy["completion_status"][0].text
        meta = lazy.get("remote_metadata")
        if meta:
            _ = meta[0]["action_digest"][0].text
        cmd = lazy.get("command")
        if cmd:
            _ = cmd[0]["identifiers"][0]["execution_id"][0].text

    res_lazy_fail = measure(lazy_failure_path, iterations=iterations)
    speedup_fail = (
        baseline_ms / res_lazy_fail.min_ms if res_lazy_fail.min_ms > 0 else 1.0
    )
    print(
        f"{'3. parse_lazy (failure path: with command)':<42} {res_lazy_fail.min_ms:>10.3f} {res_lazy_fail.median_ms:>12.3f} {f'{speedup_fail:.1f}x':>10}"
    )

    # 4. parse_lazy() - Full Eager Materialization (.to_dict())
    def lazy_full_dict() -> None:
        lazy = textpb.parse_lazy(text)
        _ = lazy.to_dict()

    res_lazy_dict = measure(lazy_full_dict, iterations=iterations)
    speedup_dict = (
        baseline_ms / res_lazy_dict.min_ms if res_lazy_dict.min_ms > 0 else 1.0
    )
    print(
        f"{'4. parse_lazy (full materialization .to_dict)':<42} {res_lazy_dict.min_ms:>10.3f} {res_lazy_dict.median_ms:>12.3f} {f'{speedup_dict:.1f}x':>10}"
    )


def main(argv: Sequence[str]) -> int:
    parser = argparse.ArgumentParser(
        description="Benchmark textpb eager vs. lazy parsing."
    )
    parser.add_argument(
        "--iterations",
        type=int,
        default=50,
        help="Number of iterations per benchmark run (default: 50)",
    )
    args = parser.parse_args(argv)

    print("Running textpb parsing benchmarks...")
    run_payload_benchmarks(
        "Small (Trivial)", make_small_payload(), iterations=args.iterations
    )
    run_payload_benchmarks(
        "Medium (Action Log ~30 args)",
        make_medium_payload(),
        iterations=args.iterations,
    )
    run_payload_benchmarks(
        "Large (C++ Heavy 1500 args)",
        make_large_payload(1500),
        iterations=args.iterations,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import importlib
import os
import time
from dataclasses import dataclass
from typing import Dict, List, Tuple

from base import Benchmark


@dataclass
class BenchmarkResult:
    """Holds the result of a single benchmark run.

    Attributes:
        name: The name of the benchmark.
        description: The description of the benchmark.
        time_taken: The time taken to run the benchmark in seconds.
        passed: Whether the benchmark passed.
        expected: Whether the result was expected.
        compare: A list of other benchmark names to compare against.
    """

    name: str
    description: str
    time_taken: float
    passed: bool
    expected: bool
    compare: List[str]


class BenchmarkResults:
    """Holds a collection of benchmark results."""

    def __init__(self) -> None:
        self.results: List[BenchmarkResult] = []

    def add(self, result: BenchmarkResult) -> None:
        """Adds a benchmark result.

        Args:
            result: The benchmark result to add.
        """
        self.results.append(result)

    def print_table(self) -> None:
        """Prints the results in a formatted table."""
        if not self.results:
            print("No benchmark results to display.")
            return

        max_name = max(len(r.name) for r in self.results)
        max_desc = max(len(r.description) for r in self.results)

        # Header
        header = (
            f"{'Benchmark':<{max_name}} | "
            f"{'Description':<{max_desc}} | "
            f"{'Time (s)':<10} | "
            f"{'Status':<8} | "
            f"{'Expected':<8}"
        )
        print(header)
        print("-" * len(header))

        # Rows
        for result in self.results:
            status = "Passed" if result.passed else "Failed"
            expected_str = "Yes" if result.expected else "No"
            row = (
                f"{result.name:<{max_name}} | "
                f"{result.description:<{max_desc}} | "
                f"{result.time_taken:<10.4f} | "
                f"{status:<8} | "
                f"{expected_str:<8}"
            )
            print(row)

    def print_comparison_table(self) -> None:
        """Prints a comparison of benchmarks."""
        comparisons: List[Tuple[BenchmarkResult, BenchmarkResult]] = []
        results_by_name: Dict[str, BenchmarkResult] = {
            r.name: r for r in self.results
        }

        for result in self.results:
            if not result.compare:
                continue

            for other_name in result.compare:
                if other_name in results_by_name:
                    other_result = results_by_name[other_name]
                    comparisons.append((result, other_result))

        if not comparisons:
            return

        print("\n--- Benchmark Comparisons ---")

        max_name1 = max(len(c[0].name) for c in comparisons)
        max_name2 = max(len(c[1].name) for c in comparisons)

        # Header
        header = (
            f"{'Benchmark 1':<{max_name1}} | "
            f"{'Time (s)':<10} | "
            f"{'Benchmark 2':<{max_name2}} | "
            f"{'Time (s)':<10} | "
            f"{'Difference (s)':<15}"
        )
        print(header)
        print("-" * len(header))

        # Rows
        for r1, r2 in comparisons:
            time_diff = r1.time_taken - r2.time_taken
            row = (
                f"{r1.name:<{max_name1}} | "
                f"{r1.time_taken:<10.4f} | "
                f"{r2.name:<{max_name2}} | "
                f"{r2.time_taken:<10.4f} | "
                f"{time_diff:<+15.4f}"
            )
            print(row)


def find_benchmarks() -> List[Benchmark]:
    """Finds all defined benchmarks by importing all .py files in this dir.

    Returns:
        A list of all found benchmark instances.
    """
    benchmarks: List[Benchmark] = []
    benchmark_dir = os.path.dirname(__file__)
    for filename in os.listdir(benchmark_dir):
        if filename.endswith(".py") and filename not in (
            "run.py",
            "base.py",
            "__init__.py",
        ):
            module_name = filename[:-3]
            importlib.import_module(module_name)

    for subclass in Benchmark.__subclasses__():
        benchmarks.append(subclass())
    return benchmarks


def run_benchmarks(benchmarks: List[Benchmark]) -> BenchmarkResults:
    """Runs a set of benchmarks and returns the results.

    Args:
        benchmarks: A list of benchmarks to run.

    Returns:
        The results of the benchmark runs.
    """
    print("Running benchmarks...")
    results = BenchmarkResults()
    for benchmark in benchmarks:
        start_time = time.time()
        try:
            benchmark.setup()
            start_time = time.time()
            benchmark.run()
            end_time = time.time()
            passed = True
        except Exception as e:
            end_time = time.time()
            passed = False
            print(f"Benchmark {benchmark.name} failed with exception: {e}")
        finally:
            benchmark.cleanup()

        time_taken = end_time - start_time
        expected = benchmark.expected_to_pass == passed

        results.add(
            BenchmarkResult(
                name=benchmark.name,
                description=benchmark.description,
                time_taken=time_taken,
                passed=passed,
                expected=expected,
                compare=benchmark.compare,
            )
        )
    return results


def main() -> None:
    """Runs the benchmarking suite."""
    parser = argparse.ArgumentParser(description="Run benchmarks.")
    parser.add_argument(
        "--benchmark",
        action="append",
        type=str,
        help="The name of a benchmark to run. Can be specified multiple times.",
    )
    args = parser.parse_args()

    benchmarks = find_benchmarks()

    if args.benchmark:
        benchmarks = [b for b in benchmarks if b.name in args.benchmark]

    results = run_benchmarks(benchmarks)
    print("\n--- Benchmark Results ---")
    results.print_table()
    results.print_comparison_table()


if __name__ == "__main__":
    main()

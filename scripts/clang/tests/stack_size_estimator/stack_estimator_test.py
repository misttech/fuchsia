# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Tests for stack_estimator."""

import argparse
import io
import json
import pathlib
import sys
import tempfile
import unittest
from collections.abc import Sequence
from pathlib import Path
from typing import Any
from unittest.mock import patch

import call_graph
import graph
import stack_estimator

# This will be set by main() from the command line arguments.
TESTDATA_DIR: pathlib.Path


class TestGraphHelper(unittest.TestCase):
    """Tests for graph.Graph."""

    def test_scc(self) -> None:
        g = graph.Graph()
        # 0 -> 1 -> 2 -> 0 (Cycle)
        # 2 -> 3
        g.add_edge(0, 1)
        g.add_edge(1, 2)
        g.add_edge(2, 0)
        g.add_edge(2, 3)

        sccs = g.get_sccs()
        # Should have {0, 1, 2} and {3}
        self.assertEqual(len(sccs), 2)
        scc_sets = [set(scc) for scc in sccs]
        self.assertIn({0, 1, 2}, scc_sets)
        self.assertIn({3}, scc_sets)

    def test_topological_sort(self) -> None:
        g = graph.Graph()
        # 0 -> 1, 0 -> 2, 1 -> 3, 2 -> 3
        g.add_edge(0, 1)
        g.add_edge(0, 2)
        g.add_edge(1, 3)
        g.add_edge(2, 3)

        order = g.topological_sort()
        self.assertEqual(len(order), 4)
        self.assertEqual(order[0], 0)
        self.assertEqual(order[3], 3)
        # 1 and 2 can be in any order in between
        self.assertSetEqual(set(order[1:3]), {1, 2})

    def test_deep_chain(self) -> None:
        # Deeper than Python's default recursion limit.
        g = graph.Graph()
        n = 20000
        for i in range(n):
            g.add_edge(i, i + 1)
        self.assertEqual(len(g.get_sccs()), n + 1)
        self.assertEqual(g.topological_sort(), list(range(n + 1)))
        g.add_edge(n, 0)
        self.assertEqual(len(g.get_sccs()), 1)


class TestCallGraph(unittest.TestCase):
    """Tests for call_graph.py."""

    def test_parse_stack_sizes(self) -> None:
        with open(TESTDATA_DIR / "test_stack_sizes.json", "r") as f:
            data = call_graph.single_file(json.load(f))
        sizes = call_graph.parse_stack_sizes(data)
        self.assertEqual(sizes["foo"], 16)
        self.assertEqual(sizes["bar"], 32)
        self.assertEqual(sizes["main"], 64)

    def test_extract_type_id_mapping(self) -> None:
        with open(TESTDATA_DIR / "test_indirect.json", "r") as f:
            data = call_graph.single_file(json.load(f))
        mapping = call_graph.extract_type_id_mapping(data)
        self.assertIn(12345, mapping)
        self.assertCountEqual(mapping[12345], [2000, 3000])

    def test_build_call_graph(self) -> None:
        with open(TESTDATA_DIR / "test_call_graph.json", "r") as f:
            cg_data = json.load(f)
        with open(TESTDATA_DIR / "test_stack_sizes.json", "r") as f:
            ss_data = call_graph.single_file(json.load(f))

        stack_sizes = call_graph.parse_stack_sizes(ss_data)
        cg, _, func_info, _ = call_graph.build_call_graph(cg_data, stack_sizes)

        self.assertIn(6080, cg.vertices)  # main
        self.assertCountEqual(cg.graph[6080], [6032, 6048, 6064])
        self.assertEqual(func_info[6080]["stack_usage"], 64)

    def test_get_display_names(self) -> None:
        data = {
            "CallGraph": [
                {
                    "Function": {
                        "Names": ["sym_c"],
                        "Address": 0x3000,
                        "DirectCallees": [
                            {"Names": ["uninst_d"], "Address": 0x4000},
                            {"Address": 0},
                        ],
                    }
                }
            ],
        }
        names = call_graph.get_display_names(data)
        self.assertEqual(names, {0x3000: ["sym_c"], 0x4000: ["uninst_d"]})

    def test_uninstrumented_callees_in_adj_list(self) -> None:
        data = {
            "CallGraph": [
                {
                    "Function": {
                        "Names": ["caller"],
                        "Address": 0x1000,
                        "DirectCallees": [
                            {"Names": ["ext_func"], "Address": 0x9000}
                        ],
                    }
                }
            ]
        }
        _, _, _, adj_list = call_graph.build_call_graph(data, {"caller": 32})
        self.assertEqual(adj_list[0x1000]["uninstrumented"], [0x9000])

    def test_excluded_callee_is_not_uninstrumented(self) -> None:
        data = {"CallGraph": [_func(0x10, "f", [0x20]), _func(0x20, "panic")]}
        cg, _, _, adj_list = call_graph.build_call_graph(
            data, {}, {"exclude_functions": ["panic"]}
        )
        self.assertNotIn(0x20, cg.vertices)
        self.assertEqual(adj_list[0x10]["direct"], [])
        self.assertEqual(adj_list[0x10]["uninstrumented"], [])

    def test_excluded_by_address(self) -> None:
        # Two local functions share a name; an address excludes one of them,
        # whether or not the name is given too. An address also excludes a
        # callee that has no call graph record.
        data = {
            "CallGraph": [
                _func(0x10, "f", [0x20, 0x30, 0x40]),
                _func(0x20, "helper"),
                _func(0x30, "helper"),
            ]
        }
        for excluded in (
            {"address": 0x30},
            {"name": "helper", "address": 0x30},
        ):
            cg, _, _, adj_list = call_graph.build_call_graph(
                data, {}, {"exclude_functions": [excluded, {"address": 0x40}]}
            )
            self.assertIn(0x20, cg.vertices)
            self.assertNotIn(0x30, cg.vertices)
            self.assertEqual(adj_list[0x10]["direct"], [0x20])
            self.assertEqual(adj_list[0x10]["uninstrumented"], [])

    def test_single_file(self) -> None:
        file_entry = {"CallGraph": [_func(0x10, "f")]}
        self.assertIs(call_graph.single_file([file_entry]), file_entry)
        # The addresses of different files are unrelated.
        for output in ([file_entry] * 2, [], file_entry):
            with self.assertRaises(ValueError):
                call_graph.single_file(output)


class TestStackEstimator(unittest.TestCase):
    """Tests for stack_estimator.py."""

    def test_calculate_max_stack_usage_dag(self) -> None:
        """Tests calculation of max stack usage for a Directed Acyclic Graph."""
        # A -> B (10), A -> C (20)
        # B -> D (5), C -> D (5)
        # A: 100, B: 50, C: 60, D: 10
        cg = graph.Graph()
        cg.add_edge(1, 2)
        cg.add_edge(1, 3)
        cg.add_edge(2, 4)
        cg.add_edge(3, 4)

        func_info = {
            1: {"stack_usage": 100, "label": "A"},
            2: {"stack_usage": 50, "label": "B"},
            3: {"stack_usage": 60, "label": "C"},
            4: {"stack_usage": 10, "label": "D"},
        }
        sccs = [[1], [2], [3], [4]]

        results = stack_estimator.calculate_max_stack_usage(cg, func_info, sccs)

        # Max path: A -> C -> D = 100 + 60 + 10 = 170
        # SCC IDs might vary, find SCC for 1
        a_scc_id = next(i for i, scc in enumerate(sccs) if 1 in scc)
        self.assertEqual(results[a_scc_id]["max_stack_usage"], 170)

    def test_calculate_max_stack_usage_cycle(self) -> None:
        """Tests calculation of max stack usage when there are cycles (SCCs)."""
        # A -> B -> A (Cycle)
        # B -> C
        # A: 10, B: 20, C: 30
        cg = graph.Graph()
        cg.add_edge(1, 2)
        cg.add_edge(2, 1)
        cg.add_edge(2, 3)

        func_info = {
            1: {"stack_usage": 10, "label": "A"},
            2: {"stack_usage": 20, "label": "B"},
            3: {"stack_usage": 30, "label": "C"},
        }
        # Tarjan should give [[3], [1, 2]] or similar
        sccs = cg.get_sccs()

        results = stack_estimator.calculate_max_stack_usage(cg, func_info, sccs)

        # SCC {A, B} total = 10 + 20 = 30
        # SCC {C} total = 30
        # Max path from {A, B}: 30 + 30 = 60
        ab_scc_id = next(i for i, scc in enumerate(sccs) if 1 in scc)
        self.assertEqual(results[ab_scc_id]["max_stack_usage"], 60)

    def test_indirect_calls(self) -> None:
        with open(TESTDATA_DIR / "test_indirect.json", "r") as f:
            data = call_graph.single_file(json.load(f))

        stack_sizes = call_graph.parse_stack_sizes(data)
        cg, name_to_addr, func_info, _ = call_graph.build_call_graph(
            data, stack_sizes
        )

        sccs = cg.get_sccs()
        results = stack_estimator.calculate_max_stack_usage(cg, func_info, sccs)

        # caller (10) calls target1 (20) and target2 (30) indirectly
        # Max stack should be 10 + max(20, 30) = 40
        caller_addr = name_to_addr["caller"]
        caller_scc_id = next(
            i for i, scc in enumerate(sccs) if caller_addr in scc
        )
        self.assertEqual(results[caller_scc_id]["max_stack_usage"], 40)

    def test_end_to_end_combined(self) -> None:
        with open(TESTDATA_DIR / "test_combined.json", "r") as f:
            data = call_graph.single_file(json.load(f))

        stack_sizes = call_graph.parse_stack_sizes(data)
        cg, name_to_addr, func_info, _ = call_graph.build_call_graph(
            data, stack_sizes
        )

        self.assertIn("main", name_to_addr)
        entry_addr = name_to_addr["main"]

        sccs = cg.get_sccs()
        results = stack_estimator.calculate_max_stack_usage(cg, func_info, sccs)

        main_scc_id = next(i for i, scc in enumerate(sccs) if entry_addr in scc)
        max_stack = results[main_scc_id]["max_stack_usage"]

        # main (64) calls foo (16), bar (32), baz (8)
        # Total = 64 + 32 = 96
        self.assertEqual(max_stack, 96)


class TestStackEstimatorEndToEnd(unittest.TestCase):
    """End-to-End tests simulating CLI usage."""

    def test_multiple_entry_functions(self) -> None:
        """Tests that the script handles multiple entry functions."""
        with tempfile.TemporaryDirectory() as tmp_dir:
            config_path = Path(tmp_dir) / "test_config.json"
            # Create a temporary config file referencing the existing test data
            config_data = {"entry_functions": ["main", "foo"]}
            with open(config_path, "w") as f:
                json.dump(config_data, f)

            callgraph_path = TESTDATA_DIR / "test_combined.json"

            # Capture stdout from a mock run
            # Using patch to redirect stdout and simulate sys.argv
            with patch.object(sys, "stdout", new=io.StringIO()) as fake_stdout:
                with patch.object(
                    sys,
                    "argv",
                    [
                        "stack_estimator.py",
                        str(config_path),
                        str(callgraph_path),
                    ],
                ):
                    stack_estimator.main()

            output_str = fake_stdout.getvalue()
            output_json = json.loads(output_str)

            entry_funcs = output_json["entry_functions"]
            self.assertEqual(len(entry_funcs), 2)

            main_result = next(
                r for r in entry_funcs if r["entry_function"] == "main"
            )
            self.assertEqual(main_result["max_stack_usage"], 96)

            foo_result = next(
                r for r in entry_funcs if r["entry_function"] == "foo"
            )
            self.assertEqual(foo_result["max_stack_usage"], 16)

    def test_dict_entry_functions_and_uninstrumented(self) -> None:
        """Tests dictionary entry_functions and uninstrumented callee reporting."""
        with tempfile.TemporaryDirectory() as tmp_dir:
            config_path = Path(tmp_dir) / "test_config.json"
            cg_path = Path(tmp_dir) / "test_cg.json"
            config_data = {
                "entry_functions": [{"name": "caller", "address": 0x1000}]
            }
            cg_data = [
                {
                    "StackSizes": [
                        {"Entry": {"Functions": ["caller"], "Size": 48}}
                    ],
                    "CallGraph": [
                        {
                            "Function": {
                                "Names": ["caller"],
                                "Address": 0x1000,
                                "DirectCallees": [
                                    {"Names": ["ext_callee"], "Address": 0x9000}
                                ],
                            }
                        }
                    ],
                }
            ]
            with open(config_path, "w") as f:
                json.dump(config_data, f)
            with open(cg_path, "w") as f:
                json.dump(cg_data, f)

            res = stack_estimator.estimate(config_path, cg_path)
            entry_res = res["entry_functions"][0]
            self.assertEqual(entry_res["entry_function"], "caller")
            self.assertEqual(entry_res["address"], 0x1000)
            self.assertEqual(entry_res["max_stack_usage"], 48)
            self.assertTrue(entry_res["is_lower_bound"])
            self.assertEqual(
                entry_res["uninstrumented_callees"],
                [
                    {
                        "name": "ext_callee",
                        "address": 0x9000,
                        "callers": [0x1000],
                    }
                ],
            )
            self.assertEqual(res["nodes"][0x1000]["uninstrumented"], [0x9000])

    def estimate(
        self, config: dict[str, Any], records: list[Any], stack_sizes: Any
    ) -> dict[str, Any]:
        with tempfile.TemporaryDirectory() as tmp_dir:
            config_path = Path(tmp_dir) / "config.json"
            cg_path = Path(tmp_dir) / "cg.json"
            config_path.write_text(json.dumps(config))
            cg_path.write_text(
                json.dumps(
                    [
                        {
                            "CallGraph": records,
                            "StackSizes": [
                                _stack_size(n, size) for n, size in stack_sizes
                            ],
                        }
                    ]
                )
            )
            return stack_estimator.estimate(config_path, cg_path)

    def test_ambiguous_entry_function(self) -> None:
        res = self.estimate(
            {"entry_functions": ["helper", {"name": "helper", "address": 2}]},
            [_func(1, "helper"), _func(2, "helper")],
            [("helper", 8), ("helper", 16)],
        )
        ambiguous, by_address = res["entry_functions"]
        self.assertIn("Ambiguous", ambiguous["error"])
        self.assertEqual(by_address["address"], 2)
        self.assertEqual(by_address["max_stack_usage"], 16)

    def test_unknown_entry_address(self) -> None:
        # An address is authoritative: it is not resolved by name instead,
        # even if the name is unique.
        res = self.estimate(
            {"entry_functions": [{"name": "main", "address": 0x999}]},
            [_func(1, "main")],
            [("main", 8)],
        )
        (main,) = res["entry_functions"]
        self.assertEqual(main["error"], "Entry function not found")
        self.assertEqual(main["address"], 0x999)


def _func(
    addr: int,
    name: str,
    callees: Sequence[int] = (),
    indirect: Sequence[int] = (),
    type_id: int = 0,
    indirect_target: bool = False,
) -> dict[str, Any]:
    """Returns a CallGraph entry in llvm-readelf JSON format."""
    return {
        "Function": {
            "Names": [name],
            "Address": addr,
            "Version": 0,
            "IsIndirectTarget": indirect_target,
            "TypeID": type_id,
            "DirectCallees": [{"Address": c} for c in callees],
            "IndirectTypeIDs": list(indirect),
        }
    }


def _stack_size(name: str, size: int) -> dict[str, Any]:
    """Returns a StackSizes entry in llvm-readelf JSON format."""
    return {"Entry": {"Functions": [name], "Size": size}}


def main() -> None:
    # unittest.main() can't parse arguments which are not its own,
    # so we need to parse our arguments first.
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--testdata-dir",
        required=True,
        type=pathlib.Path,
        help="Path to testdata",
    )
    # unittest.main() will see the leftover args and complain.
    # We use parse_known_args to separate them out.
    args, remaining_argv = parser.parse_known_args()

    # Make TESTDATA_DIR available to the tests.
    global TESTDATA_DIR
    TESTDATA_DIR = args.testdata_dir

    # Now, run unittest with its own arguments, plus sys.argv[0]
    unittest.main(argv=[sys.argv[0]] + remaining_argv)


if __name__ == "__main__":
    main()

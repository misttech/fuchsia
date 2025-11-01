#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import sys
import unittest
from pathlib import Path
from unittest import mock

_SCRIPT_DIR = os.path.dirname(__file__)
sys.path.insert(0, _SCRIPT_DIR)

import bazel_rust_analyzer_utils


class TestBazelRustAnalyzerUtils(unittest.TestCase):
    @mock.patch("build_utils.BazelPaths")
    def test_substitute_tokens(self, MockBazelPaths) -> None:
        mock_paths = MockBazelPaths()
        mock_paths.fuchsia_dir = Path("/fuchsia_dir")
        mock_paths.workspace = Path("/workspace")
        mock_paths.execroot = Path("/execroot")
        mock_paths.output_base = Path("/output_base")

        test_cases = [
            ("Hello __WORKSPACE__", "Hello /fuchsia_dir"),
            ("Path is ${pwd}/foo", "Path is /execroot/foo"),
            ("Exec root: __EXEC_ROOT__", "Exec root: /execroot"),
            ("Output base: __OUTPUT_BASE__", "Output base: /output_base"),
            (
                "Combined: __WORKSPACE__ ${pwd} __EXEC_ROOT__ __OUTPUT_BASE__",
                "Combined: /fuchsia_dir /execroot /execroot /output_base",
            ),
            ("No tokens here", "No tokens here"),
        ]

        for input_str, expected_str in test_cases:
            with self.subTest(input_str=input_str):
                result = bazel_rust_analyzer_utils.substitute_tokens(
                    input_str, mock_paths
                )
                self.assertEqual(result, expected_str)

    @mock.patch("build_utils.BazelPaths")
    @mock.patch("pathlib.Path.read_text")
    def test_load_crate_spec_from_json_empty(
        self, mock_read_text, MockBazelPaths
    ) -> None:
        mock_paths = MockBazelPaths()
        mock_read_text.return_value = "{}"
        result = bazel_rust_analyzer_utils.load_crate_spec_from_json(
            Path("_unused_dummy.json"), mock_paths
        )
        self.assertEqual(result, {})

    @mock.patch("build_utils.BazelPaths")
    @mock.patch("pathlib.Path.read_text")
    def test_load_crate_spec_from_json(
        self, mock_read_text, MockBazelPaths
    ) -> None:
        mock_paths = MockBazelPaths()
        mock_paths.fuchsia_dir = Path("/fuchsia_dir")
        mock_paths.workspace = Path("/workspace")
        mock_paths.execroot = Path("/execroot")
        mock_paths.output_base = Path("/output_base")

        mock_read_text.return_value = (
            '{"root_module": "__WORKSPACE__/src/lib.rs"}'
        )

        expected_spec = {"root_module": "/fuchsia_dir/src/lib.rs"}

        result = bazel_rust_analyzer_utils.load_crate_spec_from_json(
            Path("_unused_dummy.json"), mock_paths
        )
        self.assertEqual(result, expected_spec)
        mock_read_text.assert_called_once()

    def test_convert_crate_specs_to_rust_project_crates(self) -> None:
        crate_specs = [
            {
                "crate_id": "lib_a",
                "display_name": "lib_a",
                "edition": "2021",
                "root_module": "a/src/lib.rs",
                "is_workspace_member": True,
                "deps": ["lib_b"],
                "cfg": ["test"],
                "env": {},
                "target": "x86_64-unknown-linux-gnu",
                "crate_type": "rlib",
            },
            {
                "crate_id": "lib_b",
                "display_name": "lib_b",
                "edition": "2021",
                "root_module": "b/src/lib.rs",
                "is_workspace_member": True,
                "deps": [],
                "cfg": [],
                "env": {},
                "target": "x86_64-unknown-linux-gnu",
                "crate_type": "rlib",
                "source": {"include_dirs": ["b/src"], "exclude_dirs": []},
                "build": {"label": "//b:b", "build_file": "b/BUILD.bazel"},
            },
            {
                "crate_id": "bin_c",
                "display_name": "bin_c",
                "edition": "2021",
                "root_module": "c/src/main.rs",
                "is_workspace_member": True,
                "deps": ["lib_a"],
                "cfg": [],
                "env": {},
                "target": "x86_64-unknown-linux-gnu",
                "crate_type": "bin",
                "is_test": False,
            },
        ]

        expected_crates = [
            {
                "crate_id": 0,
                "display_name": "lib_a",
                "root_module": "a/src/lib.rs",
                "edition": "2021",
                "deps": [{"crate": 1, "name": "lib_b"}],
                "is_workspace_member": True,
                "cfg": ["test"],
                "target": "x86_64-unknown-linux-gnu",
                "env": {},
                "is_proc_macro": False,
                "proc_macro_dylib_path": None,
                "build": None,
            },
            {
                "crate_id": 1,
                "display_name": "lib_b",
                "root_module": "b/src/lib.rs",
                "edition": "2021",
                "deps": [],
                "is_workspace_member": True,
                "source": {"include_dirs": ["b/src"], "exclude_dirs": []},
                "cfg": [],
                "target": "x86_64-unknown-linux-gnu",
                "env": {},
                "is_proc_macro": False,
                "proc_macro_dylib_path": None,
                "build": {
                    "label": "//b:b",
                    "build_file": "b/BUILD.bazel",
                    "target_kind": "lib",
                },
            },
            {
                "crate_id": 2,
                "display_name": "bin_c",
                "root_module": "c/src/main.rs",
                "edition": "2021",
                "deps": [{"crate": 0, "name": "lib_a"}],
                "is_workspace_member": True,
                "cfg": [],
                "target": "x86_64-unknown-linux-gnu",
                "env": {},
                "is_proc_macro": False,
                "proc_macro_dylib_path": None,
                "build": None,
            },
        ]

        result = bazel_rust_analyzer_utils.convert_crate_specs_to_rust_project_crates(
            crate_specs
        )
        self.assertEqual(result, expected_crates)

    @mock.patch("subprocess.check_output")
    @mock.patch("build_utils.BazelPaths")
    def test_aquery_rust_analyzer_outputs(
        self, MockBazelPaths, mock_check_output
    ) -> None:
        mock_paths = MockBazelPaths()
        mock_paths.launcher = Path("/path/to/bazel")
        mock_paths.workspace = Path("/workspace")

        # Sample aquery output
        aquery_output = {
            "artifacts": [
                {"id": 1, "pathFragmentId": 4},
                {"id": 2, "pathFragmentId": 7},  # New artifact
            ],
            "pathFragments": [
                {"id": 0, "label": "bazel-out"},
                {"id": 1, "parentId": 0, "label": "k8-fastbuild"},
                {"id": 2, "parentId": 1, "label": "bin"},
                {"id": 3, "parentId": 2, "label": "foo"},
                {
                    "id": 4,
                    "parentId": 3,
                    "label": "bar.rust_analyzer_crate_spec.json",
                },
                {"id": 5, "parentId": 2, "label": "another"},
                {"id": 6, "parentId": 5, "label": "package"},
                {
                    "id": 7,
                    "parentId": 6,
                    "label": "lib.rust_analyzer_crate_spec.json",
                },
            ],
            "actions": [{"outputIds": [1]}, {"outputIds": [2]}],
        }
        mock_check_output.return_value = json.dumps(aquery_output)
        result = bazel_rust_analyzer_utils.aquery_rust_analyzer_outputs(
            mock_paths, ["--config=unused_config"], ["//unused:target"]
        )
        self.assertEqual(
            result,
            [
                Path(
                    "/workspace/bazel-out/k8-fastbuild/bin/foo/bar.rust_analyzer_crate_spec.json"
                ),
                Path(
                    "/workspace/bazel-out/k8-fastbuild/bin/another/package/lib.rust_analyzer_crate_spec.json"
                ),
            ],
        )

    @mock.patch("subprocess.check_output")
    @mock.patch("build_utils.BazelPaths")
    def test_aquery_rust_analyzer_outputs_empty(
        self, MockBazelPaths, mock_check_output
    ) -> None:
        mock_paths = MockBazelPaths()
        mock_paths.launcher = Path("/path/to/bazel")
        mock_paths.workspace = Path("/workspace")

        aquery_output = {
            "artifacts": [],
            "pathFragments": [],
            "actions": [],
        }
        mock_check_output.return_value = json.dumps(aquery_output)
        result = bazel_rust_analyzer_utils.aquery_rust_analyzer_outputs(
            mock_paths, ["--config=unused_config"], ["//unused:target"]
        )
        self.assertEqual(result, [])

    @mock.patch(
        "bazel_rust_analyzer_utils.convert_crate_specs_to_rust_project_crates"
    )
    @mock.patch("bazel_rust_analyzer_utils.load_crate_spec_from_json")
    @mock.patch("bazel_rust_analyzer_utils.aquery_rust_analyzer_outputs")
    @mock.patch("bazel_rust_analyzer_utils.build_rust_analyzer_aspect")
    @mock.patch("build_utils.BazelPaths")
    def test_generate_rust_project_json_crates(
        self,
        MockBazelPaths,
        mock_build_aspect,
        mock_aquery,
        mock_load_spec,
        mock_convert_specs,
    ) -> None:
        mock_paths = MockBazelPaths()
        bazel_args = ["--config=unused_config"]
        targets = ["//unused:target"]

        mock_aquery.return_value = [Path("a.json"), Path("b.json")]
        mock_load_spec.side_effect = [{"crate_id": "a"}, {"crate_id": "b"}]
        mock_convert_specs.return_value = [
            {"display_name": "a"},
            {"display_name": "b"},
        ]

        result = bazel_rust_analyzer_utils.generate_rust_project_json_crates(
            mock_paths, bazel_args, targets
        )

        mock_build_aspect.assert_called_once_with(
            mock_paths, bazel_args, targets
        )
        mock_aquery.assert_called_once_with(mock_paths, bazel_args, targets)
        mock_load_spec.assert_has_calls(
            [
                mock.call(Path("a.json"), mock_paths),
                mock.call(Path("b.json"), mock_paths),
            ]
        )
        mock_convert_specs.assert_called_once_with(
            [{"crate_id": "a"}, {"crate_id": "b"}]
        )
        self.assertEqual(result, [{"display_name": "a"}, {"display_name": "b"}])

    @mock.patch("bazel_rust_analyzer_utils.aquery_rust_analyzer_outputs")
    @mock.patch("bazel_rust_analyzer_utils.build_rust_analyzer_aspect")
    @mock.patch("build_utils.BazelPaths")
    def test_generate_rust_project_json_crates_no_outputs(
        self, MockBazelPaths, mock_build_aspect, mock_aquery
    ) -> None:
        mock_paths = MockBazelPaths()
        bazel_args = ["--config=unused_config"]
        targets = ["//unused:target"]

        mock_aquery.return_value = []  # No output files

        result = bazel_rust_analyzer_utils.generate_rust_project_json_crates(
            mock_paths, bazel_args, targets
        )

        mock_build_aspect.assert_called_once_with(
            mock_paths, bazel_args, targets
        )
        mock_aquery.assert_called_once_with(mock_paths, bazel_args, targets)
        self.assertEqual(result, [])

    def test_merge_rust_project_jsons_basic(self) -> None:
        base_json = {
            "sysroot": "sysroot",
            "crates": [
                {
                    "crate_id": 0,
                    "root_module": "a.rs",
                    "target": "host",
                    "deps": [],
                }
            ],
        }
        merge_json = {
            "sysroot": "sysroot",
            "crates": [
                {
                    "crate_id": 0,
                    "root_module": "b.rs",
                    "target": "host",
                    "deps": [{"crate": 0, "name": "self_dep"}],
                }
            ],
        }
        expected_json = {
            "sysroot": "sysroot",
            "crates": [
                {
                    "crate_id": 0,
                    "root_module": "a.rs",
                    "target": "host",
                    "deps": [],
                },
                {
                    "crate_id": 1,  # Remapped from 0 + (0 + 1)
                    "root_module": "b.rs",
                    "target": "host",
                    "deps": [{"crate": 1, "name": "self_dep"}],
                },
            ],
        }
        result = bazel_rust_analyzer_utils.merge_rust_project_jsons(
            base_json, [merge_json]
        )
        self.assertEqual(result, expected_json)

    def test_merge_rust_project_jsons_deduplication(self) -> None:
        base_json = {
            "crates": [{"crate_id": 0, "root_module": "a.rs", "target": "host"}]
        }
        merge_json = {
            "crates": [
                {"crate_id": 5, "root_module": "a.rs", "target": "host"},
                {
                    "crate_id": 6,
                    "root_module": "b.rs",
                    "target": "host",
                    "deps": [{"crate": 5, "name": "a"}],
                },
            ]
        }
        expected_json = {
            "crates": [
                {
                    "crate_id": 0,
                    "root_module": "a.rs",
                    "target": "host",
                },
                {
                    "crate_id": 7,  # Remapped from 6 + (0 + 1)
                    "root_module": "b.rs",
                    "target": "host",
                    # Dependency 5 (a.rs) in merge_json is deduplicated to base crate 0.
                    "deps": [{"crate": 0, "name": "a"}],
                },
            ]
        }
        result = bazel_rust_analyzer_utils.merge_rust_project_jsons(
            base_json, [merge_json]
        )
        self.assertEqual(result, expected_json)

    def test_merge_rust_project_jsons_multiple_merges(self) -> None:
        base_json = {
            "crates": [{"crate_id": 0, "root_module": "base.rs", "target": "t"}]
        }
        merge1 = {
            "crates": [{"crate_id": 0, "root_module": "m1.rs", "target": "t"}]
        }
        merge2 = {
            "crates": [{"crate_id": 0, "root_module": "m2.rs", "target": "t"}]
        }

        expected_json = {
            "crates": [
                {"crate_id": 0, "root_module": "base.rs", "target": "t"},
                {
                    "crate_id": 1,
                    "root_module": "m1.rs",
                    "target": "t",
                },  # 0 + (0 + 1)
                {
                    "crate_id": 2,
                    "root_module": "m2.rs",
                    "target": "t",
                },  # 0 + (0 + 1 + 0 + 1)
            ]
        }
        result = bazel_rust_analyzer_utils.merge_rust_project_jsons(
            base_json, [merge1, merge2]
        )
        self.assertEqual(result, expected_json)

    def test_merge_rust_project_jsons_empty_base(self) -> None:
        base_json = {"crates": []}
        merge_json = {
            "crates": [{"crate_id": 10, "root_module": "a.rs", "target": "t"}]
        }
        # Offset starts at -1 + 1 = 0. New ID = 10 + 0 = 10.
        expected_json = {
            "crates": [
                {
                    "crate_id": 10,
                    "root_module": "a.rs",
                    "target": "t",
                }
            ]
        }
        result = bazel_rust_analyzer_utils.merge_rust_project_jsons(
            base_json, [merge_json]
        )
        self.assertEqual(result, expected_json)

    def test_merge_rust_project_jsons_broken_dependency(self) -> None:
        base_json = {
            "crates": [{"crate_id": 0, "root_module": "a.rs", "target": "t"}]
        }
        merge_json = {
            "crates": [
                {
                    "crate_id": 0,
                    "root_module": "b.rs",
                    "target": "t",
                    "deps": [{"crate": 999, "name": "missing"}],
                }
            ]
        }
        # Broken dependency should be dropped to avoid invalid crate references.
        expected_json = {
            "crates": [
                {
                    "crate_id": 0,
                    "root_module": "a.rs",
                    "target": "t",
                },
                {
                    "crate_id": 1,
                    "root_module": "b.rs",
                    "target": "t",
                    "deps": [{"crate": 1000, "name": "missing"}],
                },
            ]
        }
        result = bazel_rust_analyzer_utils.merge_rust_project_jsons(
            base_json, [merge_json]
        )
        self.assertEqual(result, expected_json)


if __name__ == "__main__":
    unittest.main()

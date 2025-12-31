# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import pathlib
import unittest
from unittest import mock

import discover_migration_candidates


class TestDiscoverMigrationCandidates(unittest.TestCase):
    def test_is_simple_dep(self):
        self.assertTrue(
            discover_migration_candidates.is_simple_dep(
                "//third_party/rust_crates/a"
            )
        )
        self.assertTrue(
            discover_migration_candidates.is_simple_dep(
                "//third_party/golibs/a"
            )
        )
        self.assertFalse(
            discover_migration_candidates.is_simple_dep("//third_party/foo")
        )
        self.assertFalse(
            discover_migration_candidates.is_simple_dep("//src/foo")
        )
        self.assertFalse(discover_migration_candidates.is_simple_dep("foo"))

    def test_end_pos_for_single_target(self):
        content = 'target("name") { deps = [] }'
        # Start after '{'
        start_pos = content.find("{") + 1
        end_pos = discover_migration_candidates.end_pos_for_target(
            content, start_pos
        )
        self.assertEqual(content[end_pos - 1], "}")
        self.assertEqual(end_pos, len(content))

        content = 'target("name") { if (true) { } }'
        start_pos = content.find("{") + 1
        end_pos = discover_migration_candidates.end_pos_for_target(
            content, start_pos
        )
        self.assertEqual(end_pos, len(content))

        content = 'target("name") { invalid'
        start_pos = content.find("{") + 1
        with self.assertRaises(ValueError):
            discover_migration_candidates.end_pos_for_target(content, start_pos)

    def test_end_pos_for_multiple_targets(self):
        content = """target("name1") {
            deps = []
        }
        target("name2") {
            sources = []
        }"""
        # Find the start of the first target's body.
        start_pos_name1 = len('target("name1") {')
        end_pos_name1 = discover_migration_candidates.end_pos_for_target(
            content, start_pos_name1
        )
        # Check that it correctly identifies the end of the first target.
        self.assertEqual(content[end_pos_name1 - 1], "}")
        # The character after the '}' of the first target should be a newline or whitespace before the next target.
        self.assertEqual(content[end_pos_name1], "\n")

        # Find the start of the second target's body.
        start_pos_name2 = content.find('target("name2") {') + len(
            'target("name2") {'
        )
        end_pos_name2 = discover_migration_candidates.end_pos_for_target(
            content, start_pos_name2
        )
        self.assertEqual(content[end_pos_name2 - 1], "}")
        self.assertEqual(end_pos_name2, len(content))

    def test_deps_from_target_body(self):
        body = 'deps = [ "//a", "//b" ]'
        deps = discover_migration_candidates.deps_from_target_body(body)
        self.assertEqual(deps, ["//a", "//b"])

        body = 'deps += [ "//c" ]'
        deps = discover_migration_candidates.deps_from_target_body(body)
        self.assertEqual(deps, ["//c"])

        body = 'deps = [ "//a",\n "//b" ]'
        deps = discover_migration_candidates.deps_from_target_body(body)
        self.assertEqual(deps, ["//a", "//b"])

        body = """
            deps = ["//a", "//b"]
            deps += ["//c"]
            inputs = ["//d"]
            deps += ["//e"]
        """
        deps = discover_migration_candidates.deps_from_target_body(body)
        self.assertEqual(deps, ["//a", "//b", "//c", "//e"])

        body = "no_deps = []"
        deps = discover_migration_candidates.deps_from_target_body(body)
        self.assertEqual(deps, [])

        body = """
            deps = ["//a", "//b"]
            public_deps = ["//public_a"]
            data = ["//data"]
            public_deps += ["//public_b"]
        """
        deps = discover_migration_candidates.deps_from_target_body(body)
        self.assertEqual(
            deps, ["//a", "//b", "//public_a", "//data", "//public_b"]
        )

    def test_fields_from_target_body(self):
        body = 'sources = ["a.cc"]\nconfigs += ["//c"]'
        fields = discover_migration_candidates.fields_from_target_body(body)
        self.assertEqual(sorted(fields), ["configs", "sources"])

        body = 'inputs = ["a.cc"]\ninputs += ["b.cc"]'
        fields = discover_migration_candidates.fields_from_target_body(body)
        self.assertEqual(sorted(fields), ["inputs"])

    def test_targets_from_gn_file(self):
        content = """
        source_set("lib") {
            sources = ["lib.cc"]
        }
        executable("bin") {
            deps = [":lib"]
        }
        """
        with mock.patch.object(pathlib.Path, "read_text", return_value=content):
            path = pathlib.Path("BUILD.gn")
            targets = discover_migration_candidates.targets_from_gn_file(
                path, ["source_set", "executable"]
            )
            self.assertEqual(
                targets,
                [
                    {
                        "path": path,
                        "name": "lib",
                        "type": "source_set",
                        "deps": [],
                        "fields": ["sources"],
                    },
                    {
                        "path": path,
                        "name": "bin",
                        "type": "executable",
                        "deps": [":lib"],
                        "fields": ["deps"],
                    },
                ],
            )

    def test_complexity_scores_by_file(self):
        targets = [
            {
                "path": "BUILD.gn",
                "name": "t1",
                "type": "rustc_binary",
                "deps": ["//third_party/rust_crates/a", "//src/b", "//src/c"],
                "fields": [
                    "sources",
                    "configs",
                    "deps",
                    "check_includes",
                    "metadata",
                ],
            },
            {
                "path": "BUILD.gn",
                "name": "t2",
                "type": "go_binary",
                "deps": [
                    "//third_party/golibs/a",
                    "//third_party/something_else",
                ],
                "fields": ["sources", "deps", "assert_no_deps"],
            },
            {
                "path": "another/BUILD.gn",
                "name": "t3",
                "type": "shared_library",
                "deps": ["//src/baz", "//src/qux"],
                "fields": ["sources", "inputs", "deps", "metadata"],
            },
        ]
        results = discover_migration_candidates.complexity_scores_by_file(
            targets
        )
        self.maxDiff = None
        self.assertEqual(
            results,
            [
                {
                    "path": "another/BUILD.gn",
                    "targets": [
                        {
                            "name": "t3",
                            "non_standard_fields": ["metadata"],
                            "non_simple_deps": ["//src/baz", "//src/qux"],
                            "simple_deps": [],
                            "type": "shared_library",
                            "complexity_score": 3,
                        },
                    ],
                    "total_complexity": 3,
                    "total_targets": 1,
                },
                {
                    "path": "BUILD.gn",
                    "targets": [
                        {
                            "name": "t1",
                            "non_standard_fields": [
                                "configs",
                                "check_includes",
                                "metadata",
                            ],
                            "non_simple_deps": ["//src/b", "//src/c"],
                            "simple_deps": ["//third_party/rust_crates/a"],
                            "type": "rustc_binary",
                            "complexity_score": 6,
                        },
                        {
                            "name": "t2",
                            "non_standard_fields": ["assert_no_deps"],
                            "non_simple_deps": ["//third_party/something_else"],
                            "simple_deps": ["//third_party/golibs/a"],
                            "type": "go_binary",
                            "complexity_score": 2,
                        },
                    ],
                    "total_complexity": 8,
                    "total_targets": 2,
                },
            ],
        )

    def test_bazel_targets_in_dir(self):
        content = 'cc_library(name = "lib")\ncc_binary(name = "bin")'
        with mock.patch.object(
            pathlib.Path, "exists", return_value=True
        ), mock.patch.object(pathlib.Path, "read_text", return_value=content):
            targets = discover_migration_candidates.bazel_targets_in_dir(
                pathlib.Path(".")
            )
            self.assertEqual(targets, {"lib", "bin"})

        with mock.patch.object(pathlib.Path, "exists", return_value=False):
            targets = discover_migration_candidates.bazel_targets_in_dir(
                pathlib.Path(".")
            )
            self.assertEqual(targets, set())


if __name__ == "__main__":
    unittest.main()

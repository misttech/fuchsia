#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for generic BEP stream decoding in bazel_build_events.py."""

import json
import os
import tempfile
import unittest
from pathlib import Path

from build.bazel.scripts import bazel_build_events


class BazelBuildEventsTest(unittest.TestCase):
    def test_file_execroot_relpath(self) -> None:
        # Case 1: path_prefix provided
        f1 = bazel_build_events.File(
            name="fake/dir/sample.json",
            path_prefix=("fake-out", "fake-config", "bin"),
        )
        self.assertEqual(
            f1.execroot_relpath(),
            os.path.join(
                "fake-out", "fake-config", "bin", "fake/dir/sample.json"
            ),
        )

        # Case 2: name already starts with bazel-out/
        f2 = bazel_build_events.File(name="bazel-out/fake-cfg/bin/sample.json")
        self.assertEqual(
            f2.execroot_relpath(), "bazel-out/fake-cfg/bin/sample.json"
        )

        # Case 3: uri with file:// relative to execroot
        f3 = bazel_build_events.File(
            name="sample.json",
            uri="file:///fake/execroot/bazel-out/bin/sample.json",
        )
        self.assertEqual(
            f3.execroot_relpath(execroot=Path("/fake/execroot")),
            os.path.join("bazel-out", "bin", "sample.json"),
        )

        # Case 4: fallback to name
        f4 = bazel_build_events.File(name="plain_name.json")
        self.assertEqual(f4.execroot_relpath(), "plain_name.json")

    def test_from_events_empty(self) -> None:
        stream = bazel_build_events.BuildEventStream.from_events([])
        self.assertEqual(stream.named_sets, {})
        self.assertEqual(stream.targets_completed, [])

    def test_get_output_group_files(self) -> None:
        events = [
            {
                "id": {"namedSet": {"id": "fake-set-1"}},
                "namedSetOfFiles": {
                    "files": [
                        {
                            "name": "fake/pkg/file_a.txt",
                            "pathPrefix": ["fake-out", "bin"],
                        }
                    ]
                },
            },
            {
                "id": {
                    "targetCompleted": {
                        "label": "//fake/pkg:target1",
                        "aspect": "//fake/aspects:sample.bzl%fake_aspect",
                    }
                },
                "completed": {
                    "outputGroup": [
                        {
                            "name": "fake_group",
                            "fileSets": [{"id": "fake-set-1"}],
                        }
                    ]
                },
            },
        ]
        stream = bazel_build_events.BuildEventStream.from_events(events)
        expected = os.path.join("fake-out", "bin", "fake/pkg/file_a.txt")
        self.assertEqual(
            stream.get_output_group_files("fake_group"), [expected]
        )
        self.assertEqual(stream.get_output_group_files("nonexistent_group"), [])

    def test_get_target_output_group_files(self) -> None:
        events = [
            {
                "id": {"namedSet": {"id": "fake-set-target"}},
                "namedSetOfFiles": {
                    "files": [
                        {
                            "name": "fake/query/output.txt",
                            "pathPrefix": ["fake-out", "bin"],
                        }
                    ]
                },
            },
            {
                "id": {
                    "targetCompleted": {
                        "label": "//fake/scope:query_target",
                    }
                },
                "completed": {
                    "outputGroup": [
                        {
                            "name": "default",
                            "fileSets": [{"id": "fake-set-target"}],
                        }
                    ]
                },
            },
        ]
        stream = bazel_build_events.BuildEventStream.from_events(events)
        expected_file = os.path.join("fake-out", "bin", "fake/query/output.txt")
        pairs = stream.get_target_output_group_files(
            "default",
            label_predicate=lambda l: "fake/scope" in l,
        )
        self.assertEqual(pairs, [("//fake/scope:query_target", expected_file)])

    def test_resolve_nested_named_sets(self) -> None:
        events = [
            {
                "id": {"namedSet": {"id": "child-set-1"}},
                "namedSetOfFiles": {
                    "files": [
                        {
                            "name": "fake/child1.txt",
                            "pathPrefix": ["fake-out", "bin"],
                        }
                    ]
                },
            },
            {
                "id": {"namedSet": {"id": "child-set-2"}},
                "namedSetOfFiles": {
                    "files": [
                        {
                            "name": "fake/child2.txt",
                            "pathPrefix": ["fake-out", "bin"],
                        }
                    ],
                    "fileSets": [{"id": "child-set-1"}],
                },
            },
            {
                "id": {"namedSet": {"id": "root-set"}},
                "namedSetOfFiles": {
                    "fileSets": [{"id": "child-set-2"}, {"id": "child-set-1"}],
                },
            },
            {
                "id": {"targetCompleted": {"label": "//fake:root"}},
                "completed": {
                    "outputGroup": [
                        {
                            "name": "fake_root_group",
                            "fileSets": [{"id": "root-set"}],
                        }
                    ]
                },
            },
        ]
        stream = bazel_build_events.BuildEventStream.from_events(events)
        p1 = os.path.join("fake-out", "bin", "fake/child1.txt")
        p2 = os.path.join("fake-out", "bin", "fake/child2.txt")
        self.assertEqual(
            stream.get_output_group_files("fake_root_group"), [p2, p1]
        )

    def test_from_lines_and_from_file(self) -> None:
        lines = [
            json.dumps(
                {
                    "id": {"namedSet": {"id": "set-1"}},
                    "namedSetOfFiles": {
                        "files": [
                            {
                                "name": "fake/target.txt",
                                "pathPrefix": ["fake-out", "bin"],
                            }
                        ]
                    },
                }
            ),
            "",
            "invalid json line",
            json.dumps(
                {
                    "id": {"targetCompleted": {"label": "//fake:target"}},
                    "completed": {
                        "outputGroup": [
                            {
                                "name": "fake_group",
                                "fileSets": [{"id": "set-1"}],
                            }
                        ]
                    },
                }
            ),
        ]
        stream = bazel_build_events.BuildEventStream.from_lines(lines)
        expected = os.path.join("fake-out", "bin", "fake/target.txt")
        self.assertEqual(
            stream.get_output_group_files("fake_group"), [expected]
        )

        with tempfile.TemporaryDirectory() as tmpdir:
            file_path = Path(tmpdir) / "stream.json"
            file_path.write_text("\n".join(lines), encoding="utf-8")
            file_stream = bazel_build_events.BuildEventStream.from_file(
                file_path
            )
            self.assertEqual(
                file_stream.get_output_group_files("fake_group"), [expected]
            )

            missing_stream = bazel_build_events.BuildEventStream.from_file(
                Path(tmpdir) / "nonexistent.json"
            )
            self.assertEqual(
                missing_stream.get_output_group_files("fake_group"), []
            )

    def test_target_completed_default_success(self) -> None:
        tc = bazel_build_events.TargetCompleted(label="//fake:target")
        self.assertFalse(tc.success)

    def test_resolve_named_set_memoization(self) -> None:
        events = [
            {
                "id": {"namedSet": {"id": "set-1"}},
                "namedSetOfFiles": {
                    "files": [
                        {
                            "name": "fake/target.txt",
                            "pathPrefix": ["fake-out", "bin"],
                        }
                    ]
                },
            },
        ]
        stream = bazel_build_events.BuildEventStream.from_events(events)
        self.assertEqual(len(stream._named_set_cache), 0)
        res1 = stream.resolve_named_set_files("set-1")
        self.assertEqual(len(stream._named_set_cache), 1)
        res2 = stream.resolve_named_set_files("set-1")
        self.assertIs(res1, res2)


if __name__ == "__main__":
    unittest.main()

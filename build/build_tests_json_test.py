#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit-tests for build/build_tests_json.py functions."""

import json
import os
import sys
import tempfile
import typing as T
import unittest
from pathlib import Path

_SCRIPT_DIR = os.path.dirname(__file__)
sys.path.insert(0, _SCRIPT_DIR)
import build_tests_json
from build_utils import BazelPaths, CommandRunner, MockCommandRunner
from serialization import instance_from_dict, instance_to_dict


class BuildTestsJsonTest(unittest.TestCase):
    def setUp(self) -> None:
        self.maxDiff = None
        self._td = tempfile.TemporaryDirectory()
        self._dir = Path(self._td.name)
        self.source_dir = self._dir / "source"
        self.source_dir.mkdir()
        (self.source_dir / ".jiri_manifest").touch()

        self.build_dir = self.source_dir / "out" / "not-default"
        self.build_dir.mkdir(parents=True)

        self.output_dir = self._dir / "output"
        self.output_dir.mkdir()

        (self.build_dir / "obj" / "tests").mkdir(parents=True)

        # Compute the Bazel execroot path, relative to the build directory.
        BazelPaths.write_topdir_config_for_test(self.source_dir, "bazel_topdir")
        self.execroot_path = os.path.relpath(
            BazelPaths(self.source_dir, self.build_dir).execroot,
            self.build_dir,
        )
        (self.build_dir / "ungrouped_bazel_target_test_suites.txt").write_text(
            ""
        )
        (self.build_dir / "target_tests.gn_targets_manifest.json").write_text(
            "[]"
        )
        (self.build_dir / "bazel_target_infos.json").write_text(
            json.dumps(
                [
                    {
                        "bazel_target": "//build/bazel/target_tests:target_tests_stamp",
                        "gn_targets_manifest": "target_tests.gn_targets_manifest.json",
                    }
                ]
            )
        )

    def tearDown(self) -> None:
        self._td.cleanup()

    def _test(
        self,
        tests_from_metadata: list[T.Any],
        test_groups: list[T.Any],
        product_bundles: list[T.Any],
        with_bazel_tests: bool = False,
        command_runner: CommandRunner | None = None,
        all_test_environments: dict[str, T.Any] | None = None,
        default_test_environments: dict[str, T.Any] | None = None,
    ) -> tuple[set[Path], list[T.Any]]:
        tests_from_metadata_str = json.dumps(tests_from_metadata)
        tests_from_metadata_path = self.build_dir / "tests_from_metadata.json"
        tests_from_metadata_path.write_text(tests_from_metadata_str)

        test_groups_str = json.dumps(test_groups)
        test_groups_path = (
            self.build_dir / "obj" / "tests" / "product_bundle_test_groups.json"
        )
        test_groups_path.write_text(test_groups_str)

        if all_test_environments is None:
            all_test_environments = {
                "all_environments": [],
                "platforms": [
                    {"device_type": "AEMU", "cpu": "x64"},
                    {"device_type": "QEMU", "cpu": "x64"},
                    {"device_type": "Intel NUC Kit NUC11TNHv5", "cpu": "x64"},
                    {"device_type": "QEMU", "cpu": "arm64"},
                    {
                        "device_type": "QEMU",
                        "cpu": "arm64",
                        "host_device_type": "GCP_C4A_HIGHMEM_96_BM",
                    },
                    {
                        "device_type": "QEMU",
                        "cpu": "arm64",
                        "host_device_type": "AmpereAltraMax-M128-30",
                    },
                    {"device_type": "Vim3", "cpu": "arm64"},
                    {"device_type": "Astro", "cpu": "arm64"},
                    {"os": "Linux", "cpu": "x64"},
                    {"os": "Linux", "cpu": "arm64"},
                ],
                "all_dimension_keys": [
                    "device_type",
                    "cpu",
                    "os",
                    "pool",
                    "testbed",
                    "host_device_type",
                ],
                "host_env": {"dimensions": {"os": "Linux", "cpu": "x64"}},
            }
        environments_path = (
            self.build_dir / "obj" / "tests" / "all_test_environments.json"
        )
        environments_path.write_text(json.dumps(all_test_environments))

        if default_test_environments is None:
            default_test_environments = {
                "target_cpu": "x64",
                "default_environments": [
                    {"dimensions": {"device_type": "AEMU"}}
                ],
                "allowed_device_types": ["AEMU", "QEMU"],
                "allowed_host_device_types": [],
            }
        default_environments_path = (
            self.build_dir / "obj" / "tests" / "default_test_environments.json"
        )
        default_environments_path.write_text(
            json.dumps(default_test_environments)
        )

        product_bundles_str = json.dumps(product_bundles)
        product_bundles_path = self.build_dir / "product_bundles.json"
        product_bundles_path.write_text(product_bundles_str)

        if with_bazel_tests:
            (self.build_dir / "bazel_host_test_suites.txt").write_text(
                "//fake/test1\n//fake/test2"
            )
        inputs = build_tests_json.build_tests_json(
            self.build_dir, with_bazel_tests, command_runner
        )

        tests_string = (self.build_dir / "tests.json").read_text()
        tests = json.loads(tests_string)

        return (inputs, tests)

    def test_only_tests_from_metadata_no_environments(self) -> None:
        tests_from_metadata = [
            {"test": {"name": "test1", "cpu": "x64"}},
            {"test": {"name": "test2", "cpu": "x64"}},
        ]
        product_bundles = [{"name": "my_pb"}]
        (_, tests) = self._test(tests_from_metadata, [], product_bundles)
        expected_tests = [
            {
                "test": {"name": "test1", "cpu": "x64"},
                "environments": [{"dimensions": {"device_type": "AEMU"}}],
            },
            {
                "test": {"name": "test2", "cpu": "x64"},
                "environments": [{"dimensions": {"device_type": "AEMU"}}],
            },
        ]
        self.assertEqual(expected_tests, tests)
        self.assertEqual(
            json.loads(
                (self.build_dir / "bazel_test_packages.list").read_text()
            ),
            {"content": {"manifests": []}, "version": "1"},
        )

    def test_only_test_groups(self) -> None:
        tests_json = [
            {"test": {"name": "test1", "cpu": "x64"}},
            {"test": {"name": "test2", "cpu": "x64"}},
        ]
        tests_json_str = json.dumps(tests_json)
        tests_json_path = self.build_dir / "pb_tests.json"
        tests_json_path.write_text(tests_json_str)

        env = {"dimensions": {"device_type": "Vim3"}}
        test_groups = [
            {
                "product_bundle_name": "my_pb",
                "environments": [env],
                "tests_json": str(tests_json_path),
            }
        ]
        product_bundles = [{"name": "my_pb"}]
        (_, tests) = self._test([], test_groups, product_bundles)

        expected_tests_json = [
            {
                "product_bundle": "my_pb",
                "test": {"name": "test1-my_pb", "cpu": "x64"},
                "environments": [env],
            },
            {
                "product_bundle": "my_pb",
                "test": {"name": "test2-my_pb", "cpu": "x64"},
                "environments": [env],
            },
        ]
        self.assertEqual(expected_tests_json, tests)

    def test_incorrect_product_bundle_name(self) -> None:
        tests_json = [{"test": {"name": "test1"}}, {"test": {"name": "test2"}}]
        tests_json_str = json.dumps(tests_json)
        tests_json_path = self.build_dir / "pb_tests.json"
        tests_json_path.write_text(tests_json_str)

        test_groups = [
            {
                "product_bundle_name": "my_pb_incorrect",
                "tests_json": str(tests_json_path),
            }
        ]
        product_bundles = [{"name": "my_pb"}]

        with self.assertRaises(SystemExit):
            self._test([], test_groups, product_bundles)

    def test_metadata_and_product_bundles(self) -> None:
        tests_from_metadata = [
            {"test": {"name": "test1", "cpu": "x64"}},
            {"test": {"name": "test2", "cpu": "x64"}},
        ]
        tests_json = [
            {"test": {"name": "test1", "cpu": "x64"}},
            {"test": {"name": "test2", "cpu": "x64"}},
        ]
        tests_json_str = json.dumps(tests_json)
        tests_json_path = self.build_dir / "pb_tests.json"
        tests_json_path.write_text(tests_json_str)
        product_bundles = [{"name": "my_pb"}]

        env = {"dimensions": {"device_type": "Vim3"}}
        test_groups = [
            {
                "product_bundle_name": "my_pb",
                "environments": [env],
                "tests_json": str(tests_json_path),
            }
        ]

        _, tests = self._test(
            tests_from_metadata,
            test_groups,
            product_bundles,
        )

        default_env = {"dimensions": {"device_type": "AEMU"}}
        expected_tests_json = [
            {
                "test": {"name": "test1", "cpu": "x64"},
                "environments": [default_env],
            },
            {
                "product_bundle": "my_pb",
                "environments": [env],
                "test": {"name": "test1-my_pb", "cpu": "x64"},
            },
            {
                "test": {"name": "test2", "cpu": "x64"},
                "environments": [default_env],
            },
            {
                "product_bundle": "my_pb",
                "environments": [env],
                "test": {"name": "test2-my_pb", "cpu": "x64"},
            },
        ]
        self.assertEqual(expected_tests_json, tests)

    def test_bazel_tests(self) -> None:
        # Prepare mock runner for bazel cquery
        mock_runner = MockCommandRunner()
        test1 = {
            "name": "test1",
            "label": "@@//t1",
            "source_label": "@@//t1",
            "launcher_execroot_path": "p1",
            "runtime_deps_json_execroot_path": "d1",
            "os": "linux",
            "cpu": "x64",
            "list_cases_argument": "list_cases_1",
        }
        test2 = {
            "name": "test2",
            "label": "@@//t2",
            "source_label": "@@//t2",
            "launcher_execroot_path": "p2",
            "runtime_deps_json_execroot_path": "d2",
            "os": "linux",
            "cpu": "x64",
            "list_cases_argument": "",
        }
        mock_runner.push_result(
            stdout=json.dumps(test1) + "\n" + json.dumps(test2)
        )

        dummy_host_test = {
            "environments": [
                {
                    "dimensions": {
                        "os": "Linux",
                        "cpu": "x64",
                    }
                }
            ],
            "test": {
                "name": "dummy_host_test",
                "cpu": "x64",
            },
        }
        _, tests = self._test(
            [dummy_host_test],
            [],
            [],
            with_bazel_tests=True,
            command_runner=mock_runner,
        )

        expected_tests_json: list[dict[str, T.Any]] = [
            dummy_host_test,
            {
                "environments": [
                    {
                        "dimensions": {
                            "os": "Linux",
                            "cpu": "x64",
                        }
                    }
                ],
                "expects_ssh": False,
                "test": {
                    "name": "//t1",
                    "label": "@@//t1",
                    "source_label": "//t1",
                    "path": f"{self.execroot_path}/p1",
                    "runtime_deps": f"{self.execroot_path}/d1",
                    "os": "linux",
                    "cpu": "x64",
                    "list_cases_argument": "list_cases_1",
                },
            },
            {
                "environments": [
                    {
                        "dimensions": {
                            "os": "Linux",
                            "cpu": "x64",
                        }
                    }
                ],
                "expects_ssh": False,
                "test": {
                    "name": "//t2",
                    "label": "@@//t2",
                    "source_label": "//t2",
                    "path": f"{self.execroot_path}/p2",
                    "runtime_deps": f"{self.execroot_path}/d2",
                    "os": "linux",
                    "cpu": "x64",
                },
            },
        ]
        self.assertEqual(len(expected_tests_json), len(tests))
        self.assertDictEqual(expected_tests_json[0], tests[0])
        self.assertDictEqual(expected_tests_json[1], tests[1])

    def test_bazel_device_tests_get_default_environments(self) -> None:
        (self.build_dir / "ungrouped_bazel_target_test_suites.txt").write_text(
            "//fake/device_tests"
        )
        mock_runner = MockCommandRunner()
        # Host test cquery, which finds no tests.
        mock_runner.push_result(stdout="")
        mock_runner.push_result(
            stdout=json.dumps(
                {
                    "label": "@@//src/my_test:my_test",
                    "package_manifest_execroot_path": "bazel-out/my_test/package_manifest.json",
                    "os": "fuchsia",
                    "cpu": "arm64",
                    "test_components": [
                        {
                            "component_name": "my_test",
                            "package_url": "fuchsia-pkg://fuchsia.com/my-test#meta/my_test.cm",
                        },
                    ],
                }
            )
        )

        _, tests = self._test(
            [],
            [],
            [],
            with_bazel_tests=True,
            command_runner=mock_runner,
            default_test_environments={
                "target_cpu": "arm64",
                "default_environments": [
                    {"dimensions": {"device_type": "Vim3"}}
                ],
                "allowed_device_types": ["Vim3"],
                "allowed_host_device_types": [],
            },
        )

        self.assertEqual(len(tests), 1)
        self.assertEqual(
            tests[0]["test"]["package_url"],
            "fuchsia-pkg://fuchsia.com/my-test#meta/my_test.cm",
        )
        self.assertEqual(
            tests[0]["environments"], [{"dimensions": {"device_type": "Vim3"}}]
        )

    def test_bazel_device_tests_custom_environments(self) -> None:
        (self.build_dir / "ungrouped_bazel_target_test_suites.txt").write_text(
            "//fake/device_tests"
        )
        mock_runner = MockCommandRunner()
        # Host test cquery, which finds no tests.
        mock_runner.push_result(stdout="")
        # Device test cquery returning two tests:
        # 1) A test specifying AEMU, QEMU (with 1cpu emulator config), NUC11
        #    (x64, filtered out by allowed_device_types = ["AEMU", "QEMU"]),
        #    and Astro (arm64, filtered out by target_cpu = "x64").
        # 2) A test specifying only NUC11 (x64, not in allowed_device_types)
        #    and Vim3 (arm64), which should become build_only.
        test1 = {
            "label": "@@//src/my_test:my_test",
            "package_manifest_execroot_path": "bazel-out/my_test/package_manifest.json",
            "os": "fuchsia",
            "cpu": "x64",
            "environments": [
                {"dimensions": {"device_type": "AEMU"}},
                {
                    "dimensions": {"device_type": "QEMU"},
                    "emulator": {"name": "1cpu", "device": "x64-emu-min"},
                },
                {"dimensions": {"device_type": "Intel NUC Kit NUC11TNHv5"}},
                {"dimensions": {"device_type": "Astro"}},
            ],
            "test_components": [
                {
                    "component_name": "my_test",
                    "package_url": "fuchsia-pkg://fuchsia.com/my-test#meta/my_test.cm",
                },
            ],
        }
        test2 = {
            "label": "@@//src/hw_only_test:hw_only_test",
            "package_manifest_execroot_path": "bazel-out/hw_only_test/package_manifest.json",
            "os": "fuchsia",
            "cpu": "x64",
            "environments": [
                {"dimensions": {"device_type": "Intel NUC Kit NUC11TNHv5"}},
                {"dimensions": {"device_type": "Vim3"}},
            ],
            "test_components": [
                {
                    "component_name": "hw_only_test",
                    "package_url": "fuchsia-pkg://fuchsia.com/hw-only-test#meta/hw_only_test.cm",
                },
            ],
        }
        mock_runner.push_result(
            stdout=json.dumps(test1) + "\n" + json.dumps(test2)
        )

        _, tests = self._test(
            [],
            [],
            [],
            with_bazel_tests=True,
            command_runner=mock_runner,
        )

        self.assertEqual(len(tests), 2)
        self.assertEqual(
            tests[0]["environments"],
            [
                {"dimensions": {"device_type": "AEMU"}},
                {
                    "dimensions": {"device_type": "QEMU"},
                    "emulator": {"name": "1cpu", "device": "x64-emu-min"},
                },
            ],
        )
        self.assertNotIn("build_only", tests[0])
        self.assertEqual(tests[1]["environments"], [])
        self.assertTrue(tests[1]["build_only"])

    def test_bazel_device_tests_invalid_environment(self) -> None:
        (self.build_dir / "ungrouped_bazel_target_test_suites.txt").write_text(
            "//fake/device_tests"
        )
        mock_runner = MockCommandRunner()
        mock_runner.push_result(stdout="")
        mock_runner.push_result(
            stdout=json.dumps(
                {
                    "label": "@@//src/bad_test:bad_test",
                    "package_manifest_execroot_path": "bazel-out/bad_test/package_manifest.json",
                    "os": "fuchsia",
                    "cpu": "x64",
                    "environments": [
                        {"dimensions": {"device_type": "NonExistent"}},
                    ],
                    "test_components": [
                        {
                            "component_name": "bad_test",
                            "package_url": "fuchsia-pkg://fuchsia.com/bad-test#meta/bad_test.cm",
                        },
                    ],
                }
            )
        )

        with self.assertRaisesRegex(
            ValueError,
            r"fuchsia-pkg://fuchsia.com/bad-test#meta/bad_test\.cm \(@@//src/bad_test:bad_test\): Could not match environment specifications",
        ):
            self._test(
                [],
                [],
                [],
                with_bazel_tests=True,
                command_runner=mock_runner,
            )

    def test_bazel_device_tests_in_product_bundle_test_group(self) -> None:
        pb_tests_json_path = self.build_dir / "pb_tests.json"
        pb_tests_json_path.write_text("[]")
        # Like real product_bundle_test_group()s, each group has its own suite
        # file, even though both list the same Bazel test suite.
        (self.build_dir / "my_pb_suites.txt").write_text(
            "//fake/pb_device_tests\n"
        )
        (self.build_dir / "my_build_only_pb_suites.txt").write_text(
            "//fake/pb_device_tests\n"
        )

        mock_runner = MockCommandRunner()
        # Host test cquery finds no tests.
        mock_runner.push_result(stdout="")
        # Device test cquery covering both groups' suite files.
        mock_runner.push_result(
            stdout=json.dumps(
                {
                    "label": "@@//src/my_test:my_test",
                    "package_manifest_execroot_path": "bazel-out/my_test/package_manifest.json",
                    "os": "fuchsia",
                    "cpu": "arm64",
                    "test_components": [
                        {
                            "component_name": "my_test",
                            "package_url": "fuchsia-pkg://fuchsia.com/my-test#meta/my_test.cm",
                        },
                    ],
                }
            )
        )

        vim3_env = {"dimensions": {"device_type": "Vim3"}}
        test_groups = [
            {
                "product_bundle_name": "my_pb",
                "environments": [vim3_env],
                "tests_json": "pb_tests.json",
                "bazel_target_test_suites": "my_pb_suites.txt",
            },
            {
                "product_bundle_name": "my_build_only_pb",
                "build_only": True,
                "tests_json": "pb_tests.json",
                "bazel_target_test_suites": "my_build_only_pb_suites.txt",
            },
        ]
        product_bundles = [{"name": "my_pb"}, {"name": "my_build_only_pb"}]

        _, tests = self._test(
            [],
            test_groups,
            product_bundles,
            with_bazel_tests=True,
            command_runner=mock_runner,
            default_test_environments={
                "target_cpu": "arm64",
                "default_environments": [],
                "allowed_device_types": [],
                "allowed_host_device_types": [],
            },
        )

        self.assertEqual(len(tests), 1)
        self.assertEqual(
            tests[0]["test"]["name"],
            "fuchsia-pkg://fuchsia.com/my-test#meta/my_test.cm-my_pb",
        )
        self.assertEqual(tests[0]["product_bundle"], "my_pb")
        self.assertEqual(tests[0]["environments"], [vim3_env])
        self.assertNotIn("build_only", tests[0])

    def test_full(self) -> None:
        tests_from_metadata = [
            {
                "test": {"name": "test1", "cpu": "x64"},
                "environments": [{"dimensions": {"os": "Linux"}}],
            },
            {"test": {"name": "test2", "cpu": "x64"}},
        ]
        tests_json = [
            {"test": {"name": "test1", "cpu": "x64"}},
            {"test": {"name": "test2", "cpu": "x64"}},
        ]
        tests_json_str = json.dumps(tests_json)
        tests_json_path = self.build_dir / "pb_tests.json"
        tests_json_path.write_text(tests_json_str)
        product_bundles = [{"name": "my_pb"}]

        env = {"dimensions": {"device_type": "Vim3"}}
        test_groups = [
            {
                "product_bundle_name": "my_pb",
                "environments": [env],
                "tests_json": str(tests_json_path),
            }
        ]

        mock_runner = MockCommandRunner()
        test1 = {
            "name": "test1",
            "label": "@@//t1",
            "source_label": "@@//t1",
            "launcher_execroot_path": "p1",
            "runtime_deps_json_execroot_path": "d1",
            "os": "linux",
            "cpu": "x64",
            "list_cases_argument": "",
        }
        mock_runner.push_result(stdout=json.dumps(test1))

        (_, tests) = self._test(
            tests_from_metadata,
            test_groups,
            product_bundles,
            with_bazel_tests=True,
            command_runner=mock_runner,
        )

        expected_tests_json: list[dict[str, T.Any]] = [
            {
                "test": {"name": "test1", "cpu": "x64"},
                "environments": [{"dimensions": {"os": "Linux"}}],
            },
            {
                "product_bundle": "my_pb",
                "environments": [env],
                "test": {"name": "test1-my_pb", "cpu": "x64"},
            },
            {
                "test": {"name": "test2", "cpu": "x64"},
                "environments": [{"dimensions": {"device_type": "AEMU"}}],
            },
            {
                "product_bundle": "my_pb",
                "environments": [env],
                "test": {"name": "test2-my_pb", "cpu": "x64"},
            },
            {
                "environments": [
                    {
                        "dimensions": {
                            "os": "Linux",
                            "cpu": "x64",
                        }
                    }
                ],
                "expects_ssh": False,
                "test": {
                    "name": "//t1",
                    "label": "@@//t1",
                    "source_label": "//t1",
                    "path": f"{self.execroot_path}/p1",
                    "runtime_deps": f"{self.execroot_path}/d1",
                    "os": "linux",
                    "cpu": "x64",
                },
            },
        ]
        self.assertEqual(expected_tests_json, tests)

    def test_ninja_inputs(self) -> None:
        (inputs, _) = self._test([], [], [])
        self.assertEqual(
            {
                Path(self.build_dir / "tests_from_metadata.json"),
                Path(
                    self.build_dir
                    / "obj"
                    / "tests"
                    / "product_bundle_test_groups.json"
                ),
                Path(
                    self.build_dir
                    / "obj"
                    / "tests"
                    / "all_test_environments.json"
                ),
                Path(
                    self.build_dir
                    / "obj"
                    / "tests"
                    / "default_test_environments.json"
                ),
            },
            inputs,
        )

    def test_only_write_if_changed(self) -> None:
        tests_from_metadata = [
            {
                "test": {"name": "test1", "cpu": "x64"},
                "environments": [{"dimensions": {"device_type": "AEMU"}}],
            },
            {
                "test": {"name": "test2", "cpu": "x64"},
                "environments": [{"dimensions": {"device_type": "AEMU"}}],
            },
        ]
        tests_json = json.dumps(tests_from_metadata)
        tests_json_path = self.build_dir / "tests.json"
        tests_json_path.write_text(tests_json)
        previous_write_time = os.path.getmtime(tests_json_path)

        self._test(tests_from_metadata, [], [])

        # ensure the file did not change
        current_write_time = os.path.getmtime(tests_json_path)
        self.assertEqual(previous_write_time, current_write_time)

    def test_dimensions_dataclass(self) -> None:
        d1 = instance_from_dict(
            build_tests_json.Dimensions,
            {
                "os": "Linux",
                "cpu": "x64",
                "device_type": "QEMU",
                "host_device_type": "GCP_C4A_HIGHMEM_96_BM",
                "pool": "fuchsia.tests",
                "testbed": "my_testbed",
                "access_points": "1",
            },
        )
        d2 = build_tests_json.Dimensions(
            device_type="QEMU",
            cpu="x64",
            os="Linux",
            pool="fuchsia.tests",
            testbed="my_testbed",
            host_device_type="GCP_C4A_HIGHMEM_96_BM",
            access_points="1",
        )
        self.assertEqual(d1, d2)
        self.assertEqual(hash(d1), hash(d2))
        self.assertEqual(len({d1, d2}), 1)
        self.assertEqual(d1.cpu, "x64")
        self.assertEqual(d1.device_type, "QEMU")
        self.assertEqual(d1.host_device_type, "GCP_C4A_HIGHMEM_96_BM")
        self.assertEqual(d1.os, "Linux")
        self.assertEqual(d1.pool, "fuchsia.tests")
        self.assertEqual(d1.testbed, "my_testbed")
        self.assertEqual(d1.access_points, "1")
        self.assertEqual(d1.get("access_points"), "1")
        self.assertIsNone(d1.get("missing"))
        self.assertEqual(d1.to_dict(), instance_to_dict(d1))

        sub = build_tests_json.Dimensions(device_type="QEMU")
        self.assertTrue(sub.is_subset_of(d1))
        self.assertFalse(d1.is_subset_of(sub))
        self.assertTrue(d1.is_subset_of(d1))
        self.assertTrue(build_tests_json.Dimensions().is_subset_of(sub))
        self.assertFalse(
            build_tests_json.Dimensions(device_type="AEMU").is_subset_of(d1)
        )
        self.assertFalse(
            build_tests_json.Dimensions(
                device_type="QEMU", cpu="arm64"
            ).is_subset_of(d1)
        )
        # An empty string is still an explicitly specified dimension.
        self.assertFalse(build_tests_json.Dimensions(cpu="").is_subset_of(sub))

        with self.assertRaisesRegex(ValueError, "tags are only valid"):
            instance_from_dict(
                build_tests_json.Dimensions,
                {"device_type": "QEMU", "tags": ["foo"]},
            )
        with self.assertRaisesRegex(
            ValueError, "found nested dimensions environment field"
        ):
            instance_from_dict(
                build_tests_json.Dimensions,
                {"dimensions": {"device_type": "QEMU"}},
            )

    def test_emulator_config_dataclass(self) -> None:
        emu_dict = {
            "name": "1cpu",
            "device": "x64-emu-min",
            "accel": "hyper",
            "kernel_args": ["arg1", "arg2"],
            "uefi": True,
            "vbmeta_key": "path/to/key.pem",
            "vbmeta_key_metadata": "path/to/meta.bin",
        }
        emu = instance_from_dict(
            build_tests_json.EmulatorConfig,
            emu_dict,
        )
        self.assertEqual(emu.to_dict(), emu_dict)
        self.assertEqual(instance_to_dict(emu), emu_dict)

        with self.assertRaisesRegex(ValueError, "requires a unique `name`"):
            instance_from_dict(
                build_tests_json.EmulatorConfig, {"device": "x64-emu-min"}
            )

        with self.assertRaisesRegex(
            ValueError, "must provide a `vbmeta_key` and `vbmeta_key_metadata`"
        ):
            instance_from_dict(
                build_tests_json.EmulatorConfig,
                {"name": "uefi_emu", "uefi": True},
            )

    def test_environment_dataclass_and_deduplication(self) -> None:
        env_a1 = instance_from_dict(
            build_tests_json.Environment,
            {
                "dimensions": {"device_type": "QEMU", "cpu": "x64"},
                "tags": ["tag1"],
            },
        )
        env_a2 = instance_from_dict(
            build_tests_json.Environment,
            {
                "dimensions": {"cpu": "x64", "device_type": "QEMU"},
                "tags": ["tag1"],
            },
        )
        env_b = instance_from_dict(
            build_tests_json.Environment,
            {"dimensions": {"device_type": "AEMU"}},
        )
        self.assertEqual(env_a1, env_a2)
        self.assertEqual(hash(env_a1), hash(env_a2))
        self.assertEqual(env_a1.to_dict(), instance_to_dict(env_a1))
        self.assertEqual(
            sorted({env_a1, env_b, env_a2}),
            [env_a1, env_b],
        )

        with self.assertRaisesRegex(
            ValueError, "each environment must specify dimensions"
        ):
            instance_from_dict(build_tests_json.Environment, {})
        with self.assertRaisesRegex(
            ValueError, "each environment must specify dimensions"
        ):
            instance_from_dict(build_tests_json.Environment, {"dimensions": {}})

    def test_partition_platforms(self) -> None:
        platforms = [
            {"device_type": "AEMU", "cpu": "x64"},
            {"device_type": "Vim3", "cpu": "arm64"},
            {"device_type": "Astro", "pool": "fuchsia.tests.connectivity"},
        ]
        target_p, other_p = build_tests_json.partition_platforms(
            platforms, "x64"
        )
        self.assertEqual(
            target_p,
            {
                instance_from_dict(
                    build_tests_json.Dimensions,
                    {"device_type": "AEMU", "cpu": "x64"},
                ),
            },
        )
        self.assertEqual(
            other_p,
            {
                instance_from_dict(
                    build_tests_json.Dimensions,
                    {"device_type": "Vim3", "cpu": "arm64"},
                ),
                instance_from_dict(
                    build_tests_json.Dimensions,
                    {
                        "device_type": "Astro",
                        "pool": "fuchsia.tests.connectivity",
                    },
                ),
            },
        )

    def test_unknown_platform_raises_value_error(self) -> None:
        tests_from_metadata = [
            {
                "test": {"name": "bad_env_test", "cpu": "x64"},
                "environments": [
                    {"dimensions": {"device_type": "NonExistent"}}
                ],
            }
        ]
        with self.assertRaisesRegex(
            ValueError, "Could not match environment specifications"
        ):
            self._test(tests_from_metadata, [], [])

    def test_multiple_validation_errors_collected(self) -> None:
        tests_from_metadata = [
            {
                "test": {
                    "name": "bad_platform_test",
                    "label": "//src:test1",
                    "cpu": "x64",
                },
                "environments": [
                    {"dimensions": {"device_type": "NonExistent"}}
                ],
            },
            {
                "test": {
                    "name": "bad_tags_test",
                    "label": "//src:test2",
                    "cpu": "x64",
                },
                "environments": [
                    {"dimensions": {"device_type": "AEMU", "tags": ["oops"]}}
                ],
            },
        ]
        with self.assertRaises(ValueError) as ctx:
            self._test(tests_from_metadata, [], [])
        err_msg = str(ctx.exception)
        self.assertIn("bad_platform_test (//src:test1)", err_msg)
        self.assertIn("Could not match environment specifications", err_msg)
        self.assertIn("bad_tags_test (//src:test2)", err_msg)
        self.assertIn("tags are only valid in an environments scope", err_msg)

    def test_metadata_host_test_default_and_filtering(self) -> None:
        tests_from_metadata = [
            {
                "test": {"name": "pure_host_test", "os": "linux", "cpu": "x64"},
                "expects_ssh": False,
            },
            {
                "test": {
                    "name": "host_driven_target_test",
                    "os": "linux",
                    "cpu": "x64",
                },
                "expects_ssh": True,
            },
            {
                "test": {"name": "filtered_out_test", "cpu": "x64"},
                "environments": [{"dimensions": {"device_type": "Vim3"}}],
            },
            {
                "test": {
                    "name": "filtered_out_host_device_type_test",
                    "cpu": "x64",
                },
                "environments": [
                    {
                        "dimensions": {
                            "device_type": "QEMU",
                            "host_device_type": "AmpereAltraMax-M128-30",
                        }
                    }
                ],
            },
            {
                "test": {"name": "explicit_build_only_test", "cpu": "x64"},
                "build_only": True,
            },
        ]
        (_, tests) = self._test(tests_from_metadata, [], [])
        expected_tests = [
            {
                "test": {"name": "pure_host_test", "os": "linux", "cpu": "x64"},
                "expects_ssh": False,
                "environments": [{"dimensions": {"cpu": "x64", "os": "Linux"}}],
            },
            {
                "test": {
                    "name": "host_driven_target_test",
                    "os": "linux",
                    "cpu": "x64",
                },
                "expects_ssh": True,
                "environments": [{"dimensions": {"device_type": "AEMU"}}],
            },
            {
                "test": {"name": "filtered_out_test", "cpu": "x64"},
                "build_only": True,
                "environments": [],
            },
            {
                "test": {
                    "name": "filtered_out_host_device_type_test",
                    "cpu": "x64",
                },
                "build_only": True,
                "environments": [],
            },
            {
                "test": {"name": "explicit_build_only_test", "cpu": "x64"},
                "build_only": True,
                "environments": [],
            },
        ]
        self.assertEqual(expected_tests, tests)

    def test_product_bundle_test_group_environment_override(self) -> None:
        vim3_env = {"dimensions": {"device_type": "Vim3"}}
        astro_env = {"dimensions": {"device_type": "Astro"}}
        aemu_env = {"dimensions": {"device_type": "AEMU"}}

        pb_tests = [
            # No environments set: should inherit group's environments (NOT basic_envs).
            {"test": {"name": "default_pb_test", "cpu": "x64"}},
            # Overlapping environments: should be overridden by group's environments.
            {
                "test": {"name": "subset_pb_test", "cpu": "x64"},
                "environments": [vim3_env, aemu_env],
            },
            # Disjoint environments: should be overridden by group's environments.
            {
                "test": {"name": "disjoint_pb_test", "cpu": "x64"},
                "environments": [aemu_env],
            },
            # Explicit build_only: remains build_only = True with empty environments.
            {
                "test": {"name": "build_only_pb_test", "cpu": "x64"},
                "build_only": True,
            },
        ]
        tests_json_path = self.build_dir / "pb_tests.json"
        tests_json_path.write_text(json.dumps(pb_tests))

        test_groups = [
            {
                "product_bundle_name": "my_pb",
                "environments": [vim3_env, astro_env],
                "tests_json": str(tests_json_path),
            }
        ]
        product_bundles = [{"name": "my_pb"}]
        (_, tests) = self._test([], test_groups, product_bundles)

        expected_tests = [
            {
                "test": {"name": "default_pb_test-my_pb", "cpu": "x64"},
                "product_bundle": "my_pb",
                "environments": [astro_env, vim3_env],
            },
            {
                "test": {"name": "subset_pb_test-my_pb", "cpu": "x64"},
                "product_bundle": "my_pb",
                "environments": [astro_env, vim3_env],
            },
            {
                "test": {"name": "disjoint_pb_test-my_pb", "cpu": "x64"},
                "product_bundle": "my_pb",
                "environments": [astro_env, vim3_env],
            },
            {
                "test": {"name": "build_only_pb_test", "cpu": "x64"},
                "build_only": True,
                "environments": [],
            },
        ]
        self.assertEqual(expected_tests, tests)

    def test_product_bundle_test_group_allowed_environments(self) -> None:
        vim3_env = {"dimensions": {"device_type": "Vim3"}}
        astro_env = {"dimensions": {"device_type": "Astro"}}
        aemu_env = {"dimensions": {"device_type": "AEMU"}}

        pb_tests = [
            # No environments set: should inherit group's allowed_environments when default_environments is omitted.
            {"test": {"name": "default_pb_test", "cpu": "x64"}},
            # Overlapping environments: should be intersected with group's allowed_environments.
            {
                "test": {"name": "subset_pb_test", "cpu": "x64"},
                "environments": [vim3_env, aemu_env],
            },
            # Disjoint environments: becomes build_only = True with empty environments.
            {
                "test": {"name": "disjoint_pb_test", "cpu": "x64"},
                "environments": [aemu_env],
            },
        ]
        tests_json_path = self.build_dir / "pb_tests.json"
        tests_json_path.write_text(json.dumps(pb_tests))

        test_groups = [
            {
                "product_bundle_name": "my_pb",
                "allowed_environments": [vim3_env, astro_env],
                "tests_json": str(tests_json_path),
            }
        ]
        product_bundles = [{"name": "my_pb"}]
        (_, tests) = self._test([], test_groups, product_bundles)

        expected_tests = [
            {
                "test": {"name": "default_pb_test-my_pb", "cpu": "x64"},
                "product_bundle": "my_pb",
                "environments": [astro_env, vim3_env],
            },
            {
                "test": {"name": "subset_pb_test-my_pb", "cpu": "x64"},
                "product_bundle": "my_pb",
                "environments": [vim3_env],
            },
            {
                "test": {"name": "disjoint_pb_test", "cpu": "x64"},
                "build_only": True,
                "environments": [],
            },
        ]
        self.assertEqual(expected_tests, tests)

    def test_product_bundle_test_group_default_environments_subset(
        self,
    ) -> None:
        aemu_env = {"dimensions": {"device_type": "AEMU"}}
        nuc11_env = {"dimensions": {"device_type": "Intel NUC Kit NUC11TNHv5"}}
        vim3_env = {"dimensions": {"device_type": "Vim3"}}

        pb_tests = [
            # No environments set: should only inherit group's default_environments ([aemu_env]).
            {"test": {"name": "default_pb_test", "cpu": "x64"}},
            # Explicitly specifies nuc11_env: allowed by allowed_environments ([aemu_env, nuc11_env]).
            {
                "test": {"name": "nuc11_pb_test", "cpu": "x64"},
                "environments": [nuc11_env],
            },
            # Explicitly specifies vim3_env and nuc11_env: intersected with allowed_environments -> [nuc11_env].
            {
                "test": {"name": "mixed_pb_test", "cpu": "x64"},
                "environments": [vim3_env, nuc11_env],
            },
            # Disjoint environments: becomes build_only = True with empty environments.
            {
                "test": {"name": "disjoint_pb_test", "cpu": "x64"},
                "environments": [vim3_env],
            },
        ]
        tests_json_path = self.build_dir / "pb_tests.json"
        tests_json_path.write_text(json.dumps(pb_tests))

        test_groups = [
            {
                "product_bundle_name": "my_pb",
                "allowed_environments": [aemu_env, nuc11_env],
                "default_environments": [aemu_env],
                "tests_json": str(tests_json_path),
            }
        ]
        product_bundles = [{"name": "my_pb"}]
        (_, tests) = self._test([], test_groups, product_bundles)

        expected_tests = [
            {
                "test": {"name": "default_pb_test-my_pb", "cpu": "x64"},
                "product_bundle": "my_pb",
                "environments": [aemu_env],
            },
            {
                "test": {"name": "nuc11_pb_test-my_pb", "cpu": "x64"},
                "product_bundle": "my_pb",
                "environments": [nuc11_env],
            },
            {
                "test": {"name": "mixed_pb_test-my_pb", "cpu": "x64"},
                "product_bundle": "my_pb",
                "environments": [nuc11_env],
            },
            {
                "test": {"name": "disjoint_pb_test", "cpu": "x64"},
                "build_only": True,
                "environments": [],
            },
        ]
        self.assertEqual(expected_tests, tests)

    def test_product_bundle_test_group_empty_default_environments(self) -> None:
        aemu_env = {"dimensions": {"device_type": "AEMU"}}
        nuc11_env = {"dimensions": {"device_type": "Intel NUC Kit NUC11TNHv5"}}

        pb_tests = [
            # No environments set: becomes build_only when default_environments is empty.
            {"test": {"name": "default_pb_test", "cpu": "x64"}},
            # Explicitly specifies nuc11_env: allowed by allowed_environments.
            {
                "test": {"name": "nuc11_pb_test", "cpu": "x64"},
                "environments": [nuc11_env],
            },
        ]
        tests_json_path = self.build_dir / "pb_tests.json"
        tests_json_path.write_text(json.dumps(pb_tests))

        test_groups = [
            {
                "product_bundle_name": "my_pb",
                "allowed_environments": [aemu_env, nuc11_env],
                "default_environments": [],
                "tests_json": str(tests_json_path),
            }
        ]
        product_bundles = [{"name": "my_pb"}]
        (_, tests) = self._test([], test_groups, product_bundles)

        expected_tests = [
            {
                "test": {"name": "default_pb_test", "cpu": "x64"},
                "build_only": True,
                "environments": [],
            },
            {
                "test": {"name": "nuc11_pb_test-my_pb", "cpu": "x64"},
                "product_bundle": "my_pb",
                "environments": [nuc11_env],
            },
        ]
        self.assertEqual(expected_tests, tests)

    def test_product_bundle_test_group_consolidates_build_only_tests(
        self,
    ) -> None:
        aemu_env = {"dimensions": {"device_type": "AEMU"}}
        vim3_env = {"dimensions": {"device_type": "Vim3"}}

        tests_from_metadata = [
            # Already build-only in metadata:
            {
                "test": {"name": "already_build_only", "cpu": "x64"},
                "build_only": True,
            },
            # Filtered out to build-only in metadata (Vim3 not in allowed_device_types):
            {
                "test": {"name": "filtered_to_build_only", "cpu": "x64"},
                "environments": [vim3_env],
            },
            # Runnable in metadata (NOT build-only):
            {"test": {"name": "runnable_in_main", "cpu": "x64"}},
            # Already build-only in metadata, and will be deduplicated against those
            # in PBs.
            {
                "test": {"name": "test_1", "cpu": "x64"},
                "build_only": True,
            },
        ]

        pb_tests = [
            {"test": {"name": "test_1", "cpu": "x64"}},
            {"test": {"name": "test_2", "cpu": "x64"}},
            {"test": {"name": "test_3", "cpu": "x64"}},
            {"test": {"name": "test_4", "cpu": "x64"}},
            {
                "test": {
                    "name": "pure_host_test",
                    "os": "linux",
                    "cpu": "x64",
                },
                "expects_ssh": False,
            },
        ]
        pb_tests_json_path = self.build_dir / "pb_tests.json"
        pb_tests_json_path.write_text(json.dumps(pb_tests))

        test_groups = [
            {
                "product_bundle_name": "my_pb",
                "build_only": True,
                "tests_json": str(pb_tests_json_path),
            },
            {
                "product_bundle_name": "my_other_pb",
                "build_only": True,
                "tests_json": str(pb_tests_json_path),
            },
        ]
        product_bundles = [{"name": "my_pb"}, {"name": "my_other_pb"}]
        (_, tests) = self._test(
            tests_from_metadata, test_groups, product_bundles
        )

        expected_tests = [
            {
                "test": {"name": "already_build_only", "cpu": "x64"},
                "build_only": True,
                "environments": [],
            },
            {
                "test": {"name": "filtered_to_build_only", "cpu": "x64"},
                "build_only": True,
                "environments": [],
            },
            {
                "test": {"name": "runnable_in_main", "cpu": "x64"},
                "environments": [aemu_env],
            },
            {
                "test": {"name": "test_1", "cpu": "x64"},
                "build_only": True,
                "environments": [],
            },
            {
                "test": {"name": "test_2", "cpu": "x64"},
                "build_only": True,
                "environments": [],
            },
            {
                "test": {"name": "test_3", "cpu": "x64"},
                "build_only": True,
                "environments": [],
            },
            {
                "test": {"name": "test_4", "cpu": "x64"},
                "build_only": True,
                "environments": [],
            },
            {
                "test": {
                    "name": "pure_host_test",
                    "os": "linux",
                    "cpu": "x64",
                },
                "build_only": True,
                "environments": [],
                "expects_ssh": False,
            },
        ]
        self.assertEqual(expected_tests, tests)

    def test_product_bundles_promote_host_test_to_runnable(self) -> None:
        """If a test is runnable in one PB, regardless of order of PBs, it should always be listed
        as runnable.
        """
        # The same test has the same `test` spec wherever it's listed.
        tests_from_metadata = [
            # Already build-only in metadata:
            {
                "test": {
                    "name": "test_1",
                    "os": "linux",
                    "cpu": "x64",
                },
                "build_only": True,
            },
        ]

        tests_from_pb_1 = [
            {
                "test": {
                    "name": "test_1",
                    "os": "linux",
                    "cpu": "x64",
                },
                "build_only": True,
            },
        ]

        pb_tests_json_path_1 = self.build_dir / "pb_1_tests.json"
        pb_tests_json_path_1.write_text(json.dumps(tests_from_pb_1))

        tests_from_pb_2 = [
            {
                "test": {
                    "name": "test_1",
                    "os": "linux",
                    "cpu": "x64",
                },
                "expects_ssh": False,
            },
        ]

        pb_tests_json_path_2 = self.build_dir / "pb_2_tests.json"
        pb_tests_json_path_2.write_text(json.dumps(tests_from_pb_2))

        test_groups = [
            {
                "product_bundle_name": "my_pb",
                "tests_json": str(pb_tests_json_path_1),
            },
            {
                "product_bundle_name": "my_other_pb",
                "tests_json": str(pb_tests_json_path_2),
            },
        ]
        product_bundles = [{"name": "my_pb"}, {"name": "my_other_pb"}]
        (_, tests) = self._test(
            tests_from_metadata, test_groups, product_bundles
        )

        expected_tests = [
            {
                "test": {
                    "name": "test_1",
                    "os": "linux",
                    "cpu": "x64",
                },
                "environments": [{"dimensions": {"cpu": "x64", "os": "Linux"}}],
                "expects_ssh": False,
            },
        ]
        self.assertEqual(expected_tests, tests)

        # Now reverse the order, should be the same.
        test_groups_B = [
            {
                "product_bundle_name": "my_other_pb",
                "tests_json": str(pb_tests_json_path_2),
            },
            {
                "product_bundle_name": "my_pb",
                "tests_json": str(pb_tests_json_path_1),
            },
        ]
        product_bundles = [{"name": "my_pb"}, {"name": "my_other_pb"}]
        (_, tests_B) = self._test(
            tests_from_metadata, test_groups_B, product_bundles
        )

        self.assertEqual(expected_tests, tests_B)

    def test_cross_arch_linux_host_test(self) -> None:
        tests_from_metadata = [
            {
                "test": {"name": "x64_host_test", "os": "linux", "cpu": "x64"},
                "expects_ssh": False,
            },
            {
                "test": {
                    "name": "arm64_host_test",
                    "os": "linux",
                    "cpu": "arm64",
                },
                "expects_ssh": False,
            },
        ]
        default_test_environments = {
            "target_cpu": "arm64",
            "default_environments": [
                {
                    "dimensions": {
                        "device_type": "QEMU",
                        "host_device_type": "GCP_C4A_HIGHMEM_96_BM",
                    }
                }
            ],
            "allowed_device_types": ["QEMU"],
            "allowed_host_device_types": ["GCP_C4A_HIGHMEM_96_BM"],
        }
        (_, tests) = self._test(
            tests_from_metadata,
            [],
            [],
            default_test_environments=default_test_environments,
        )
        expected_tests = [
            {
                "test": {"name": "x64_host_test", "os": "linux", "cpu": "x64"},
                "expects_ssh": False,
                "build_only": True,
                "environments": [],
            },
            {
                "test": {
                    "name": "arm64_host_test",
                    "os": "linux",
                    "cpu": "arm64",
                },
                "expects_ssh": False,
                "environments": [
                    {"dimensions": {"cpu": "arm64", "os": "Linux"}}
                ],
            },
        ]
        self.assertEqual(expected_tests, tests)

    def test_boot_tests_for_each_host_cpu(self) -> None:
        """When the host and target CPUs differ, boot_test() defines a test for each host CPU
        that it can be run from.  These all share the same name, but are different tests (each
        with its own `cpu` and runner script `path`), so all of them must be kept, whether
        they're runnable or build-only.  Boot tests look like host tests (`os: linux` and
        `expects_ssh: False`), but they need to run against their target environments.
        """
        qemu_env = {"dimensions": {"device_type": "QEMU"}}
        vim3_env = {"dimensions": {"device_type": "Vim3"}}
        astro_env = {"dimensions": {"device_type": "Astro"}}
        tests_from_metadata = [
            # Emulators are run from a `target_cpu` host, but QEMU is not an
            # allowed device type, so this becomes build-only.
            {
                "test": {
                    "name": "boot_test_1",
                    "os": "linux",
                    "cpu": "arm64",
                    "path": "boot_test_1.host-arm64.sh",
                },
                "environments": [qemu_env],
                "expects_ssh": False,
                "is_boot_test": True,
            },
            # Hardware is run from a `host_cpu` host, and Vim3 is allowed.
            {
                "test": {
                    "name": "boot_test_1",
                    "os": "linux",
                    "cpu": "x64",
                    "path": "boot_test_1.host-x64.sh",
                },
                "environments": [vim3_env],
                "expects_ssh": False,
                "is_boot_test": True,
            },
            # Neither QEMU nor Astro are allowed, so both become build-only.
            {
                "test": {
                    "name": "boot_test_2",
                    "os": "linux",
                    "cpu": "arm64",
                    "path": "boot_test_2.host-arm64.sh",
                },
                "environments": [qemu_env],
                "expects_ssh": False,
                "is_boot_test": True,
            },
            {
                "test": {
                    "name": "boot_test_2",
                    "os": "linux",
                    "cpu": "x64",
                    "path": "boot_test_2.host-x64.sh",
                },
                "environments": [astro_env],
                "expects_ssh": False,
                "is_boot_test": True,
            },
        ]
        default_test_environments = {
            "target_cpu": "arm64",
            "default_environments": [vim3_env],
            "allowed_device_types": ["Vim3"],
            "allowed_host_device_types": [],
        }
        (_, tests) = self._test(
            tests_from_metadata,
            [],
            [],
            default_test_environments=default_test_environments,
        )

        expected_tests: list[dict[str, T.Any]] = [
            {
                "test": {
                    "name": "boot_test_1",
                    "os": "linux",
                    "cpu": "arm64",
                    "path": "boot_test_1.host-arm64.sh",
                },
                "build_only": True,
                "environments": [],
                "expects_ssh": False,
                "is_boot_test": True,
            },
            {
                "test": {
                    "name": "boot_test_1",
                    "os": "linux",
                    "cpu": "x64",
                    "path": "boot_test_1.host-x64.sh",
                },
                "environments": [vim3_env],
                "expects_ssh": False,
                "is_boot_test": True,
            },
            {
                "test": {
                    "name": "boot_test_2",
                    "os": "linux",
                    "cpu": "arm64",
                    "path": "boot_test_2.host-arm64.sh",
                },
                "build_only": True,
                "environments": [],
                "expects_ssh": False,
                "is_boot_test": True,
            },
            {
                "test": {
                    "name": "boot_test_2",
                    "os": "linux",
                    "cpu": "x64",
                    "path": "boot_test_2.host-x64.sh",
                },
                "build_only": True,
                "environments": [],
                "expects_ssh": False,
                "is_boot_test": True,
            },
        ]
        self.assertEqual(expected_tests, tests)

    def test_product_bundle_test_group_host_tests(self) -> None:
        # Use the exact same `pb_tests` input for both the arm64 and x64
        # product_bundle_test_group configurations to verify that the same
        # host test specs produce different results depending on the target/group
        # environments:
        # - A host-driven E2E test (`expects_ssh: True`) always receives the
        #   product bundle's target environment and `product_bundle` name.
        # - A pure host test (`expects_ssh: False`, `cpu: "x64"`) is never
        #   assigned the product bundle's target environment or `product_bundle`
        #   name; instead it resolves to `build_only: True` when `target_cpu` is
        #   `arm64` (`minimal.arm64`), and resolves to a runnable Linux x64 host
        #   test when `target_cpu` is `x64` (`minimal.x64`).
        pb_tests: list[dict[str, T.Any]] = [
            {
                "build_only": False,
                "environments": [],
                "expects_ssh": True,
                "test": {
                    "cpu": "x64",
                    "name": "host_x64/e2e_test",
                    "os": "linux",
                },
            },
            {
                "build_only": False,
                "environments": [],
                "expects_ssh": False,
                "test": {
                    "cpu": "x64",
                    "label": "//zircon/kernel/lib/acpi_lite/rust:acpi_lite_test(//build/toolchain:host_x64)",
                    "name": "host_x64/acpi_lite_lib_test",
                    "os": "linux",
                    "path": "host_x64/obj/zircon/kernel/lib/acpi_lite/rust/acpi_lite_test.sh",
                    "runtime_deps": "host_x64/gen/zircon/kernel/lib/acpi_lite/rust/acpi_lite_test.deps.json",
                    "source_label": "//zircon/kernel/lib/acpi_lite/rust:acpi_lite_test",
                },
            },
        ]
        pb_tests_json_path = self.build_dir / "pb_host_tests.json"
        pb_tests_json_path.write_text(json.dumps(pb_tests))

        # Case 1: minimal.arm64 (target_cpu = "arm64", c4a_env).
        # The x64 pure host test does not match the arm64 target platform, so it
        # becomes build_only with no environments and no product_bundle.
        c4a_env = {
            "dimensions": {
                "device_type": "QEMU",
                "host_device_type": "GCP_C4A_HIGHMEM_96_BM",
            }
        }
        arm64_test_groups = [
            {
                "product_bundle_name": "minimal.arm64",
                "allowed_environments": [c4a_env],
                "tests_json": str(pb_tests_json_path),
            }
        ]
        arm64_default_test_environments = {
            "target_cpu": "arm64",
            "default_environments": [c4a_env],
            "allowed_device_types": ["QEMU"],
            "allowed_host_device_types": ["GCP_C4A_HIGHMEM_96_BM"],
        }
        (_, arm64_tests) = self._test(
            [],
            arm64_test_groups,
            [{"name": "minimal.arm64"}],
            default_test_environments=arm64_default_test_environments,
        )

        expected_arm64_tests: list[dict[str, T.Any]] = [
            {
                "build_only": False,
                "environments": [c4a_env],
                "expects_ssh": True,
                "product_bundle": "minimal.arm64",
                "test": {
                    "cpu": "x64",
                    "name": "host_x64/e2e_test-minimal.arm64",
                    "os": "linux",
                },
            },
            {
                "build_only": True,
                "environments": [],
                "expects_ssh": False,
                "test": {
                    "cpu": "x64",
                    "label": "//zircon/kernel/lib/acpi_lite/rust:acpi_lite_test(//build/toolchain:host_x64)",
                    "name": "host_x64/acpi_lite_lib_test",
                    "os": "linux",
                    "path": "host_x64/obj/zircon/kernel/lib/acpi_lite/rust/acpi_lite_test.sh",
                    "runtime_deps": "host_x64/gen/zircon/kernel/lib/acpi_lite/rust/acpi_lite_test.deps.json",
                    "source_label": "//zircon/kernel/lib/acpi_lite/rust:acpi_lite_test",
                },
            },
        ]
        self.assertEqual(expected_arm64_tests, arm64_tests)

        # Case 2: minimal.x64 (target_cpu = "x64", aemu_env) using the same
        # `pb_tests_json_path` input.
        # The x64 pure host test matches the x64 target platform, so it resolves
        # to a runnable Linux x64 host test (without `product_bundle` or name
        # suffix) and is deduplicated across groups.
        aemu_env = {"dimensions": {"device_type": "AEMU"}}
        x64_test_groups = [
            {
                "product_bundle_name": "minimal.x64",
                "allowed_environments": [aemu_env],
                "tests_json": str(pb_tests_json_path),
            },
            {
                "product_bundle_name": "minimal.x64",
                "build_only": True,
                "tests_json": str(pb_tests_json_path),
            },
        ]
        (_, x64_tests) = self._test(
            [], x64_test_groups, [{"name": "minimal.x64"}]
        )

        expected_x64_tests: list[dict[str, T.Any]] = [
            {
                "build_only": False,
                "environments": [aemu_env],
                "expects_ssh": True,
                "product_bundle": "minimal.x64",
                "test": {
                    "cpu": "x64",
                    "name": "host_x64/e2e_test-minimal.x64",
                    "os": "linux",
                },
            },
            {
                "build_only": False,
                "environments": [{"dimensions": {"cpu": "x64", "os": "Linux"}}],
                "expects_ssh": False,
                "test": {
                    "cpu": "x64",
                    "label": "//zircon/kernel/lib/acpi_lite/rust:acpi_lite_test(//build/toolchain:host_x64)",
                    "name": "host_x64/acpi_lite_lib_test",
                    "os": "linux",
                    "path": "host_x64/obj/zircon/kernel/lib/acpi_lite/rust/acpi_lite_test.sh",
                    "runtime_deps": "host_x64/gen/zircon/kernel/lib/acpi_lite/rust/acpi_lite_test.deps.json",
                    "source_label": "//zircon/kernel/lib/acpi_lite/rust:acpi_lite_test",
                },
            },
        ]
        self.assertEqual(expected_x64_tests, x64_tests)

    def test_resolve_test_environments_helper(self) -> None:
        target_p, other_p = build_tests_json.partition_platforms(
            [
                {"device_type": "AEMU", "cpu": "x64"},
                {"device_type": "Vim3", "cpu": "arm64"},
            ],
            "x64",
        )
        aemu_env = build_tests_json.Environment(
            dimensions=build_tests_json.Dimensions(device_type="AEMU")
        )
        vim3_env = build_tests_json.Environment(
            dimensions=build_tests_json.Dimensions(device_type="Vim3")
        )

        # 1. General test mode
        test_basic: dict[str, T.Any] = {"test": {"name": "t1"}}
        build_tests_json.resolve_general_test_environments(
            test_basic,
            default_envs=[aemu_env],
            allowed_device_types={"AEMU"},
            allowed_host_device_types=set(),
            target_platforms=target_p,
            other_platforms=other_p,
        )
        self.assertEqual(
            test_basic["environments"],
            [{"dimensions": {"device_type": "AEMU"}}],
        )

        # 2. Product bundle test group mode with mandatory environments
        test_pb_override: dict[str, T.Any] = {
            "test": {"name": "t2"},
            "environments": [{"dimensions": {"device_type": "AEMU"}}],
        }
        build_tests_json.override_product_bundle_test_environments(
            test_pb_override,
            group_envs=[vim3_env],
        )
        self.assertEqual(
            test_pb_override["environments"],
            [{"dimensions": {"device_type": "Vim3"}}],
        )

        # 3. Product bundle test group mode with allowed_envs and default_envs
        test_pb_no_override: dict[str, T.Any] = {
            "test": {"name": "t3"},
            "environments": [
                {"dimensions": {"device_type": "AEMU"}},
                {"dimensions": {"device_type": "Vim3"}},
            ],
        }
        build_tests_json.resolve_product_bundle_test_environments(
            test_pb_no_override,
            allowed_envs=[vim3_env],
            default_envs=[vim3_env],
            target_platforms=target_p,
            other_platforms=other_p,
        )
        self.assertEqual(
            test_pb_no_override["environments"],
            [{"dimensions": {"device_type": "Vim3"}}],
        )

        # Invalid test environment raises ValueError immediately
        test_invalid: dict[str, T.Any] = {
            "test": {"name": "bad"},
            "environments": [{"dimensions": {"device_type": "UnknownDevice"}}],
        }
        with self.assertRaises(ValueError):
            build_tests_json.resolve_general_test_environments(
                test_invalid,
                default_envs=[aemu_env],
                allowed_device_types={"AEMU"},
                allowed_host_device_types=set(),
                target_platforms=target_p,
                other_platforms=other_p,
            )


if __name__ == "__main__":
    unittest.main()

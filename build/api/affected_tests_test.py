#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import re
import tempfile
import typing as T
import unittest
from pathlib import Path

from build.api import affected_tests, ninja_artifacts
from build.bazel.scripts.build_utils import (
    BazelPaths,
    CommandResult,
    MockBazelLauncher,
    MockNinjaRunner,
)


class QueryRecordingMockBazelLauncher(MockBazelLauncher):
    """A mock BazelLauncher that records query arguments and --query_file contents."""

    def __init__(
        self,
        target_to_bzl_map: dict[str, list[str]],
        target_to_sources_map: dict[str, list[str]] | None = None,
        returncode: int = 0,
    ) -> None:
        super().__init__()
        self.target_to_bzl_map = target_to_bzl_map
        self.target_to_sources_map = target_to_sources_map or {}
        self.returncode = returncode
        self.queries: list[list[str]] = []
        self.query_expressions: list[str] = []

    def run_query(
        self, query_type: str, query_args: list[str], ignore_errors: bool
    ) -> CommandResult:
        self.queries.append(query_args)
        query_str = query_args[-1]
        for arg in query_args:
            if arg.startswith("--query_file="):
                query_str = Path(arg.removeprefix("--query_file=")).read_text()
                break
        self.query_expressions.append(query_str)

        universe_targets: set[str] | None = None
        for arg in query_args:
            if arg.startswith("--universe_scope="):
                universe_targets = set(
                    arg.removeprefix("--universe_scope=").split(",")
                )
                break

        affected: set[str] = set()
        rbuildfiles_match = re.search(r"rbuildfiles\((.*?)\)", query_str)
        if rbuildfiles_match:
            queried_bzls = {
                item.replace('\\"', '"').replace("\\\\", "\\")
                for item in re.findall(
                    r'"((?:\\.|[^"\\])*)"', rbuildfiles_match.group(1)
                )
            }
            for target, bzls in self.target_to_bzl_map.items():
                if (
                    universe_targets is not None
                    and target not in universe_targets
                ):
                    continue
                normalized_bzls = {
                    b.removeprefix("@@//").replace(":", "/") for b in bzls
                }
                if queried_bzls & (set(bzls) | normalized_bzls):
                    affected.add(target)

        set_match = re.search(r"set\((.*)\)", query_str)
        if set_match:
            set_body = set_match.group(1)
            # If rbuildfiles(...) follows set(...), trim at the closing ')' of set(...)
            if ") + siblings(" in set_body:
                set_body = set_body.partition(") + siblings(")[0]
            queried_sources = {
                item.replace('\\"', '"').replace("\\\\", "\\")
                for item in re.findall(r'"((?:\\.|[^"\\])*)"', set_body)
            }
            for target, sources in self.target_to_sources_map.items():
                if (
                    universe_targets is not None
                    and target not in universe_targets
                ):
                    continue
                if queried_sources & set(sources):
                    affected.add(target)

        return CommandResult(
            returncode=self.returncode,
            stdout="\n".join(sorted(affected)),
            stderr="",
        )


class ChunkByCharLimitTest(unittest.TestCase):
    def test_empty(self) -> None:
        self.assertEqual(
            affected_tests._chunk_by_char_limit(
                [], separator=",", max_chars=10
            ),
            [],
        )

    def test_fits_in_one_chunk(self) -> None:
        self.assertEqual(
            affected_tests._chunk_by_char_limit(
                ["aa", "bb", "cc"], separator=",", max_chars=8
            ),
            [["aa", "bb", "cc"]],
        )

    def test_separator_counts_against_the_limit(self) -> None:
        # The three items are 6 chars on their own, but "aa,bb,cc" is 8, so a
        # 7 char budget must split them.
        self.assertEqual(
            affected_tests._chunk_by_char_limit(
                ["aa", "bb", "cc"], separator=",", max_chars=7
            ),
            [["aa", "bb"], ["cc"]],
        )

    def test_longer_separator_splits_earlier(self) -> None:
        self.assertEqual(
            affected_tests._chunk_by_char_limit(
                ["aa", "bb", "cc"], separator=", ", max_chars=7
            ),
            [["aa", "bb"], ["cc"]],
        )

    def test_oversized_item_gets_its_own_chunk(self) -> None:
        # An item longer than max_chars cannot be split any further, so it is
        # emitted alone rather than dropped or merged with a neighbor.
        self.assertEqual(
            affected_tests._chunk_by_char_limit(
                ["a", "toolongtofit", "b"], separator=",", max_chars=4
            ),
            [["a"], ["toolongtofit"], ["b"]],
        )

    def test_no_joined_chunk_exceeds_limit(self) -> None:
        items = [f"item{i}" for i in range(100)]
        for max_chars in (6, 7, 13, 20, 999):
            chunks = affected_tests._chunk_by_char_limit(
                items, separator=",", max_chars=max_chars
            )
            self.assertEqual(
                [item for chunk in chunks for item in chunk], items
            )
            for chunk in chunks:
                if len(chunk) > 1:
                    self.assertLessEqual(len(",".join(chunk)), max_chars)


class CreateTestArtifactsMappingTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self.root = Path(self._td.name)
        (self.root / ".jiri_manifest").write_text("")
        self.build_dir = self.root / "out/build"
        self.build_dir.mkdir(parents=True)
        self.tests_json_path = self.build_dir / "tests.json"
        BazelPaths.write_topdir_config_for_test(self.root, "bazel_topdir")

    def tearDown(self) -> None:
        self._td.cleanup()

    def write_json(self, path: Path, tests: T.Any) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("wt") as f:
            json.dump(tests, f)

    def test_no_tests(self) -> None:
        self.write_json(self.tests_json_path, [])
        result = affected_tests.create_gn_test_artifacts_mapping(self.build_dir)
        self.assertDictEqual(result, {})

    HOST_TEST_LABEL = "//src/microfuchsia:pkvm-hello-world-test(//build/toolchain/fuchsia:arm64)"
    HOST_TEST_RUNTIME_DEPS_PATH = (
        "gen/src/microfuchsia/pkvm-hello-world-test.host-arm64.deps.json"
    )
    HOST_TEST_RUNTIME_DEPS = [
        "arm64-shared/obj/sdk/fidl/fuchsia.gpu.virtio/fuchsia.gpu.virtio_bindlib/test_data/bind-tests/fuchsia.gpu.virtio.bind",
        "host_x64/seriallistener",
        "zbi-hello-world-test.host-arm64.sh",
    ]

    HOST_TEST_ENTRY = {
        "environments": [],
        "expects_ssh": False,
        "is_boot_test": True,
        "product_bundle": "pkvm-hello-world-test",
        "test": {
            "cpu": "arm64",
            "isolated": True,
            "label": HOST_TEST_LABEL,
            "log_settings": {"max_severity": "WARN"},
            "name": "pkvm-hello-world-test",
            "os": "linux",
            "path": "pkvm-hello-world-test.host-arm64.sh",
            "runtime_deps": HOST_TEST_RUNTIME_DEPS_PATH,
            "timeout_secs": 600,
        },
    }

    HOST_TEST_EXPECTED_SET = {
        "arm64-shared/obj/sdk/fidl/fuchsia.gpu.virtio/fuchsia.gpu.virtio_bindlib/test_data/bind-tests/fuchsia.gpu.virtio.bind",
        "host_x64/seriallistener",
        "pkvm-hello-world-test.host-arm64.sh",
        "zbi-hello-world-test.host-arm64.sh",
        HOST_TEST_RUNTIME_DEPS_PATH,
    }

    DEVICE_TEST_LABEL = "//src/power/testing/system-integration/example:bootstrap_pkg(//build/toolchain/fuchsia:arm64)"
    DEVICE_TEST_PACKAGE_MANIFEST_DEPS_PATH = "gen/src/power/testing/system-integration/example/bootstrap_pkg_test_bootstrap_component.pkg_manifests.json"
    DEVICE_TEST_PACKAGE_MANIFEST_DEPS = [
        "obj/src/developer/debug/debug_agent/debug_agent/package_manifest.json",
    ]

    DEVICE_TEST_RUNTIME_DEPS_PATH = "gen/src/power/testing/system-integration/example/bootstrap_pkg_test_bootstrap_component.deps.json"
    DEVICE_TEST_RUNTIME_DEPS = [
        "obj/src/devices/bind/fuchsia.test/fuchsia.test/test_data/bind-tests/fuchsia.test.bind",
        "host_x64/test-pilot",
        "bootstrap_power_system_integration_example_test_pkg_bootstrap_power_system_integration_example_test.cm_test.sh",
        "test_configs/bootstrap_power_system_integration_example_test_pkg.bootstrap_power_system_integration_example_test.cm.test_config.json",
    ]

    DEVICE_TEST_ENTRY = {
        "environments": [{"dimensions": {"device_type": "QEMU"}}],
        "expects_ssh": False,
        "test": {
            "build_rule": "fuchsia_bootfs_test_package",
            "component_label": "//src/power/testing/system-integration/example:bootstrap_component(//build/toolchain/fuchsia:arm64)",
            "cpu": "arm64",
            "label": DEVICE_TEST_LABEL,
            "log_settings": {"max_severity": "WARN"},
            "name": "fuchsia-boot:///bootstrap_power_system_integration_example_test_pkg#meta/bootstrap_power_system_integration_example_test.cm",
            "new_path": "bootstrap_power_system_integration_example_test_pkg_bootstrap_power_system_integration_example_test.cm_test.sh",
            "os": "fuchsia",
            "package_label": DEVICE_TEST_LABEL,
            "package_manifest_deps": DEVICE_TEST_PACKAGE_MANIFEST_DEPS_PATH,
            "package_manifests": [
                "obj/src/power/testing/system-integration/example/bootstrap_pkg/package_manifest.json"
            ],
            "package_url": "fuchsia-boot:///bootstrap_power_system_integration_example_test_pkg#meta/bootstrap_power_system_integration_example_test.cm",
            "runtime_deps": DEVICE_TEST_RUNTIME_DEPS_PATH,
        },
    }

    DEVICE_TEST_EXPECTED_SET = {
        "gen/src/power/testing/system-integration/example/bootstrap_pkg_test_bootstrap_component.pkg_manifests.json",
        "bootstrap_power_system_integration_example_test_pkg_bootstrap_power_system_integration_example_test.cm_test.sh",
        "gen/src/power/testing/system-integration/example/bootstrap_pkg_test_bootstrap_component.deps.json",
        "host_x64/test-pilot",
        "obj/src/developer/debug/debug_agent/debug_agent/package_manifest.json",
        "obj/src/devices/bind/fuchsia.test/fuchsia.test/test_data/bind-tests/fuchsia.test.bind",
        "obj/src/power/testing/system-integration/example/bootstrap_pkg/package_manifest.json",
        "test_configs/bootstrap_power_system_integration_example_test_pkg.bootstrap_power_system_integration_example_test.cm.test_config.json",
        DEVICE_TEST_RUNTIME_DEPS_PATH,
        DEVICE_TEST_PACKAGE_MANIFEST_DEPS_PATH,
    }

    def test_single_host_test(self) -> None:
        self.write_json(self.tests_json_path, [self.HOST_TEST_ENTRY])
        self.write_json(
            self.build_dir / self.HOST_TEST_RUNTIME_DEPS_PATH,
            self.HOST_TEST_RUNTIME_DEPS,
        )

        mapping = affected_tests.create_gn_test_artifacts_mapping(
            self.build_dir
        )
        self.assertEqual(len(mapping), 1)

        label, test_info = mapping.popitem()
        self.assertEqual(label, self.HOST_TEST_LABEL)
        self.assertEqual(test_info.os_name, "linux")
        self.assertSetEqual(
            test_info.ninja_artifacts, self.HOST_TEST_EXPECTED_SET
        )

    def test_single_device_test(self) -> None:
        self.write_json(self.tests_json_path, [self.DEVICE_TEST_ENTRY])
        self.write_json(
            self.build_dir / self.DEVICE_TEST_RUNTIME_DEPS_PATH,
            self.DEVICE_TEST_RUNTIME_DEPS,
        )
        self.write_json(
            self.build_dir / self.DEVICE_TEST_PACKAGE_MANIFEST_DEPS_PATH,
            self.DEVICE_TEST_PACKAGE_MANIFEST_DEPS,
        )

        mapping = affected_tests.create_gn_test_artifacts_mapping(
            self.build_dir
        )

        self.assertEqual(len(mapping), 1)

        target_label, test_info = mapping.popitem()
        self.assertEqual(target_label, self.DEVICE_TEST_LABEL)
        self.assertEqual(test_info.os_name, "fuchsia")
        self.assertSetEqual(
            test_info.ninja_artifacts, self.DEVICE_TEST_EXPECTED_SET
        )

    def test_multiple_tests(self) -> None:
        self.write_json(
            self.tests_json_path, [self.HOST_TEST_ENTRY, self.DEVICE_TEST_ENTRY]
        )
        self.write_json(
            self.build_dir / self.HOST_TEST_RUNTIME_DEPS_PATH,
            self.HOST_TEST_RUNTIME_DEPS,
        )
        self.write_json(
            self.build_dir / self.DEVICE_TEST_RUNTIME_DEPS_PATH,
            self.DEVICE_TEST_RUNTIME_DEPS,
        )
        self.write_json(
            self.build_dir / self.DEVICE_TEST_PACKAGE_MANIFEST_DEPS_PATH,
            self.DEVICE_TEST_PACKAGE_MANIFEST_DEPS,
        )

        mapping = affected_tests.create_gn_test_artifacts_mapping(
            self.build_dir
        )

        self.assertEqual(len(mapping), 2)

        target_label, test_info = mapping.popitem()
        self.assertEqual(target_label, self.DEVICE_TEST_LABEL)
        self.assertEqual(test_info.os_name, "fuchsia")
        self.assertSetEqual(
            test_info.ninja_artifacts, self.DEVICE_TEST_EXPECTED_SET
        )

        target_label, test_info = mapping.popitem()
        self.assertEqual(target_label, self.HOST_TEST_LABEL)
        self.assertEqual(test_info.os_name, "linux")
        self.assertSetEqual(
            test_info.ninja_artifacts, self.HOST_TEST_EXPECTED_SET
        )


class FindTestsAffectedByChangedFilesTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self.root = Path(self._td.name)
        (self.root / ".jiri_manifest").write_text("")
        self.build_dir = self.root / "out/build"
        self.build_dir.mkdir(parents=True)
        BazelPaths.write_topdir_config_for_test(self.root, "bazel_topdir")
        self.bazel_paths = BazelPaths.new(self.root, self.build_dir)
        self.bazel_paths.output_base.mkdir(parents=True)

        (self.root / "BUILD.gn").touch()
        (
            self.build_dir / ninja_artifacts.NINJA_BUILD_PLAN_DEPS_FILE
        ).write_text("build.ninja.stamp: ../../BUILD.gn\n")

        (self.build_dir / ninja_artifacts.LAST_NINJA_TARGETS_FILE).write_text(
            ":default\n"
        )

        self.ninja_artifacts_path = (
            self.build_dir / ninja_artifacts.LAST_NINJA_ARTIFACTS_FILE
        )
        self.ninja_artifacts_path.write_text(
            "\n".join(
                [
                    "obj/gn/target1",
                    "obj/gn/target1.o",
                    "obj/bazel/target2.bazel_outputs/foo",
                    "obj/bazel/target2.bazel_outputs/package_manifest.json",
                    "obj/some/target2.out",
                    "test-list.json",
                    "test-config.json",
                ]
            )
            + "\n"
        )
        st = self.ninja_artifacts_path.stat()
        os.utime(self.ninja_artifacts_path, (st.st_atime, st.st_mtime + 100))

        (self.root / "src/bazel").mkdir(parents=True)
        (self.root / "src/bazel/BUILD.bazel").touch()

        tests_json = [
            {
                "test": {
                    "label": "//gn:target1",
                    "path": "obj/gn/target1",
                    "os": "fuchsia",
                },
            },
            {
                "test": {
                    "label": "//bazel:target2",  # A Bazel test wrapped by a GN target.
                    "package_manifests": [
                        "obj/bazel/target2.bazel_outputs/package_manifest.json",
                    ],
                    "os": "linux",
                },
            },
            {
                "test": {
                    "label": "@@//src/bazel:target3",  # A Bazel test target.
                    "path": "bazel-bin/src/bazel/target3_bin",
                    "os": "linux",
                },
            },
        ]

        self.tests_json_path = self.build_dir / "tests.json"
        with self.tests_json_path.open("wt") as f:
            json.dump(tests_json, f)

    def tearDown(self) -> None:
        self._td.cleanup()

    def test_no_change(self) -> None:
        result = affected_tests.find_tests_affected_by_changed_files(
            ["some/file.txt"],
            self.root,
            MockNinjaRunner(self.build_dir, "obj/some/target2.out\n"),
            MockBazelLauncher.new_with_empty_outputs(),
        )
        self.assertSetEqual(result.affected_tests, set())
        self.assertFalse(result.build_not_affected)

    def test_build_not_affected(self) -> None:
        result = affected_tests.find_tests_affected_by_changed_files(
            ["docs/README.md"],
            self.root,
            MockNinjaRunner(self.build_dir, ""),
            MockBazelLauncher.new_with_empty_outputs(),
        )
        self.assertSetEqual(result.affected_tests, set())
        self.assertTrue(result.build_not_affected)

    def test_unbuilt_target_does_not_affect_build(self) -> None:
        # A file change affects obj/some/unbuilt.out in build.ninja, but that
        # target was not built by this builder (not in last_build_artifacts).
        result = affected_tests.find_tests_affected_by_changed_files(
            ["some/unbuilt_file.txt"],
            self.root,
            MockNinjaRunner(self.build_dir, "obj/some/unbuilt.out\n"),
            MockBazelLauncher.new_with_empty_outputs(),
        )
        self.assertSetEqual(result.affected_tests, set())
        self.assertTrue(result.build_not_affected)

    def test_build_gn_change_affects_build(self) -> None:
        # A BUILD.gn or .gni change that does not directly define a test (e.g.
        # a test bundle in bundles/buildbot/.../BUILD.gn) causes Ninja to output
        # build.ninja.stamp. Because build.ninja.stamp is part of this builder's
        # build plan, build_not_affected must be False so all tests are run.
        result = affected_tests.find_tests_affected_by_changed_files(
            ["bundles/buildbot/some_bundle/BUILD.gn"],
            self.root,
            MockNinjaRunner(self.build_dir, "build.ninja.stamp\n"),
            MockBazelLauncher.new_with_empty_outputs(),
        )
        self.assertSetEqual(result.affected_tests, set())
        self.assertFalse(result.build_not_affected)

    def test_one_target_affected(self) -> None:
        targets = affected_tests.find_tests_affected_by_changed_files(
            ["gn/source.txt"],
            self.root,
            MockNinjaRunner(
                self.build_dir,
                "\n".join(["obj/gn/target1", "obj/gn/target1.o"]),
            ),
            MockBazelLauncher.new_with_empty_outputs(),
        ).affected_tests
        self.assertSetEqual(
            targets,
            {affected_tests.AffectedTestTarget("//gn:target1", "fuchsia")},
        )

        targets = affected_tests.find_tests_affected_by_changed_files(
            ["bazel/source.txt"],
            self.root,
            MockNinjaRunner(
                self.build_dir,
                "\n".join(
                    [
                        "obj/bazel/target2.bazel_outputs/foo",
                        "obj/bazel/target2.bazel_outputs/package_manifest.json",
                    ]
                ),
            ),
            MockBazelLauncher.new_with_empty_outputs(),
        ).affected_tests
        self.maxDiff = None
        self.assertSetEqual(
            targets,
            {affected_tests.AffectedTestTarget("//bazel:target2", "linux")},
        )

        bazel_launcher = QueryRecordingMockBazelLauncher(
            target_to_bzl_map={},
            target_to_sources_map={
                "@@//src/bazel:target3": ["@@//:bazel/test3.cc"],
            },
        )

        targets = affected_tests.find_tests_affected_by_changed_files(
            ["bazel/test3.cc"],
            self.root,
            MockNinjaRunner(self.build_dir, ""),
            bazel_launcher,
        ).affected_tests
        self.maxDiff = None
        self.assertSetEqual(
            targets,
            {
                affected_tests.AffectedTestTarget(
                    "@@//src/bazel:target3", "linux"
                )
            },
        )
        self.assertEqual(len(bazel_launcher.queries), 1)
        self.assertIn(
            "--universe_scope=@@//src/bazel:target3",
            bazel_launcher.queries[0],
        )
        self.assertEqual(
            bazel_launcher.query_expressions[0],
            'allrdeps(set("@@//:bazel/test3.cc"))',
        )

        targets = affected_tests.find_tests_affected_by_changed_files(
            ["bazel/source.txt", "gn/source.txt"],
            self.root,
            MockNinjaRunner(
                self.build_dir,
                "\n".join(
                    [
                        "obj/bazel/target2.bazel_outputs/foo",
                        "obj/bazel/target2.bazel_outputs/package_manifest.json",
                        "obj/gn/target1.o",
                        "obj/gn/target1",
                    ]
                ),
            ),
            MockBazelLauncher.new_with_empty_outputs(),
        ).affected_tests
        self.assertSetEqual(
            targets,
            {
                affected_tests.AffectedTestTarget("//gn:target1", "fuchsia"),
                affected_tests.AffectedTestTarget("//bazel:target2", "linux"),
            },
        )

    def test_native_bazel_target_affected(self) -> None:
        tests_json = [
            {
                "test": {
                    "label": "@@//src/bazel:test1",
                    "os": "linux",
                },
            },
            {
                "test": {
                    "label": "@@//src/bazel:test2",
                    "os": "linux",
                }
            },
        ]
        with self.tests_json_path.open("wt") as f:
            json.dump(tests_json, f)

        MockNinjaRunner(self.build_dir, "")

        mock_bazel_launcher = MockBazelLauncher()
        mock_bazel_launcher.push_expected_outputs(
            [
                # Result of allrdeps(set(//src/bazel:test1.cc)) query
                "@@//src/bazel:test1\n",
            ]
        )

        # First, check that if the source of only one test is modified, only that specific
        # test is reported.
        targets = affected_tests.find_tests_affected_by_changed_files(
            ["src/bazel/test1.cc"],
            self.root,
            MockNinjaRunner(self.build_dir, ""),
            mock_bazel_launcher,
        ).affected_tests

        self.assertSetEqual(
            targets,
            {affected_tests.AffectedTestTarget("@@//src/bazel:test1", "linux")},
        )

        # Do the same for the second test.
        mock_bazel_launcher.push_expected_outputs(
            [
                # Result of allrdeps(set(//src/bazel:test2.cc)) query
                "@@//src/bazel:test2\n",
            ]
        )

        targets = affected_tests.find_tests_affected_by_changed_files(
            ["src/bazel/test2.cc"],
            self.root,
            MockNinjaRunner(self.build_dir, ""),
            mock_bazel_launcher,
        ).affected_tests
        self.assertSetEqual(
            targets,
            {affected_tests.AffectedTestTarget("@@//src/bazel:test2", "linux")},
        )

        # Do the same for a build file.
        mock_bazel_launcher.push_expected_outputs(
            [
                # Result of allrdeps(set(//src/bazel:all)) query
                "@@//src/bazel:test1\n"
                + "@@//src/bazel:test2\n",
            ]
        )
        targets = affected_tests.find_tests_affected_by_changed_files(
            ["src/bazel/BUILD.bazel"],
            self.root,
            MockNinjaRunner(self.build_dir, ""),
            mock_bazel_launcher,
        ).affected_tests
        self.assertSetEqual(
            targets,
            {
                affected_tests.AffectedTestTarget(
                    "@@//src/bazel:test1", "linux"
                ),
                affected_tests.AffectedTestTarget(
                    "@@//src/bazel:test2", "linux"
                ),
            },
        )

    def test_bzl_file_changes_single_query(self) -> None:
        tests_json = [
            {
                "test": {
                    "label": "@@//src/bazel:test1",
                    "os": "linux",
                },
            },
            {
                "test": {
                    "label": "@@//src/bazel:test2",
                    "os": "linux",
                }
            },
            {
                "test": {
                    "label": "@@//src/bazel:test3",
                    "os": "linux",
                }
            },
            {
                "test": {
                    "label": "@@//src/bazel:test4",
                    "os": "linux",
                }
            },
        ]
        with self.tests_json_path.open("wt") as f:
            json.dump(tests_json, f)

        mock_ninja_runner = MockNinjaRunner(self.build_dir, "")

        target_to_bzl_map: dict[str, list[str]] = {
            "@@//src/bazel:test1": [],
            "@@//src/bazel:test2": ["@@//src/bazel:foo.bzl"],
            "@@//src/bazel:test3": [],
            "@@//src/bazel:test4": [],
        }

        mock_bazel_launcher = QueryRecordingMockBazelLauncher(
            target_to_bzl_map=target_to_bzl_map,
            target_to_sources_map={
                "@@//src/bazel:test4": ["@@//src/bazel:test4.cc"],
            },
        )

        targets = affected_tests.find_tests_affected_by_changed_files(
            ["src/bazel/foo.bzl"],
            self.root,
            mock_ninja_runner,
            mock_bazel_launcher,
        ).affected_tests

        self.assertSetEqual(
            targets,
            {affected_tests.AffectedTestTarget("@@//src/bazel:test2", "linux")},
        )

        self.assertEqual(len(mock_bazel_launcher.queries), 1)
        self.assertIn(
            "--universe_scope=@@//src/bazel:test1,@@//src/bazel:test2,@@//src/bazel:test3,@@//src/bazel:test4",
            mock_bazel_launcher.queries[0],
        )
        self.assertEqual(
            mock_bazel_launcher.query_expressions[0],
            'allrdeps(siblings(rbuildfiles("src/bazel/foo.bzl")))',
        )

        # Verify that when both source files (including filenames with query
        # punctuation such as parentheses) and .bzl files change, a single
        # combined query expression with quoted labels is executed via --query_file,
        # and partial analysis exit code 3 (--keep_going) is accepted.
        mock_bazel_launcher.queries.clear()
        mock_bazel_launcher.query_expressions.clear()
        mock_bazel_launcher.returncode = 3
        targets = affected_tests.find_tests_affected_by_changed_files(
            ["src/bazel/foo.bzl", "src/bazel/test4.cc", "src/foo/bar(1).txt"],
            self.root,
            MockNinjaRunner(self.build_dir, ""),
            mock_bazel_launcher,
        ).affected_tests
        self.assertSetEqual(
            targets,
            {
                affected_tests.AffectedTestTarget(
                    "@@//src/bazel:test2", "linux"
                ),
                affected_tests.AffectedTestTarget(
                    "@@//src/bazel:test4", "linux"
                ),
            },
        )
        self.assertEqual(len(mock_bazel_launcher.queries), 1)
        self.assertEqual(
            mock_bazel_launcher.query_expressions[0],
            'allrdeps(set("@@//:src/foo/bar(1).txt" "@@//src/bazel:test4.cc") + siblings(rbuildfiles("src/bazel/foo.bzl")))',
        )

        # Verify that fatal bazel query exit codes (such as 2 for syntax/flag
        # errors) raise RuntimeError instead of silently returning zero affected tests.
        mock_bazel_launcher.returncode = 2
        with self.assertRaises(RuntimeError):
            affected_tests.find_tests_affected_by_changed_files(
                ["src/bazel/foo.bzl"],
                self.root,
                MockNinjaRunner(self.build_dir, ""),
                mock_bazel_launcher,
            )

    def test_large_inputs_query_file_and_chunking(self) -> None:
        tests_json = [
            {
                "test": {
                    "label": "@@//src/bazel:test1",
                    "os": "linux",
                },
            },
            {
                "test": {
                    "label": "@@//src/bazel:test2",
                    "os": "linux",
                },
            },
        ]
        with self.tests_json_path.open("wt") as f:
            json.dump(tests_json, f)

        mock_bazel_launcher = QueryRecordingMockBazelLauncher(
            target_to_bzl_map={
                "@@//src/bazel:test2": ["src/bazel/foo.bzl"],
            },
            target_to_sources_map={
                "@@//src/bazel:test1": ["@@//src/bazel:file_1999.cc"],
            },
        )

        # 2,000 changed files (> 50 KB of labels) must be written to --query_file
        # rather than passed as a raw command-line argument, and chunking
        # --universe_scope when _MAX_UNIVERSE_SCOPE_CHARS is small must union
        # results across all universe chunks.
        changed_files = [f"src/bazel/file_{i}.cc" for i in range(2000)] + [
            "src/bazel/foo.bzl"
        ]
        orig_limit = affected_tests._MAX_SINGLE_ARG_CHARS
        try:
            affected_tests._MAX_SINGLE_ARG_CHARS = 25
            targets = affected_tests.find_tests_affected_by_changed_files(
                changed_files,
                self.root,
                MockNinjaRunner(self.build_dir, ""),
                mock_bazel_launcher,
            ).affected_tests
        finally:
            affected_tests._MAX_SINGLE_ARG_CHARS = orig_limit

        self.assertEqual(len(mock_bazel_launcher.queries), 2)
        for query_args in mock_bazel_launcher.queries:
            self.assertTrue(
                all(len(arg) < 200 for arg in query_args),
                f"Expected short CLI arguments using --query_file, got {[len(a) for a in query_args]}",
            )
        self.assertSetEqual(
            targets,
            {
                affected_tests.AffectedTestTarget(
                    "@@//src/bazel:test1", "linux"
                ),
                affected_tests.AffectedTestTarget(
                    "@@//src/bazel:test2", "linux"
                ),
            },
        )

    def test_gn_label_to_build_gn_path(self) -> None:
        self.assertEqual(
            affected_tests.gn_label_to_build_gn_path(
                "//src/foo/bar:bar(//build/toolchain/fuchsia:x64)"
            ),
            "src/foo/bar/BUILD.gn",
        )
        self.assertEqual(
            affected_tests.gn_label_to_build_gn_path("//src/foo/bar:bar"),
            "src/foo/bar/BUILD.gn",
        )
        self.assertEqual(
            affected_tests.gn_label_to_build_gn_path("//src/foo"),
            "src/foo/BUILD.gn",
        )
        self.assertEqual(
            affected_tests.gn_label_to_build_gn_path("//:root_target"),
            "BUILD.gn",
        )
        self.assertEqual(
            affected_tests.gn_label_to_build_gn_path("@@//src/foo:bar"),
            "",
        )
        self.assertEqual(
            affected_tests.gn_label_to_build_gn_path(""),
            "",
        )

    def test_gn_test_affected_by_build_gn(self) -> None:
        tests_json = [
            {
                "test": {
                    "label": "//src/foo:foo_test(//build/toolchain:x64)",
                    "package_label": "//src/foo:foo_pkg(//build/toolchain:x64)",
                    "os": "fuchsia",
                },
            },
            {
                "test": {
                    "label": "//src/bar/sub:bar_test(//build/toolchain:x64)",
                    "package_label": "//src/bar/pkg:bar_pkg(//build/toolchain:x64)",
                    "os": "linux",
                },
            },
        ]
        with self.tests_json_path.open("wt") as f:
            json.dump(tests_json, f)

        # MockNinjaRunner returns only build.ninja.stamp as ninja -t affected does for BUILD.gn
        def new_mock_ninja_runner() -> MockNinjaRunner:
            return MockNinjaRunner(self.build_dir, "build.ninja.stamp\n")

        mock_bazel_launcher = MockBazelLauncher.new_with_empty_outputs()

        # 1. Modifying the test's target BUILD.gn marks it as affected
        targets = affected_tests.find_tests_affected_by_changed_files(
            ["src/foo/BUILD.gn"],
            self.root,
            new_mock_ninja_runner(),
            mock_bazel_launcher,
        ).affected_tests
        self.assertSetEqual(
            targets,
            {
                affected_tests.AffectedTestTarget(
                    "//src/foo:foo_test(//build/toolchain:x64)", "fuchsia"
                )
            },
        )

        # 2. Modifying the test package's BUILD.gn marks it as affected
        targets = affected_tests.find_tests_affected_by_changed_files(
            ["src/bar/pkg/BUILD.gn"],
            self.root,
            new_mock_ninja_runner(),
            mock_bazel_launcher,
        ).affected_tests
        self.assertSetEqual(
            targets,
            {
                affected_tests.AffectedTestTarget(
                    "//src/bar/sub:bar_test(//build/toolchain:x64)", "linux"
                )
            },
        )

        # 3. Modifying secondary overlay BUILD.gn marks it as affected
        tests_json_secondary = [
            {
                "test": {
                    "label": "//third_party/libfoo:libfoo_test(//build/toolchain:x64)",
                    "os": "linux",
                },
            }
        ]
        with self.tests_json_path.open("wt") as f:
            json.dump(tests_json_secondary, f)

        targets = affected_tests.find_tests_affected_by_changed_files(
            ["build/secondary/third_party/libfoo/BUILD.gn"],
            self.root,
            new_mock_ninja_runner(),
            mock_bazel_launcher,
        ).affected_tests
        self.assertSetEqual(
            targets,
            {
                affected_tests.AffectedTestTarget(
                    "//third_party/libfoo:libfoo_test(//build/toolchain:x64)",
                    "linux",
                )
            },
        )

        # 4. Modifying unrelated BUILD.gn does not mark any test as affected
        targets = affected_tests.find_tests_affected_by_changed_files(
            ["src/unrelated/BUILD.gn"],
            self.root,
            new_mock_ninja_runner(),
            mock_bazel_launcher,
        ).affected_tests
        self.assertSetEqual(targets, set())

        # 5. Modifying only non-BUILD.gn files does not match BUILD.gn logic
        targets = affected_tests.find_tests_affected_by_changed_files(
            ["src/foo/some_file.cc"],
            self.root,
            new_mock_ninja_runner(),
            mock_bazel_launcher,
        ).affected_tests
        self.assertSetEqual(targets, set())


if __name__ == "__main__":
    unittest.main()

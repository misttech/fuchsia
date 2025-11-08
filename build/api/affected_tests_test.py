#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import sys
import tempfile
import typing as T
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(__file__))
import affected_tests
import ninja_artifacts
from ninja_artifacts import MockNinjaRunner


class CreateTestArtifactsMappingTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self.root = Path(self._td.name)
        self.build_dir = self.root / "out/build"
        self.build_dir.mkdir(parents=True)
        self.tests_json_path = self.build_dir / "tests.json"

    def tearDown(self) -> None:
        self._td.cleanup()

    def write_json(self, path: Path, tests: T.Any) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("wt") as f:
            json.dump(tests, f)

    def test_no_tests(self) -> None:
        self.write_json(self.tests_json_path, [])
        result = affected_tests.create_test_artifacts_mapping(self.build_dir)
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

        mapping = affected_tests.create_test_artifacts_mapping(self.build_dir)
        self.assertEqual(len(mapping), 1)

        label, artifacts = mapping.popitem()
        self.assertEqual(label, self.HOST_TEST_LABEL)
        self.assertSetEqual(artifacts, self.HOST_TEST_EXPECTED_SET)

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

        mapping = affected_tests.create_test_artifacts_mapping(self.build_dir)

        self.assertEqual(len(mapping), 1)

        target_label, artifacts = mapping.popitem()
        self.assertEqual(target_label, self.DEVICE_TEST_LABEL)
        self.assertSetEqual(artifacts, self.DEVICE_TEST_EXPECTED_SET)

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

        mapping = affected_tests.create_test_artifacts_mapping(self.build_dir)

        self.assertEqual(len(mapping), 2)

        target_label, artifacts = mapping.popitem()
        self.assertEqual(target_label, self.DEVICE_TEST_LABEL)
        self.assertSetEqual(artifacts, self.DEVICE_TEST_EXPECTED_SET)

        target_label, artifacts = mapping.popitem()
        self.assertEqual(target_label, self.HOST_TEST_LABEL)
        self.assertSetEqual(artifacts, self.HOST_TEST_EXPECTED_SET)


class FindTestsAffectedByChangedFilesTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self.root = Path(self._td.name)
        self.build_dir = self.root / "out/build"
        self.build_dir.mkdir(parents=True)

        (
            self.build_dir / ninja_artifacts.NINJA_BUILD_PLAN_DEPS_FILE
        ).write_text(
            "build.ninja.stamp: ../../BUILD.gn ../../src/foo.gni dep1 dep2 dep3 dep4"
        )

        tests_json = [
            {
                "test": {"label": "//gn:target1", "path": "obj/gn/target1"},
            },
            {
                "test": {
                    "label": "@//bazel:target2",
                    "package_manifests": [
                        "obj/bazel/target2.bazel_outputs/package_manifest.json",
                    ],
                },
            },
        ]

        self.tests_json_path = self.build_dir / "tests.json"
        with self.tests_json_path.open("wt") as f:
            json.dump(tests_json, f)

    def tearDown(self) -> None:
        self._td.cleanup()

    def test_no_change(self) -> None:
        targets = affected_tests.find_tests_affected_by_changed_files(
            ["some/file.txt"],
            self.root,
            MockNinjaRunner(self.build_dir, "obj/some/target2.out\n"),
        )
        self.assertSetEqual(targets, set())

    def test_one_target_affected(self) -> None:
        targets = affected_tests.find_tests_affected_by_changed_files(
            ["gn/source.txt"],
            self.root,
            MockNinjaRunner(
                self.build_dir,
                "\n".join(["obj/gn/target1", "obj/gn/target1.o"]),
            ),
        )
        self.assertSetEqual(targets, {"//gn:target1"})

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
        )
        self.assertSetEqual(targets, {"@//bazel:target2"})

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
        )
        self.assertSetEqual(targets, {"//gn:target1", "@//bazel:target2"})


if __name__ == "__main__":
    unittest.main()

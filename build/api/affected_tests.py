# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import dataclasses
import json
import os
import sys
from pathlib import Path

_SCRIPT_DIR = os.path.dirname(__file__)
sys.path.insert(0, _SCRIPT_DIR)
from ninja_artifacts import NinjaRunner

sys.path.insert(
    0, os.path.join(_SCRIPT_DIR, "..", "..", "build", "bazel", "scripts")
)


@dataclasses.dataclass(frozen=True)
class TestTargetInfo:
    """Description of a single tests.json entry.

    The format is documented at
    https://fuchsia.dev/fuchsia-src/reference/testing/tests-json-format
    """

    # For Bazel tests, begins with @ and relies on os_name to know which platform to build it.
    # Note that this is the label to use to build the test. When a Bazel test target is wrapped
    # by a bazel_test_package_group(), this label will be the label of the group, not the test,
    # which will appear in the "source_label" field, which is ignored here.
    label: str

    # The OS the test must run on. "linux" or "fuchsia".
    os_name: str

    # Used by host tests to point to the main test executable / script.
    path: str = ""

    # Used by device end-to-end tests to point to the host test runner executable.
    # See https://fxbug.dev/458823250.
    new_path: str = ""

    # A list of paths to package manifest files. Those are generated at build time,
    # so cannot be read directly, but they should be part of the affected artifacts when the
    # corresponding source file changes.
    package_manifests: list[str] = dataclasses.field(default_factory=list)

    # Points to a JSON file that contains an array of string paths to other package manifests,
    # corresponding to extra packages needed at runtime during testing. For GN tests, it is
    # generated at regeneration time, before the build, and is safe to read here. For Bazel tests
    # this is generated at build time, and cannot be read directly (the information must be
    # extracted with a query instead).
    package_manifest_deps: str = ""

    # Points to a JSON file that contains an array of string paths to Ninja artifacts needed at
    # runtime. For GN tests, it is generated at regeneration time, before the build, and is safe
    # to read here. For Bazel tests, this must be obtained with a query.
    runtime_deps: str = ""


def parse_tests_json(build_dir: Path) -> list[TestTargetInfo]:
    """Parse the tests.json file and return a list of TestTargetInfo values.

    Args:
        build_dir: Path to Ninja build directory.
    Returns:
        A list of TestTargetInfo values.
    """
    result: list[TestTargetInfo] = []

    tests_json_path = build_dir / "tests.json"
    with tests_json_path.open("rt") as f:
        tests_json = json.load(f)

    for entry in tests_json:
        test = entry["test"]
        test_label = test["label"]
        test_os = test["os"]

        path = test.get("path", "")
        if path:
            assert isinstance(path, str)

        new_path = test.get("new_path", "")
        if new_path:
            assert isinstance(new_path, str)

        package_manifests = test.get("package_manifests", [])
        if package_manifests:
            assert isinstance(package_manifests, list)

        package_manifest_deps_path = test.get("package_manifest_deps", "")
        if package_manifest_deps_path:
            assert isinstance(package_manifest_deps_path, str)

        runtime_deps_path = test.get("runtime_deps", "")
        if runtime_deps_path:
            assert isinstance(runtime_deps_path, str)

        result.append(
            TestTargetInfo(
                label=test_label,
                os_name=test_os,
                path=path,
                new_path=new_path,
                package_manifests=package_manifests,
                package_manifest_deps=package_manifest_deps_path,
                runtime_deps=runtime_deps_path,
            )
        )

    return result


def split_gn_and_bazel_tests(
    input_tests: list[TestTargetInfo],
) -> tuple[list[TestTargetInfo], list[TestTargetInfo]]:
    """Split a list of TestTargetInfo into GN and Bazel tests.

    Args:
        input_tests: List of TestTargetInfo values.
    Returns:
        A tuple of (gn_tests, bazel_tests).
    """
    gn_tests: list[TestTargetInfo] = []
    bazel_tests: list[TestTargetInfo] = []
    for test in input_tests:
        if test.label.startswith("@@"):
            bazel_tests.append(test)
        else:
            gn_tests.append(test)
    return gn_tests, bazel_tests


@dataclasses.dataclass(frozen=True)
class GnTestArtifactsInfo:
    # Name of the os this test must run on. "linux" or "fuchsia".
    os_name: str

    # Set of Ninja artifact paths, relative to the Ninja build directory
    ninja_artifacts: set[str]


class GnTestArtifactsMap(dict[str, GnTestArtifactsInfo]):
    """A mapping from GN test labels to their GnTestArtifactsInfo value."""


def create_gn_test_artifacts_mapping(build_dir: Path) -> GnTestArtifactsMap:
    gn_test_infos, bazel_test_infos = split_gn_and_bazel_tests(
        parse_tests_json(build_dir)
    )
    return _create_gn_test_artifacts_mapping(gn_test_infos, build_dir)


def _create_gn_test_artifacts_mapping(
    gn_test_infos: list[TestTargetInfo],
    build_dir: Path,
) -> GnTestArtifactsMap:
    """Generate a mapping from GN test labels to their OS and Ninja artifacts.

    Args:
        gn_test_infos: List of GN test targets.
        build_dir: Ninja build directory.
    Returns:
        A GnTestTargetMap value. The first element is the OS of the test (e.g. 'fuchsia' or 'linux').
        The second element is a set of Ninja artifact paths, relative to the Ninja
        build directory, that each test target should produce or use at
        runtime.
    """
    result = GnTestArtifactsMap()

    for test in gn_test_infos:
        target_label = test.label
        assert not target_label.startswith(
            "@@"
        ), f"Unexpected Bazel test label {target_label}"

        test_os = test.os_name

        # It is common to get duplicates because runtime_deps and package_manifest_deps
        # are the result of GN metadata collection, which does simple concatenation of
        # lists instead of unions of sets.
        artifacts: set[str] = set()

        if test.path:
            artifacts.add(test.path)

        if test.new_path:
            artifacts.add(test.new_path)

        # test.package_manifests are generated at build time, while they cannot be read directly,
        # they should be part of the affected artifacts when the corresponding source file changes.
        if test.package_manifests:
            artifacts.update(test.package_manifests)

        # test.package_manifest_deps is generated at regeneration time for GN tests, and is safe
        # to read here.
        if test.package_manifest_deps:
            with (build_dir / test.package_manifest_deps).open("rt") as f:
                manifests = json.load(f)
                assert isinstance(manifests, list)
                artifacts.update(manifests)
            artifacts.add(test.package_manifest_deps)

        # test.runtime_deps is generated at regeneration time for GN tests, and is safe
        # to read here. It points to a JSON file that contains an array of
        # string paths to Ninja artifacts needed at runtime.
        if test.runtime_deps:
            with (build_dir / test.runtime_deps).open("rt") as f:
                runtime_deps = json.load(f)
                assert isinstance(runtime_deps, list)
                artifacts.update(runtime_deps)
            artifacts.add(test.runtime_deps)

        result[target_label] = GnTestArtifactsInfo(
            os_name=test_os, ninja_artifacts=artifacts
        )

    return result


@dataclasses.dataclass(frozen=True)
class AffectedTestTarget:
    """Represents a test target and its operating system."""

    # The test label. GN test labels begin with // and always contain a toolchain suffix,
    # while Bazel test labels begin with @ and rely on os_name to know which platform to use
    # to build them.
    label: str
    os_name: str


def find_tests_affected_by_changed_files(
    changed_files: list[str],
    fuchsia_dir: Path,
    ninja_runner: NinjaRunner,
) -> set[AffectedTestTarget]:
    """Return the set of test labels that are affected by a set of changed files.

    Given a set of paths to changed files (for example after applying a
    git commit just after the last build), determine which targets need to
    be rebuilt (and for tests re-run), return the set of tests labels that
    would need to be rebuilt and then re-run after the build.

    Args:
        changed_files: List of file path strings, relative to Fuchsia source directory,
            of files that were changed since the last build.
        fuchsia_dir: Path to Fuchsia source directory.
        ninja_runner: A NinjaRunner instance.
    Returns:
        A set of tuples, each containing a test target label and its OS name.
    """
    changed_sources: set[str] = set()
    for file in changed_files:
        if os.path.isabs(file):
            changed_sources.add(os.path.relpath(file, fuchsia_dir))
        else:
            changed_sources.add(str(file))

    build_dir = ninja_runner.build_dir

    # Read the content of tests.json to determine which important artifacts
    # each test requires at runtime.
    test_artifacts = create_gn_test_artifacts_mapping(build_dir)

    # The list of source files as they must appear in the Ninja build plan.
    # All source inputs appear with a prefix like ../../ that corresponds
    # to the relative path from the build directory to the Fuchsia source one.
    source_prefix = os.path.relpath(fuchsia_dir, build_dir) + "/"
    ninja_sources = [
        f"{source_prefix}{source_path}"
        for source_path in sorted(changed_sources)
    ]

    # Run the 'affected' tool which returns the list of Ninja artifacts affected
    # by the changed sources. --ignore-errors is used because the changed file list
    # might include things that are not build plan inputs and should be ignored.
    # --depfile is used to ensure that implicit dependencies from the last build are
    # followed properly. This is critical for Bazel defined targets that are built
    # through bazel_action() GN target definitions.
    #
    # Note that for now, all Bazel targets, tests or not, must be wrapped through
    # GN bazel_action() targets.
    tool_output = ninja_runner.run_and_extract_output(
        [
            "-t",
            "affected",
            "--depfile",
            "--ignore-errors",
        ]
        + ninja_sources
    )

    affected_ninja_artifacts = set(tool_output.splitlines())

    return {
        AffectedTestTarget(label=test_label, os_name=test_info.os_name)
        for test_label, test_info in test_artifacts.items()
        if bool(test_info.ninja_artifacts & affected_ninja_artifacts)
    }

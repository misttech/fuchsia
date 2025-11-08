# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import sys
from pathlib import Path

sys.path.insert(0, os.path.dirname(__file__))
from ninja_artifacts import NinjaRunner


def create_test_artifacts_mapping(build_dir: Path) -> dict[str, set[str]]:
    """Generate a mapping from test labels to Ninja artifacts.

    Args:
        build_dir: Ninja build directory.
    Returns:
        A { target_label -> artifact_list } dictionary, mapping GN or Bazel
        target labels to a set of Ninja artifact paths, relative to the Ninja
        build directory, that each test target should produce or use at
        runtime.
    """
    result: dict[str, set[str]] = {}

    tests_json_path = build_dir / "tests.json"
    with tests_json_path.open("rt") as f:
        tests_json = json.load(f)

    for entry in tests_json:
        test = entry["test"]
        target_label = test["label"]

        artifacts = set()

        # test.path is used by host tests to point to the main test executable.
        path: str | None = test.get("path")
        if path:
            assert isinstance(path, str)
            artifacts.add(path)

        # test.new_path is used by device tests to point to the host test runner executable.
        # See https://fxbug.dev(458823250)
        new_path: str | None = test.get("new_path")
        if new_path:
            assert isinstance(new_path, str)
            artifacts.add(new_path)

        # test.package_manifests is a list of path to package manifest files. Those are generated
        # at build time, so cannot be read directly, but they should be part of the affected
        # artifacts when the corresponding source file changes.
        package_manifests: list[str] | None = test.get("package_manifests")
        if package_manifests:
            assert isinstance(package_manifests, list)
            artifacts.update(package_manifests)

        # test.package_manifest_deps path points to a JSON file that contains an array
        # of string paths to other package manifests, corresponding to extra packages
        # needed at runtime during testing. Since it is generated at regeneration time,
        # before the build, it is safe to read here.
        package_manifest_deps_path: str | None = test.get(
            "package_manifest_deps"
        )
        if package_manifest_deps_path:
            assert isinstance(package_manifest_deps_path, str)
            with (build_dir / package_manifest_deps_path).open("rt") as f:
                manifests = json.load(f)
                assert isinstance(manifests, list)
                artifacts.update(manifests)
            artifacts.add(package_manifest_deps_path)

        # The test.runtime_deps path points to a JSON file that contains an array of
        # string paths to Ninja artifacts needed at runtime. Since it is generated
        # at regeneration time, before the build, it is safe to read here.
        runtime_deps_path: str | None = test.get("runtime_deps")
        if runtime_deps_path:
            with (build_dir / runtime_deps_path).open("rt") as f:
                runtime_deps = json.load(f)
                assert isinstance(runtime_deps, list)
                artifacts.update(runtime_deps)
            artifacts.add(runtime_deps_path)

        # It is common to get duplicates because runtime_deps and package_manifest_deps
        # are the result of GN metadata collection, which does simple concatenation of
        # lists instead of unions of sets.
        result[target_label] = artifacts

    return result


def find_tests_affected_by_changed_files(
    changed_files: list[str],
    fuchsia_dir: Path,
    ninja_runner: NinjaRunner,
) -> set[str]:
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
        A set of test target labels.
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
    test_artifacts = create_test_artifacts_mapping(build_dir)

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

    affected_artifacts = set(tool_output.splitlines())

    return {
        test_label
        for test_label, artifacts in test_artifacts.items()
        if bool(set(artifacts) & affected_artifacts)
    }

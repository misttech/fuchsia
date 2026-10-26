# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import collections
import dataclasses
import functools
import json
import os
import sys
import tempfile
import typing as T
from pathlib import Path

from build.api import ninja_artifacts
from build.bazel.scripts.build_utils import BazelLauncher, NinjaRunner

# Set this to True to debug operations locally in this script.
_DEBUG = False

_SECONDARY_BUILD_DIR_PREFIX = "build/secondary/"

# Linux caps any single command-line argument at 128 KiB (MAX_ARG_STRLEN,
# hard-coded as PAGE_SIZE * 32) and the whole argv plus environment at
# ARG_MAX (2 MiB under the default 8 MiB stack rlimit).
#
# Both budgets stay well under their cap: ARG_MAX scales with the stack rlimit
# and is shared with the environment, these counts are characters rather than
# bytes, and overshooting means an E2BIG crash with no affected-test signal
# while undershooting only costs extra subprocesses.
_MAX_SINGLE_ARG_CHARS = 64 * 1024
_MAX_AGGREGATE_ARGS_CHARS = 512 * 1024


def _chunk_by_char_limit(
    items: list[str], separator: str, max_chars: int
) -> list[list[str]]:
    """Split items into chunks so that separator.join(chunk) fits in max_chars.

    Args:
        items: The strings to split into chunks.
        separator: The string each chunk will later be joined with. Pass the
            same value used at the join() call site, so the budget accounting
            here cannot drift from the argument that is actually built.
        max_chars: Maximum length of any joined chunk.
    Returns:
        A list of chunks. A single item longer than max_chars gets its own
        chunk, since it cannot be split any further.
    """
    if not items:
        return []
    separator_len = len(separator)
    chunks: list[list[str]] = []
    current_chunk: list[str] = []
    current_len = 0
    for item in items:
        added_len = len(item) + (separator_len if current_chunk else 0)
        if current_chunk and current_len + added_len > max_chars:
            chunks.append(current_chunk)
            current_chunk = [item]
            current_len = len(item)
        else:
            current_chunk.append(item)
            current_len += added_len
    if current_chunk:
        chunks.append(current_chunk)
    return chunks


def debug_log(msg: str) -> None:
    """Log a message to stderr if _DEBUG is True.

    Note that for performance reasons, only call this when _DEBUG is True.
    This avoids un-needed string formatting operations in the usual case where
    the flag is False.
    """
    assert _DEBUG, "Do not call debug_log() directly, check for _DEBUG first!"
    print(msg, file=sys.stderr)


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

    # The full GN label of the package for this test.
    package_label: str = ""


def gn_label_to_build_gn_path(label: str) -> str:
    """Return the relative path to the BUILD.gn file defining a GN label.

    For example:
      //src/foo:bar(//build/toolchain:arm64) -> src/foo/BUILD.gn
      //src/foo/bar:bar                      -> src/foo/bar/BUILD.gn
      //:root_target                         -> BUILD.gn
    """
    if not label or label.startswith("@"):
        return ""
    # Strip toolchain if present: //foo:bar(//build/toolchain:...)
    target = label.partition("(")[0]
    target = target.removeprefix("//")
    # Strip target name after :
    pkg_dir = target.partition(":")[0]
    if pkg_dir:
        return os.path.normpath(os.path.join(pkg_dir, "BUILD.gn"))
    return "BUILD.gn"


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

        package_label = test.get("package_label", "")
        if package_label:
            assert isinstance(package_label, str)

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
                package_label=package_label,
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
        if test.label.startswith("@"):
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
    gn_test_infos, _ = split_gn_and_bazel_tests(parse_tests_json(build_dir))
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


@dataclasses.dataclass(frozen=True)
class AffectedTestsResult:
    """Represents the result of find_tests_affected_by_changed_files."""

    # Set of affected test targets.
    affected_tests: set[AffectedTestTarget]

    # True if no targets in the build graph were affected by the changed files.
    build_not_affected: bool


def _quote_bazel_query_word(word: str) -> str:
    """Quote a target label or path for safe inclusion in a Bazel query expression."""
    escaped = word.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def map_file_path_to_bazel_label(
    file_path: str,
    fuchsia_dir: Path,
    package_for_dir: T.Callable[[str], str],
) -> str:
    """Map a given file path to a Bazel target label.

    For example, if //some/package/BUILD.bazel exists, then an input
    of 'some/package/with/target/in/subdir' will produce a result
    of '@@//some/package:with/target/in/subdir'.

    Args:
        file_path: A file path. If relative, this is assumed to be relative to
            fuchsia_dir.
        fuchsia_dir: The path to the Fuchsia source directory.
        package_for_dir: Maps a directory to the Bazel package enclosing it.
    Returns:
        A Bazel target label looking like @@//<package>:<target>.
    """
    if os.path.isabs(file_path):
        file_path = os.path.relpath(file_path, fuchsia_dir)

    package_path = package_for_dir(os.path.dirname(file_path))
    return f"@@//{package_path}:{os.path.relpath(file_path, package_path)}"


def map_file_paths_to_bazel_labels(
    file_paths: set[str], fuchsia_dir: Path
) -> set[str]:
    """Convert a list of source files, relative to the Fuchsia directory, into a set of Bazel labels.

    Args:
        file_paths: An iterable of source files, relative to the Fuchsia directory.
        fuchsia_dir: The path to the Fuchsia source directory.
    Returns:
        A set of Bazel labels corresponding to the input source files.
    """

    # Package boundaries are determined by the presence of a BUILD.bazel file,
    # so resolving one directory means walking up to the root in the worst case.
    # Memoizing the recursive call caches every directory along the way, not
    # just the ones asked about directly, which matters for changes that touch
    # thousands of files sharing common ancestors.
    @functools.cache
    def package_for_dir(dir_path: str) -> str:
        if not dir_path or dir_path == ".":
            return ""
        if (fuchsia_dir / dir_path / "BUILD.bazel").exists():
            return dir_path
        return package_for_dir(os.path.dirname(dir_path))

    return {
        map_file_path_to_bazel_label(file_path, fuchsia_dir, package_for_dir)
        for file_path in file_paths
    }


def find_bazel_tests_affected_by_changed_files(
    changed_files: set[str],
    bazel_tests: list[TestTargetInfo],
    fuchsia_dir: Path,
    ninja_build_dir: Path,
    bazel_launcher: BazelLauncher,
) -> list[AffectedTestTarget]:
    """Extract the list of Bazel test targets from tests.json

    Args:
        changed_files: A list of changed source files, relative to the Fuchsia source directory.
        bazel_tests: A list of TestTargetInfo for Bazel-defined tests.
        fuchsia_dir: A Path to the Fuchsia source directory.
        ninja_build_dir: Path to Ninja build directory.
        bazel_launcher: A BazelLauncher instance used to run queries.
    Returns:
        A list of AffectedTestTarget values.
    """
    # There are three sets of files to consider:
    #
    # - Regular input sources, which can be passed as inputs to allrdeps().
    #
    # - Bazel BUILD.bazel files, which are ignored as inputs by rdeps/allrdeps,
    #   so //src/foo:BUILD.bazel is substituted with //src/foo:all.
    #
    # - Bazel .bzl files, which are resolved by SkyQuery's
    #   siblings(rbuildfiles(...)) operator scoped to --universe_scope=<all_test_labels>.
    #   rbuildfiles() is transitive over load() edges, so a .bzl file loaded
    #   only by another .bzl file still resolves to the BUILD files using it.
    #
    all_test_labels = {test.label for test in bazel_tests}
    if not all_test_labels:
        return []

    changed_input_labels: set[str] = set()
    changed_bzl_files: set[str] = set()
    for changed_label in map_file_paths_to_bazel_labels(
        changed_files, fuchsia_dir
    ):
        package, _, target = changed_label.partition(":")
        if target.endswith(".bzl"):
            pkg_path = package.removeprefix("@@//")
            changed_bzl_files.add(
                os.path.join(pkg_path, target) if pkg_path else target
            )
        else:
            if target in ("BUILD", "BUILD.bazel"):
                target = "all"
            changed_input_labels.add(f"{package}:{target}")

    if not changed_input_labels and not changed_bzl_files:
        return []

    def run_query(query_args: list[str], query_expr: str) -> list[str]:
        query_args = ["--config=quiet", "--consistent_labels"] + query_args
        if _DEBUG:
            debug_log(f"BAZEL QUERY: {query_args}\n")
        ret = bazel_launcher.run_query("query", query_args, ignore_errors=True)
        # With --keep_going (set by ignore_errors=True), Bazel returns 0 when all
        # targets in the query exist or 3 (PARTIAL_ANALYSIS_FAILURE) when some
        # changed files are not Bazel targets. The latter happens on nearly every
        # change, since most changed files are not Bazel targets at all. Any other
        # exit code (such as 2 for a query syntax/flag error) is fatal and must not
        # be silently treated as zero affected tests.
        if ret.returncode not in (0, 3):
            # ignore_errors=True discards Bazel's stderr, and the temporary
            # --query_file is gone by the time anyone reads this, so embed the
            # query expression itself to make the failure diagnosable.
            raise RuntimeError(
                f"bazel query failed with returncode {ret.returncode}:"
                f" {query_args}\nquery: {query_expr}"
            )
        return ret.stdout.splitlines()

    query_terms: list[str] = []
    if changed_input_labels:
        quoted_inputs = " ".join(
            _quote_bazel_query_word(label)
            for label in sorted(changed_input_labels)
        )
        query_terms.append(f"set({quoted_inputs})")
    if changed_bzl_files:
        if _DEBUG:
            debug_log(
                "CHANGED BZL FILES:\n  {}\n".format(
                    "\n  ".join(sorted(changed_bzl_files))
                )
            )
        bzl_args = ", ".join(
            _quote_bazel_query_word(path) for path in sorted(changed_bzl_files)
        )
        query_terms.append(f"siblings(rbuildfiles({bzl_args}))")

    query_expr = f"allrdeps({' + '.join(query_terms)})"

    affected_test_labels: set[str] = set()
    universe_chunks = _chunk_by_char_limit(
        sorted(all_test_labels),
        separator=",",
        max_chars=_MAX_SINGLE_ARG_CHARS,
    )

    with tempfile.NamedTemporaryFile(
        mode="wt", suffix=".bazel_query"
    ) as query_file:
        query_file.write(query_expr)
        query_file.flush()
        for universe_chunk in universe_chunks:
            reverse_deps = set(
                run_query(
                    [
                        # allrdeps() and rbuildfiles() only exist in Bazel's
                        # SkyQuery environment, which is entered by passing
                        # --universe_scope together with --order_output=no.
                        # Without both, the query fails to parse. Unlike
                        # rdeps(), allrdeps() takes no universe argument of its
                        # own; --universe_scope is where it gets its scope, and
                        # results are limited to that preloaded closure.
                        f"--universe_scope={','.join(universe_chunk)}",
                        "--order_output=no",
                        f"--query_file={query_file.name}",
                    ],
                    query_expr,
                )
            )
            affected_test_labels.update(reverse_deps & all_test_labels)

    if _DEBUG:
        debug_log(
            "All affected Bazel test labels:\n  {}\n".format(
                "\n  ".join(sorted(affected_test_labels))
            )
        )

    label_to_os_names = collections.defaultdict(list)
    for test in bazel_tests:
        label_to_os_names[test.label].append(test.os_name)

    result: list[AffectedTestTarget] = []
    for label in affected_test_labels:
        for os_name in label_to_os_names[label]:
            result.append(AffectedTestTarget(label, os_name))

    return result


def _normalize_build_gn_path(path: str) -> str:
    """Normalize a path to a BUILD.gn file, stripping secondary tree prefixes."""
    normalized = os.path.normpath(path)
    if normalized.startswith(_SECONDARY_BUILD_DIR_PREFIX):
        return normalized[len(_SECONDARY_BUILD_DIR_PREFIX) :]
    return normalized


def _find_gn_tests_affected_by_build_gn_files(
    gn_tests: list[TestTargetInfo],
    changed_sources: set[str],
) -> set[AffectedTestTarget]:
    """Find GN tests whose BUILD.gn file was directly changed.

    In Ninja's graph, BUILD.gn is an input to GN regeneration (build.ninja.stamp),
    not directly to test targets, so ninja -t affected only reports build.ninja.stamp.
    This explicitly associates changed BUILD.gn files with the tests whose target
    or package is defined within them.
    """
    changed_build_gns = {
        _normalize_build_gn_path(source)
        for source in changed_sources
        if os.path.basename(source) == "BUILD.gn"
    }
    if not changed_build_gns:
        return set()

    return {
        AffectedTestTarget(label=test.label, os_name=test.os_name)
        for test in gn_tests
        if (
            gn_label_to_build_gn_path(test.label) in changed_build_gns
            or gn_label_to_build_gn_path(test.package_label)
            in changed_build_gns
        )
    }


def find_tests_affected_by_changed_files(
    changed_files: list[str],
    fuchsia_dir: Path,
    ninja_runner: NinjaRunner,
    bazel_launcher: BazelLauncher,
) -> AffectedTestsResult:
    """Return the set of test labels and build affected status for changed files.

    Given a set of paths to changed files (for example after applying a
    git commit just after the last build), determine which targets need to
    be rebuilt (and for tests re-run), return the set of tests labels that
    would need to be rebuilt and then re-run after the build, as well as whether
    any build graph targets were affected.

    Args:
        changed_files: List of file path strings, relative to Fuchsia source directory,
            of files that were changed since the last build.
        fuchsia_dir: Path to Fuchsia source directory.
        ninja_runner: A NinjaRunner instance.
        bazel_launcher: A BazelLauncher instance.
    Returns:
        An AffectedTestsResult containing the set of affected tests and whether
        the build was unaffected.
    """

    if _DEBUG:
        debug_log(f"changed_files={changed_files}")

    changed_sources: set[str] = set()
    for file in changed_files:
        if os.path.isabs(file):
            changed_sources.add(os.path.relpath(file, fuchsia_dir))
        else:
            changed_sources.add(str(file))

    build_dir = ninja_runner.build_dir

    gn_tests, bazel_tests = split_gn_and_bazel_tests(
        parse_tests_json(build_dir)
    )

    if _DEBUG:
        debug_log(
            "GN_TESTS: {}\n  ".format(
                "\n  ".join(test.label for test in gn_tests)
            ),
        )
        debug_log(
            "BAZEL_TESTS: {}\n  ".format(
                "\n  ".join(test.label for test in bazel_tests)
            ),
        )

    affected_ninja_artifacts: set[str] = set()
    if changed_sources:
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
        for source_chunk in _chunk_by_char_limit(
            ninja_sources,
            separator=" ",
            max_chars=_MAX_AGGREGATE_ARGS_CHARS,
        ):
            tool_output = ninja_runner.run_and_extract_output(
                [
                    "-t",
                    "affected",
                    "--depfile",
                    "--ignore-errors",
                ]
                + source_chunk
            )
            affected_ninja_artifacts.update(tool_output.splitlines())

    ninja_results: set[AffectedTestTarget] = set()

    if gn_tests:
        ninja_results.update(
            _find_gn_tests_affected_by_build_gn_files(gn_tests, changed_sources)
        )

        # Read the content of tests.json to determine which important artifacts
        # each test requires at runtime.
        gn_test_artifacts = _create_gn_test_artifacts_mapping(
            gn_tests, build_dir
        )

        ninja_results.update(
            {
                AffectedTestTarget(label=test_label, os_name=test_info.os_name)
                for test_label, test_info in gn_test_artifacts.items()
                if bool(test_info.ninja_artifacts & affected_ninja_artifacts)
            }
        )

    bazel_results: set[AffectedTestTarget] = set()

    if bazel_tests:
        bazel_results = set(
            find_bazel_tests_affected_by_changed_files(
                changed_sources,
                bazel_tests,
                fuchsia_dir,
                build_dir,
                bazel_launcher,
            )
        )

    affected_build_artifacts: set[str] = set()
    if affected_ninja_artifacts:
        last_build_artifacts = set(
            ninja_artifacts.get_last_build_artifacts(ninja_runner)
        )
        # build.ninja.stamp is generated whenever any BUILD.gn or .gni file
        # that is part of this builder's build plan is modified. If it is in
        # affected_ninja_artifacts, the build graph for this builder was changed.
        last_build_artifacts.add("build.ninja.stamp")
        affected_build_artifacts = (
            affected_ninja_artifacts & last_build_artifacts
        )
    build_not_affected = not changed_sources or (
        len(affected_build_artifacts) == 0
        and len(ninja_results) == 0
        and len(bazel_results) == 0
    )

    return AffectedTestsResult(
        affected_tests=ninja_results | bazel_results,
        build_not_affected=build_not_affected,
    )

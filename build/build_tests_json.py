#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Generate tests.json.
"""

import dataclasses
import json
import pprint
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).parent / "bazel/scripts"))
sys.path.insert(0, str(Path(__file__).parent / "python/modules"))
import bazel_tests_utils
import build_utils
from build_utils import CommandRunner
from serialization import JSONValue, instance_from_dict


@dataclass(frozen=True)
class Dimensions:
    """Swarming dimensions for a test environment or platform."""

    access_points: str | None = None
    attenuators: str | None = None
    cpu: str | None = None
    device_type: str | None = None
    dimensions: JSONValue | None = None
    host_device_type: str | None = None
    iperf_servers: str | None = None
    os: str | None = None
    pool: str | None = None
    sherlocks: str | None = None
    sorrels: str | None = None
    tags: JSONValue | None = None
    testbed: str | None = None
    vim3s: str | None = None

    def __post_init__(self) -> None:
        if self.tags is not None:
            raise ValueError(
                "tags are only valid in an environments scope, not in dimensions"
            )
        if self.dimensions is not None:
            raise ValueError(
                "found nested dimensions environment field. Did you set "
                + "`dimensions = some_env`? It should be `dimensions = some_env.dimensions`"
            )

    def get(self, key: str, default: str | None = None) -> str | None:
        val: str | None = getattr(self, key, None)
        return val if val is not None else default

    def is_subset_of(self, other: "Dimensions") -> bool:
        """Return True if all non-None dimensions in `self` are present with the same values in `other`.

        Because `dict.items()` returns a set-like `dict_items` view of `(key,
        value)` pairs, the `<=` operator performs a set subset comparison,
        verifying that every dimension requirement in `self` is satisfied by
        `other` (which may also specify additional dimensions).
        """
        return self.to_dict().items() <= other.to_dict().items()

    def to_dict(self) -> dict[str, str]:
        """Return only the explicitly specified (non-None) dimensions.

        This is called once per test/environment/platform combination, so it
        deliberately avoids `instance_to_dict()`, whose per-call type-hint
        reflection is far too slow for this hot path.
        """
        return {k: v for k, v in vars(self).items() if v is not None}


@dataclass(frozen=True)
class EmulatorConfig:
    """Emulator-specific configuration for a test environment."""

    accel: str | None = None
    device: str | None = None
    kernel_args: tuple[str, ...] | None = None
    name: str = ""
    uefi: bool | None = None
    vbmeta_key: str | None = None
    vbmeta_key_metadata: str | None = None

    def __post_init__(self) -> None:
        if not self.name:
            raise ValueError("The `emulator` scope requires a unique `name`")
        if self.uefi and (not self.vbmeta_key or not self.vbmeta_key_metadata):
            raise ValueError(
                "Emulator environments with `uefi` set to true must provide "
                + "a `vbmeta_key` and `vbmeta_key_metadata`"
            )
        if isinstance(self.kernel_args, list):
            object.__setattr__(self, "kernel_args", tuple(self.kernel_args))

    def to_dict(self) -> dict[str, Any]:
        """Return only the explicitly specified (non-None) fields."""
        return {
            k: list(v) if isinstance(v, tuple) else v
            for k, v in vars(self).items()
            if v is not None
        }


@dataclass(frozen=True)
class Environment:
    """Full device environment specification in which a test should run."""

    dimensions: Dimensions = Dimensions()
    emulator: EmulatorConfig | None = None
    netboot: bool | None = None
    service_account: str | None = None
    tags: tuple[str, ...] | None = None

    def __post_init__(self) -> None:
        if not self.dimensions.to_dict():
            raise ValueError("each environment must specify dimensions")
        if isinstance(self.tags, list):
            object.__setattr__(self, "tags", tuple(self.tags))

    def __lt__(self, other: "Environment") -> bool:
        return json.dumps(self.to_dict(), sort_keys=True) < json.dumps(
            other.to_dict(), sort_keys=True
        )

    def to_dict(self) -> dict[str, Any]:
        """Return only the explicitly specified (non-None) fields.

        Like `Dimensions.to_dict()`, this avoids `instance_to_dict()` because it
        is called for every resolved test environment and in `__lt__()`.
        """
        return {
            k: (
                v.to_dict()
                if isinstance(v, (Dimensions, EmulatorConfig))
                else list(v)
                if isinstance(v, tuple)
                else v
            )
            for k, v in vars(self).items()
            if v is not None
        }


@dataclass
class TestEntries:
    """For each test (see `TestEntryKey`), the build-only and runnable entries, for deduplication."""

    build_only_entry: dict[str, Any] | None = None
    runnable_entries: list[dict[str, Any]] = dataclasses.field(
        default_factory=list
    )

    def update_with(self, test: dict[str, Any]) -> None:
        if test.get("build_only", False):
            if not self.build_only_entry:
                self.build_only_entry = test
        else:
            # If the same exact test is added via multiple paths, only
            # include it once.  (This is likely to happen with host-only
            # tests.)
            if test not in self.runnable_entries:
                self.runnable_entries.append(test)


@dataclass(frozen=True)
class TestEntryKey:
    """The key that identifies which test a test entry is for: its name and cpu.

    These are the same whether the test is found via the GN metadata walk,
    Bazel, or a product_bundle_test_group.

    The name alone is not enough: when the host and target CPUs differ,
    `boot_test()` defines a test for each host CPU that it can be run from.
    These all share the same name (and label), but each has its own `cpu` (and
    with it, its own runner script `path` and `runtime_deps`), and each needs to
    be built (and possibly run).
    """

    name: str
    cpu: str

    @classmethod
    def from_test(cls, test: dict[str, Any]) -> "TestEntryKey":
        """Return the key for a test entry.

        Runnable tests in a product_bundle_test_group have the product bundle's
        name appended to their name, so this must be called before that's done.
        """
        test_info = test["test"]
        return cls(name=test_info["name"], cpu=test_info["cpu"])


def partition_platforms(
    platforms: list[dict[str, Any]], target_cpu: str
) -> tuple[set[Dimensions], set[Dimensions]]:
    """Partition platform definitions into target_cpu platforms and other platforms."""
    parsed_platforms = [instance_from_dict(Dimensions, p) for p in platforms]
    target_cpu_device_types = {
        p.device_type
        for p in parsed_platforms
        if p.cpu == target_cpu and p.device_type is not None
    }

    target_platforms: set[Dimensions] = set()
    other_platforms: set[Dimensions] = set()
    for p in parsed_platforms:
        if p.cpu == target_cpu or (
            p.cpu is None
            and (
                p.device_type is None
                or p.device_type in target_cpu_device_types
            )
        ):
            target_platforms.add(p)
        else:
            other_platforms.add(p)
    return target_platforms, other_platforms


def validate_known_platform(
    env: Environment,
    target_platforms: set[Dimensions],
    other_platforms: set[Dimensions],
) -> Environment:
    """Validate that env matches at least one known platform (target or other)."""
    if not any(
        env.dimensions.is_subset_of(p)
        for p in (target_platforms | other_platforms)
    ):
        raise ValueError(
            f"Could not match environment specifications: {env.to_dict()}\n"
            + "Consult //build/testing/platforms.gni for all allowable specifications"
        )
    return env


def matches_target_platform(
    env: Environment,
    target_platforms: set[Dimensions],
    other_platforms: set[Dimensions],
) -> bool:
    """Validate that env matches a known platform, and return True if it matches target_platforms."""
    if any(env.dimensions.is_subset_of(p) for p in target_platforms):
        return True
    if any(env.dimensions.is_subset_of(p) for p in other_platforms):
        return False
    raise ValueError(
        f"Could not match environment specifications: {env.to_dict()}\n"
        + "Consult //build/testing/platforms.gni for all allowable specifications"
    )


def is_pure_host_test(test: dict[str, Any]) -> bool:
    """Return True if the test spec describes a pure Linux host test (no target device)."""
    test_info = test.get("test", {})
    return test_info.get("os") == "linux" and not (
        test.get("expects_ssh", True) or test.get("is_boot_test", False)
    )


def test_is_build_only(test: dict[str, Any]) -> bool:
    """Return True if the test spec describes a build-only test."""
    return test.get("build_only", False)


def set_environments_for_test(
    test: dict[str, Any], environments: list[Environment]
) -> None:
    """Deduplicate, sort, and write resolved environments to `test` (or mark it build_only)."""
    # Use a set to deduplicate environments, then sort for a stable output order.
    sorted_envs = sorted(set(environments))
    if not sorted_envs:
        test["build_only"] = True
        test["environments"] = []
    else:
        test["environments"] = [e.to_dict() for e in sorted_envs]
        if "build_only" in test:
            test["build_only"] = False


def resolve_host_test_environments(
    test: dict[str, Any],
    host_env: Environment,
    target_platforms: set[Dimensions],
    other_platforms: set[Dimensions],
    build_only: bool = False,
) -> None:
    """Resolve environments for a pure host test against host_env and target_platforms."""

    # If the test is build_only, or all tests being processed are to be build-only,
    # set the test as build-only, with no environments, and return.
    if build_only or test.get("build_only") is True:
        test["build_only"] = True
        set_environments_for_test(test, [])
        return

    raw_test_envs = test.get("environments")
    if raw_test_envs:
        candidate_envs = [
            instance_from_dict(Environment, e) for e in raw_test_envs
        ]
    else:
        test_cpu = test.get("test", {}).get("cpu")
        if test_cpu and test_cpu != host_env.dimensions.cpu:
            candidate_envs = [
                Environment(dimensions=Dimensions(os="Linux", cpu=test_cpu))
            ]
        else:
            candidate_envs = [host_env]

    matched_envs = [
        env
        for env in candidate_envs
        if matches_target_platform(env, target_platforms, other_platforms)
    ]
    set_environments_for_test(test, matched_envs)


def resolve_general_test_environments(
    test: dict[str, Any],
    default_envs: list[Environment],
    allowed_device_types: set[str],
    allowed_host_device_types: set[str],
    target_platforms: set[Dimensions],
    other_platforms: set[Dimensions],
) -> None:
    """Resolve, validate, and filter environments for a general (non-product-bundle) test spec."""

    # Don't run purely host tests through this function
    assert not is_pure_host_test(test)

    # Mark build_only tests as having no environments.
    if test_is_build_only(test):
        set_environments_for_test(test, [])
        return

    raw_test_envs = test.get("environments")
    if raw_test_envs:
        candidate_envs = [
            instance_from_dict(Environment, e) for e in raw_test_envs
        ]
    else:
        candidate_envs = default_envs

    matched_envs = [
        env
        for env in candidate_envs
        if matches_target_platform(env, target_platforms, other_platforms)
        and (
            env.dimensions.device_type is None
            or env.dimensions.device_type in allowed_device_types
        )
        and (
            env.dimensions.host_device_type is None
            or env.dimensions.host_device_type in allowed_host_device_types
        )
    ]
    set_environments_for_test(test, matched_envs)


def override_product_bundle_test_environments(
    test: dict[str, Any],
    group_envs: list[Environment],
) -> None:
    """Resolve environments for a product_bundle_test_group that specifies mandatory `environments`."""

    # Don't run purely host tests through this function
    assert not is_pure_host_test(test)

    if test_is_build_only(test):
        # If a test is marked build-only, let it stay that way.
        set_environments_for_test(test, [])
    else:
        # Otherwise, forcibly set the environments for the test to the group's environments.
        set_environments_for_test(test, group_envs)


def resolve_product_bundle_test_environments(
    test: dict[str, Any],
    allowed_envs: list[Environment],
    default_envs: list[Environment],
    target_platforms: set[Dimensions],
    other_platforms: set[Dimensions],
) -> None:
    """Resolve environments for a product_bundle_test_group using allowed and default environments."""

    # Don't run purely host tests through this function
    assert not is_pure_host_test(test)

    # Mark build_only tests as having no environments, or mark all tests as build_only if the
    # group has no allowed environments.
    if test_is_build_only(test) or not allowed_envs:
        set_environments_for_test(test, [])
        return

    # If the test does not specify any environments, use the group's default environments.
    raw_test_envs = test.get("environments")
    if not raw_test_envs:
        set_environments_for_test(test, default_envs)
        return

    # The environments specified by a test are a set of dimension requirements
    # for a match, rather than an exact match on all Environment fields (such as
    # tags or emulator configurations). A test environment matches if its
    # dimensions are a subset of any of the group's allowed environments' dimensions.
    matched_envs: list[Environment] = []
    for e in raw_test_envs:
        env = instance_from_dict(Environment, e)
        validate_known_platform(env, target_platforms, other_platforms)
        if any(
            env.dimensions.is_subset_of(allowed_env.dimensions)
            for allowed_env in allowed_envs
        ):
            matched_envs.append(env)
    set_environments_for_test(test, matched_envs)


def build_tests_json(
    build_dir: Path,
    with_bazel_tests: bool = False,
    command_runner: CommandRunner | None = None,
    quiet: bool = True,
) -> set[Path]:
    """Generate the tests.json file.

    tests.json is created by merging two things:

    1) tests_from_metadata.json
       A collection of test specs found from a GN metadata walk. These test
       specs have their default environments populated, validated against
       platforms, and filtered by allowed device types before being written
       into the final tests.json.

    2) product_bundle_test_groups.json
       A file that declares a mapping of product bundle name to a specific set
       of tests found in another tests.json. These test specs will be modified
       to include `product_bundle: <name>` and filtered/defaulted against the
       group's environments before merging into the final tests.json.

    Args:
        build_dir: Fuchsia build directory.
        with_bazel_tests: Whether to export Bazel tests.
        command_runner: Optional command runner to use for running bazel commands.
        quiet: Whether to print status updates.

    Returns:
        A set of Path values for the input files read by this function.
    """
    tests_json_path = build_dir / "tests.json"

    # Read the list of tests that were collected from a GN metadata walk.
    tests_from_metadata_path = build_dir / "tests_from_metadata.json"
    tests = json.loads(tests_from_metadata_path.read_text())

    # Read the mapping between test sets and product bundle name.
    test_groups_path = (
        build_dir / "obj" / "tests" / "product_bundle_test_groups.json"
    )
    test_groups = json.loads(test_groups_path.read_text())

    # Read the environment constants and platform definitions.
    environments_path = (
        build_dir / "obj" / "tests" / "all_test_environments.json"
    )
    all_test_environments = json.loads(environments_path.read_text())

    # Read the builder-set default environments and allowed device types.
    default_environments_path = (
        build_dir / "obj" / "tests" / "default_test_environments.json"
    )
    default_test_environments = json.loads(
        default_environments_path.read_text()
    )

    # Read the list of product bundles that were collected from a GN metadata
    # walk.
    product_bundles_json_path = build_dir / "product_bundles.json"
    product_bundles = json.loads(product_bundles_json_path.read_text())
    product_bundle_names = [pb["name"] for pb in product_bundles]

    target_cpu: str = default_test_environments["target_cpu"]
    default_envs: list[Environment] = [
        instance_from_dict(Environment, e)
        for e in default_test_environments["default_environments"]
    ]
    allowed_device_types_set: set[str] = set(
        default_test_environments["allowed_device_types"]
    )
    allowed_host_device_types_set: set[str] = set(
        default_test_environments["allowed_host_device_types"]
    )
    host_env: Environment = instance_from_dict(
        Environment, all_test_environments["host_env"]
    )

    target_platforms, other_platforms = partition_platforms(
        all_test_environments["platforms"], target_cpu
    )

    validation_errors: list[str] = []

    # Placeholder for tests from Bazel, needed for type checking clarity.
    bazel_tests = bazel_tests_utils.BazelTestsJson(
        tests=[], grouped_tests={}, inputs=set()
    )

    if with_bazel_tests:
        bazel_paths = build_utils.BazelPaths.new(build_dir=build_dir)
        # Collect the files listing Bazel device test suites for each
        # product_bundle_test_group so `generate_tests_json` can query them in a
        # single pass and return their test specs in `bazel_tests.grouped_tests`
        # keyed by suite file path.
        group_suite_files = [
            build_dir / test_group["bazel_target_test_suites"]
            for test_group in test_groups
            if "bazel_target_test_suites" in test_group
        ]
        bazel_tests = bazel_tests_utils.generate_tests_json(
            bazel_paths,
            command_runner,
            quiet=quiet,
            extra_device_suite_files=group_suite_files,
        )

        # Add the non-product-bundle-specific tests to the overall set
        # of non-product-bundle-specific tests.  The grouped tests are
        # added later.
        tests.extend(bazel_tests.tests)

    else:
        # `//build/images/updates:all_package_manifests.list` unconditionally
        # reads `bazel_test_packages.list`, and its GN action is `no_op.sh`
        # (`touch`), which would otherwise create an empty non-JSON file.
        bazel_tests_utils.write_bazel_test_packages_list(build_dir, [])

    # Test entries for deduplication.
    #
    # This is a map of tests (by name and cpu, see `TestEntryKey`), to a list
    # of runnable test specs for the test.  If there are no runnable test specs
    # (the list is empty), then it's a build-only test.
    test_entries_by_key: dict[TestEntryKey, TestEntries] = {}

    # Resolve, validate, and filter environments for tests from metadata and
    # Bazel.
    for test in tests:
        test_info = test.get("test", {})
        test_name = test_info.get("name", "<unknown>")
        test_label = test_info.get("label")
        test_id = f"{test_name} ({test_label})" if test_label else test_name

        try:
            if is_pure_host_test(test):
                resolve_host_test_environments(
                    test,
                    host_env,
                    target_platforms,
                    other_platforms,
                )
            else:
                resolve_general_test_environments(
                    test,
                    default_envs=default_envs,
                    allowed_device_types=allowed_device_types_set,
                    allowed_host_device_types=allowed_host_device_types_set,
                    target_platforms=target_platforms,
                    other_platforms=other_platforms,
                )
        except ValueError as err:
            validation_errors.append(f"{test_id}: {err}")

        # Add the test to the set of test entries for deduplication.
        test_entries_by_key.setdefault(
            TestEntryKey.from_test(test), TestEntries()
        ).update_with(test)

    # For every group of tests that are supposed to target a specific product
    # bundle, we parse the tests, add `product_bundle: <name>` and add the test
    # to `tests`. When infra reads the final tests.json file, it will read that
    # field and know to flash the product bundle with <name> before running the
    # test.
    #
    # We also assert that the product bundle name is found in
    # product_bundles.json.
    for test_group in test_groups:
        product_bundle_name = test_group["product_bundle_name"]
        if product_bundle_name not in product_bundle_names:
            print(
                f"ERROR: {product_bundle_name} is not a valid product_bundle_name."
            )
            print("Available names are:")
            pprint.pp(product_bundle_names)
            sys.exit(1)

        group_build_only = test_group.get("build_only", False)
        raw_group_envs: list[dict[str, JSONValue]] = (
            [] if group_build_only else test_group.get("environments", [])
        )
        raw_allowed_envs: list[dict[str, JSONValue]] = (
            []
            if group_build_only
            else test_group.get("allowed_environments", [])
        )
        raw_default_envs: list[dict[str, JSONValue]] = (
            []
            if group_build_only
            else test_group.get("default_environments", raw_allowed_envs)
        )

        try:
            group_environments: list[Environment] = [
                validate_known_platform(
                    instance_from_dict(Environment, e),
                    target_platforms,
                    other_platforms,
                )
                for e in raw_group_envs
            ]
            group_allowed_environments: list[Environment] = [
                validate_known_platform(
                    instance_from_dict(Environment, e),
                    target_platforms,
                    other_platforms,
                )
                for e in raw_allowed_envs
            ]
            group_default_environments: list[Environment] = [
                validate_known_platform(
                    instance_from_dict(Environment, e),
                    target_platforms,
                    other_platforms,
                )
                for e in raw_default_envs
            ]

        except ValueError as err:
            validation_errors.append(
                f"product_bundle_test_group:{product_bundle_name}: {err}"
            )
            continue

        # Read the tests.json that is assigned to this specific product bundle.
        product_bundle_tests_file = build_dir / test_group["tests_json"]
        product_bundle_tests = json.loads(product_bundle_tests_file.read_text())

        # Add any tests from Bazel that were found in this test group.
        if (
            bazel_tests.grouped_tests
            and "bazel_target_test_suites" in test_group
        ):
            suite_file = build_dir / test_group["bazel_target_test_suites"]
            product_bundle_tests.extend(bazel_tests.grouped_tests[suite_file])

        # Update the test spec to include the product bundle target and
        # environments.
        for test in product_bundle_tests:
            test_info = test.get("test", {})
            original_name = test_info["name"]
            name = original_name + "-" + product_bundle_name
            test_label = test_info.get("label")
            test_id = f"{name} ({test_label})" if test_label else name

            # Get the key before the test's name is changed below.
            test_entry_key = TestEntryKey.from_test(test)

            try:
                if is_pure_host_test(test):
                    resolve_host_test_environments(
                        test,
                        host_env,
                        target_platforms,
                        other_platforms,
                        group_build_only,
                    )
                else:
                    if group_environments:
                        override_product_bundle_test_environments(
                            test,
                            group_environments,
                        )
                    else:
                        resolve_product_bundle_test_environments(
                            test,
                            group_allowed_environments,
                            group_default_environments,
                            target_platforms,
                            other_platforms,
                        )

                    # If it's not a build-only test, mark the runnable test as
                    # targeting this specific product bundle.
                    if not test_is_build_only(test):
                        test_info["name"] = name
                        test["product_bundle"] = product_bundle_name

                test_entries_by_key.setdefault(
                    test_entry_key,
                    TestEntries(),
                ).update_with(test)

            except ValueError as err:
                validation_errors.append(f"{test_id}: {err}")

    if validation_errors:
        raise ValueError(
            "Invalid test environment specifications found:\n"
            + "\n".join(f"  - {err}" for err in validation_errors)
        )

    # For each test in test_entries, we add the runnable test entries, and if it only
    # has build_only entries remaining, we add that as a placeholder.  If it has neither,
    # we raise an error, because that shouldn't happen.
    all_tests: list[dict[str, Any]] = []
    for test_entries in test_entries_by_key.values():
        if test_entries.runnable_entries:
            all_tests.extend(test_entries.runnable_entries)
        elif test_entries.build_only_entry:
            all_tests.append(test_entries.build_only_entry)
        else:
            raise ValueError(
                "Test has no runnable entries and no build-only entry."
            )

    # Write the final list of tests to tests.json if the contents changed.
    contents_changed = True
    if tests_json_path.exists():
        previous_tests = json.loads(tests_json_path.read_text())
        if previous_tests == all_tests:
            contents_changed = False
    if contents_changed:
        tests_json_path.write_text(json.dumps(all_tests, indent=2))

    return {
        tests_from_metadata_path,
        test_groups_path,
        environments_path,
        default_environments_path,
    } | bazel_tests.inputs

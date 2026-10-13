# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import ast
import dataclasses
import json
from pathlib import Path
from typing import Any


# LINT.IfChange(DefaultBuildFlagsSet)
@dataclasses.dataclass(frozen=True)
class DefaultBuildFlagsSet:
    """Models a set of default build_flags() labels to be applied to different target types.

    cxx_common_build_flags: A list of build_flags() labels whose flags will be used by
       actions of all C/C++ target types.

    cxx_executable_build_flags: A list of extra build_flags() labels, used for actions of
        C/C++ executable targets only.

    cxx_shared_library_build_flags: A list of extra build_flags() labels, used for actions
        of C/C++ shared library targets only.

    rust_common_build_flags: A list of build_flags() labels whose flags will be used by
       actions of all Rust target types.

    rust_executable_build_flags: A list of extra build_flags() labels, used for actions of
        Rust executable targets only.

    rust_shared_library_build_flags: A list of extra build_flags() labels, used for actions
        of Rust shared library targets only.

    target_compatible_with: A string for a Bazel expression listing Bazel constraint labels
        to restrict these definitions to artifacts built in specific build configurations.
    """

    cxx_common_build_flags: list[str] = dataclasses.field(default_factory=list)
    cxx_executable_build_flags: list[str] = dataclasses.field(
        default_factory=list
    )
    cxx_shared_library_build_flags: list[str] = dataclasses.field(
        default_factory=list
    )
    rust_common_build_flags: list[str] = dataclasses.field(default_factory=list)
    rust_executable_build_flags: list[str] = dataclasses.field(
        default_factory=list
    )
    rust_shared_library_build_flags: list[str] = dataclasses.field(
        default_factory=list
    )
    target_compatible_with: str = "[]"


# LINT.ThenChange(//build/bazel_sdk/fuchsia_rules_common/build_flags/providers.bzl:DefaultBuildFlagsSetInfo)


def get_build_flags_targets_from_ast(
    content: str, build_file: Path
) -> set[str]:
    """Use Python parser to get build_flags() target names.

    Recognizes:
      - Direct calls to `build_flags(...)` or any wrapper macro ending with
        `_build_flags(...)` (e.g., `compiler_build_flags`,
        `riscv64_abi_build_flags`) with a literal string `name = "..."`.
      - List comprehensions invoking such macros over constant string
        lists/tuples (e.g., `[compiler_build_flags(name = v, ...) for v in
        ["v9", "v9.1"]]`).

    Args:
        content: BUILD.bazel content as string.
        build_file: Path to BUILD.bazel file.
    Returns:
        A set of target name strings.
    Raises:
        Any exception encountered by the Python parser when
        processing the input.
    """
    targets: set[str] = set()
    tree = ast.parse(content, filename=str(build_file))

    def _collect_call_targets(
        call_node: ast.Call, loop_vars: dict[str, list[str]]
    ) -> None:
        if not isinstance(call_node.func, ast.Name):
            return
        func_name = call_node.func.id
        if func_name != "build_flags" and not func_name.endswith(
            "_build_flags"
        ):
            return
        for kw in call_node.keywords:
            if kw.arg == "name":
                if isinstance(kw.value, ast.Constant) and isinstance(
                    kw.value.value, str
                ):
                    targets.add(kw.value.value)
                elif (
                    isinstance(kw.value, ast.Name) and kw.value.id in loop_vars
                ):
                    targets.update(loop_vars[kw.value.id])

    for node in ast.walk(tree):
        if isinstance(node, ast.ListComp) and isinstance(node.elt, ast.Call):
            loop_vars: dict[str, list[str]] = {}
            for gen in node.generators:
                if isinstance(gen.target, ast.Name) and isinstance(
                    gen.iter, (ast.List, ast.Tuple)
                ):
                    loop_vars[gen.target.id] = [
                        e.value
                        for e in gen.iter.elts
                        if isinstance(e, ast.Constant)
                        and isinstance(e.value, str)
                    ]
            _collect_call_targets(node.elt, loop_vars)
        elif isinstance(node, ast.Call):
            _collect_call_targets(node, {})
    return targets


class BuildFileTargetsCache:
    """Find and cache the build_flags() target definitions in BUILD.bazel files."""

    def __init__(self) -> None:
        """Create instance."""
        self._cache: dict[Path, set[str]] = {}

    def get(self, build_file: Path) -> set[str]:
        """Return the set of build_flags() target names from a given BUILD.bazel path."""
        cached = self._cache.get(build_file)
        if cached is not None:
            return cached

        assert build_file.exists(), f"Missing build file: {build_file}"
        content = build_file.read_text()
        targets = get_build_flags_targets_from_ast(content, build_file)

        self._cache[build_file] = targets
        return targets


# A global cache mapping paths to BUILD.bazel files to the
# corresponding set of build_flags() target definitions.
_BUILD_FILE_TARGETS_CACHE = BuildFileTargetsCache()


def _check_build_flags_target_exists(fuchsia_dir: Path, label: str) -> bool:
    """Check if a build_flags() target corresponding to the given GN label exists in the Bazel workspace.

    The implementation parses the BUILD.bazel as a Python AST. This is fast but
    fragile, in particular it requires that name values for build_flags() targets
    are string literals, and not computed (e.g. `name = something + ".build_flags"`)
    as these definitions will be ignored.

    A better way would be to process the result of `bazel query --output=build`
    but running these is significantly slower and requires a proper Bazel
    workspace setup, which is not always guaranteed when running this code.

    Args:
        fuchsia_dir: Path to Fuchsia source directory.
        label: A label string starting with //, such as //<package>:<target_name>,
            or just //<package> (in which case <target_name> is deduced from its basename).
    Returns:
        True if //<package>/BUILD.bazel exists and defines a `build_flags()` target
        with the name <target_name> in it.
    """
    if not label.startswith("//"):
        return False

    label_path = label[2:]
    if ":" in label_path:
        package_dir, target_name = label_path.split(":", 1)
    else:
        package_dir = label_path
        target_name = label_path.split("/")[-1]

    build_file = fuchsia_dir / package_dir / "BUILD.bazel"
    if not build_file.exists():
        return False

    build_file_targets = _BUILD_FILE_TARGETS_CACHE.get(build_file)
    return target_name in build_file_targets


class DefaultBuildFlagsMap(dict[str, DefaultBuildFlagsSet]):
    """A { name -> DefaultBuildFlagsSet } map."""

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        self.missing_configs: list[str] = []

    @staticmethod
    def new_from_gn_config(
        build_dir: Path, fuchsia_dir: Path
    ) -> "DefaultBuildFlagsMap":
        """Return a new instance matching the current GN build configuration.

        Args:
            build_dir: Ninja build directory.
            fuchsia_dir: Fuchsia source directory.
        Returns:
            A new DefaultBuildFlagsMap instance.
        """
        missing_configs: set[str] = set()

        def resolve_and_filter_configs(configs: list[str]) -> list[str]:
            filtered = []
            for config in configs:
                if _check_build_flags_target_exists(fuchsia_dir, config):
                    filtered.append(config)
                else:
                    missing_configs.add(config)
            return filtered

        result_map = DefaultBuildFlagsMap()

        # The current default only distinguish between Fuchsia and Linux os values,
        # but it may be possible to provide different definitions based on additional
        # constraints, such as CPU architecture, API level, or the platform/SDK split.
        platforms_config = {
            "fuchsia": {
                "json_file": build_dir / "bazel_default_configs/fuchsia.json",
                # NOTE: The same default build flags will be used for both
                # fuchsia_platform and fuchsia_sdk artifacts produced in-tree.
                "compatible_with": '["@platforms//os:fuchsia"]',
            },
            "host": {
                "json_file": build_dir / "bazel_default_configs/host.json",
                "compatible_with": "HOST_OS_CONSTRAINTS",
            },
        }

        for name, config_info in platforms_config.items():
            json_file = config_info["json_file"]
            assert isinstance(json_file, Path)

            target_compatible_with = config_info["compatible_with"]
            assert isinstance(target_compatible_with, str)

            assert json_file.exists(), f"Missing input file: {json_file}"
            with json_file.open("rb") as f:
                data = json.load(f)

            # LINT.IfChange(DefaultConfigsJsonSchema)
            result_map[name] = DefaultBuildFlagsSet(
                cxx_common_build_flags=resolve_and_filter_configs(
                    data.get("cxx_common", [])
                ),
                cxx_executable_build_flags=resolve_and_filter_configs(
                    data.get("cxx_executable_extra", [])
                ),
                cxx_shared_library_build_flags=resolve_and_filter_configs(
                    data.get("cxx_shared_library_extra", [])
                ),
                rust_common_build_flags=resolve_and_filter_configs(
                    data.get("rust_common", [])
                ),
                rust_executable_build_flags=resolve_and_filter_configs(
                    data.get("rust_executable_extra", [])
                ),
                rust_shared_library_build_flags=resolve_and_filter_configs(
                    data.get("rust_shared_library_extra", [])
                ),
                target_compatible_with=target_compatible_with,
            )
            # LINT.ThenChange(//build/bazel/config/BUILD.gn:DefaultConfigsJsonSchema)

        result_map.missing_configs = sorted(list(missing_configs))
        return result_map

    def generate_bazel_toolchain_definitions(self) -> str:
        """Generate a BUILD.bazel fragment that defines Bazel toolchain() targets.

        These toolchains carry the default build_flags() labels describing
        the default compiler and linker flags to be used when building different
        types of C++ and Rust artifacts.

        The top-level MODULE.bazel file should call register_toolchains()
        with a filegroup() label pointing to them (or use the magic ":all" target
        name which picks up all toolchain() targets from a package).

        Returns:
            A BUILD.bazel text fragment.
        """
        content = """# Default build_flags() definition for C++ and Rust toolchains.

load("@fuchsia_rules_common//build_flags:toolchain.bzl", "build_flags_toolchain_instance")
load("@@//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
"""

        def format_labels(labels: list[str]) -> str:
            if not labels:
                return "[]"
            # Since this BUILD.bazel file is generated inside an external
            # repository (@fuchsia_build_info), prepend "@@" to labels starting
            # with "//" so Bazel resolves them relative to the main workspace root.
            lines = ["["]
            for label in labels:
                resolved = f"@@{label}" if label.startswith("//") else label
                lines.append(f'        "{resolved}",')
            lines.append("    ]")
            return "\n".join(lines)

        for name, info in self.items():
            content += """
build_flags_toolchain_instance(
    name = "{name}_default_build_flags",
    cxx_common_build_flags = {cxx_common_build_flags},
    cxx_executable_build_flags = {cxx_executable_build_flags},
    cxx_shared_library_build_flags = {cxx_shared_library_build_flags},
    rust_common_build_flags = {rust_common_build_flags},
    rust_executable_build_flags = {rust_executable_build_flags},
    rust_shared_library_build_flags = {rust_shared_library_build_flags},
)

toolchain(
    name = "{name}_toolchain",
    target_compatible_with = {target_compatible_with},
    toolchain = ":{name}_default_build_flags",
    toolchain_type = "@fuchsia_rules_common//build_flags:toolchain_type",
)
""".format(
                name=name,
                cxx_common_build_flags=format_labels(
                    info.cxx_common_build_flags
                ),
                cxx_executable_build_flags=format_labels(
                    info.cxx_executable_build_flags
                ),
                cxx_shared_library_build_flags=format_labels(
                    info.cxx_shared_library_build_flags
                ),
                rust_common_build_flags=format_labels(
                    info.rust_common_build_flags
                ),
                rust_executable_build_flags=format_labels(
                    info.rust_executable_build_flags
                ),
                rust_shared_library_build_flags=format_labels(
                    info.rust_shared_library_build_flags
                ),
                target_compatible_with=info.target_compatible_with,
            )

        if self.missing_configs:
            content += """
# The following GN configs were ignored because they do not have a matching
# Bazel build_flags() target definition in the source tree:
"""
            for config in self.missing_configs:
                content += f"# - {config}\n"

        return content

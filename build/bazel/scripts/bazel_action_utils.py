# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Helper classes for running Bazel actions and reading configuration set by GN.

- @gn_targets dependencies
- Overall Bazel configuration
  - script feature enablement
  - RBE configuration

"""

import dataclasses
import json
import os
import sys
import typing as T
from pathlib import Path

sys.path.insert(0, os.path.dirname(__file__))
# Insert fuchsia root directory to sys.path so we can import build packages hermetically
_FUCHSIA_DIR = os.path.dirname(
    os.path.dirname(os.path.dirname(os.path.dirname(__file__)))
)
if _FUCHSIA_DIR not in sys.path:
    sys.path.insert(0, _FUCHSIA_DIR)

import bazel_build_events
import build_utils
from build.rbe import rbe_settings
from build_utils import BazelPaths

JSONObject: T.TypeAlias = dict[str, T.Any]


@dataclasses.dataclass(order=True, frozen=True)
class FileOutput:
    """Mapping of a bazel output path to a ninja output path."""

    bazel_path: str
    ninja_path: str


@dataclasses.dataclass(order=True, frozen=True)
class DirectoryOutput:
    """Mapping of a bazel output directory to a ninja output directory.

    Includes tracked files used as the 'marker' files to detect changes in the
    (otherwise opaque) directory contents.
    """

    bazel_path: str
    ninja_path: str
    tracked_files: list[str] = dataclasses.field(default_factory=list)
    tracked_file_ninja_paths: list[str] = dataclasses.field(
        default_factory=list
    )
    copy_debug_symbols: bool = False


@dataclasses.dataclass(order=True, frozen=True)
class PackageOutput:
    """A package created by Bazel that's to be exported back to ninja."""

    package_label: str
    archive_path: str
    copy_debug_symbols: bool = False


@dataclasses.dataclass(order=True, frozen=True)
class FinalSymlinkOutput:
    """A bazel output path that's to be linked to a "final" ninja output path."""

    bazel_path: str
    ninja_path: str


@dataclasses.dataclass
class BazelTargetInfo(object):
    """The outputs to map from Bazel to Ninja, for a given Bazel target."""

    bazel_target: str
    bazel_platform_label: str
    bazel_platform_config: str
    ninja_depfile: str
    gn_targets_manifest: str
    stamp_path: str
    update_rust_project: bool = False
    copy_debug_symbols: bool = False
    extra_bazel_targets_file: str | None = None
    debug_symbols_manifest: str | None = None
    copy_outputs: list[FileOutput] = dataclasses.field(default_factory=list)
    directory_outputs: list[DirectoryOutput] = dataclasses.field(
        default_factory=list
    )
    package_outputs: list[PackageOutput] = dataclasses.field(
        default_factory=list
    )
    final_symlink_outputs: list[FinalSymlinkOutput] = dataclasses.field(
        default_factory=list
    )


class BazelTargetInfosMap(object):
    """A class used to model a map of Bazel target + configuration info to corresponding inputs and outputs.

    This is build from the content of the //:bazel_target_infos generated_file() output.

    See //BUILD.gn for schema description.
    """

    def __init__(self, json_content: list[dict[str, T.Any]]) -> None:
        self._targets: dict[tuple[str, str | None], BazelTargetInfo] = {}

        # LINT.IfChange(bazel_target_infos)
        for entry in json_content:
            bazel_target = entry["bazel_target"]
            bazel_platform_label = entry["bazel_platform_label"]
            bazel_platform_config = entry["bazel_platform_config"]
            ninja_depfile = entry["ninja_depfile"]
            gn_targets_manifest = entry["gn_targets_manifest"]
            stamp_path = entry["stamp_path"]
            update_rust_project = entry["update_rust_project"]
            copy_debug_symbols = entry.get("copy_debug_symbols", False)
            extra_bazel_targets_file = entry.get("extra_bazel_targets_file")
            debug_symbols_manifest = entry.get("debug_symbols_manifest")
            target_info = self._targets.setdefault(
                (bazel_target, bazel_platform_label),
                BazelTargetInfo(
                    bazel_target=bazel_target,
                    bazel_platform_label=bazel_platform_label,
                    bazel_platform_config=bazel_platform_config,
                    ninja_depfile=ninja_depfile,
                    gn_targets_manifest=gn_targets_manifest,
                    stamp_path=stamp_path,
                    update_rust_project=update_rust_project,
                    copy_debug_symbols=copy_debug_symbols,
                    extra_bazel_targets_file=extra_bazel_targets_file,
                    debug_symbols_manifest=debug_symbols_manifest,
                ),
            )

            # each entry is one of several different types, differentiated by a 'type' field
            entry_type: str = entry["type"]

            if entry_type == "file":
                target_info.copy_outputs.append(
                    FileOutput(
                        bazel_path=entry["bazel_file"],
                        ninja_path=entry["ninja_file"],
                    )
                )
            elif entry_type == "directory":
                target_info.directory_outputs.append(
                    DirectoryOutput(
                        bazel_path=entry["bazel_dir"],
                        ninja_path=entry["ninja_dir"],
                        tracked_files=entry["tracked_files"],
                        tracked_file_ninja_paths=[
                            os.path.join(entry["ninja_dir"], p)
                            for p in entry["tracked_files"]
                        ],
                        copy_debug_symbols=entry["copy_debug_symbols"],
                    )
                )
            elif entry_type == "package":
                target_info.package_outputs.append(
                    PackageOutput(
                        package_label=bazel_target,
                        archive_path=entry["ninja_archive"],
                        copy_debug_symbols=entry["copy_debug_symbols"],
                    )
                )
            elif entry_type == "final_symlink":
                target_info.final_symlink_outputs.append(
                    FinalSymlinkOutput(
                        bazel_path=entry["bazel_file"],
                        ninja_path=entry["ninja_file"],
                    )
                )
            else:
                raise ValueError(
                    f"Unknown output entry type in bazel_target_info.json: {entry_type}"
                )

        # Create a map of output to target info for the target used to create the output
        self._targets_by_ninja_output_paths: dict[str, BazelTargetInfo] = {}
        for target_info in self._targets.values():
            for copy_output in target_info.copy_outputs:
                _check_add_ninja_path_to_map(
                    self._targets_by_ninja_output_paths,
                    copy_output.ninja_path,
                    target_info,
                )
            for directory_output in target_info.directory_outputs:
                for tracked_file in directory_output.tracked_files:
                    _check_add_ninja_path_to_map(
                        self._targets_by_ninja_output_paths,
                        os.path.join(directory_output.ninja_path, tracked_file),
                        target_info,
                    )
            for package_output in target_info.package_outputs:
                _check_add_ninja_path_to_map(
                    self._targets_by_ninja_output_paths,
                    package_output.archive_path,
                    target_info,
                )
            for symlink in target_info.final_symlink_outputs:
                _check_add_ninja_path_to_map(
                    self._targets_by_ninja_output_paths,
                    symlink.ninja_path,
                    target_info,
                )
            if target_info.debug_symbols_manifest:
                _check_add_ninja_path_to_map(
                    self._targets_by_ninja_output_paths,
                    target_info.debug_symbols_manifest,
                    target_info,
                )
            _check_add_ninja_path_to_map(
                self._targets_by_ninja_output_paths,
                target_info.stamp_path,
                target_info,
            )

        # LINT.ThenChange(//BUILD.gn:bazel_target_infos, //build/bazel/bazel_action.gni:bazel_target_infos)

    @staticmethod
    def create_from_build_dir(build_dir: Path) -> "BazelTargetInfosMap":
        """Create instance from content of Ninja build directory.

        Args:
            build_dir: Ninja build directory, populated by `fx gen`.
        Returns:
            New BazelBuildActionsMap
        Raises:
            FileNotFoundError if file is missing.
        """
        with (build_dir / "bazel_target_infos.json").open("rb") as f:
            content = json.load(f)
        return BazelTargetInfosMap(content)

    def all_infos(self) -> T.Iterable[BazelTargetInfo]:
        """Retrieve the BazelTargetInfo of every declared bazel_action()."""
        return self._targets.values()

    def get_info(
        self, target: str, platform: str | None
    ) -> BazelTargetInfo | None:
        """Retrieve BazelTargetInfo matching a given Bazel target label."""
        return self._targets.get((target, platform))

    def get_target(
        self,
        ninja_output_path: str,
    ) -> BazelTargetInfo | None:
        """Retrieve the Bazel target that matches the given ninja output path."""
        return self._targets_by_ninja_output_paths.get(ninja_output_path)


def _check_add_ninja_path_to_map(
    infos_map: dict[str, BazelTargetInfo],
    ninja_path: str,
    target: BazelTargetInfo,
) -> None:
    """Add the target as the creator of the given ninja path, validating that it's the only one."""
    if ninja_path in infos_map:
        existing = infos_map[ninja_path]
        raise ValueError(
            f"Multiple Bazel targets create the same ninja path: {ninja_path}\n  {existing.bazel_target}\n  {target.bazel_target}"
        )
    infos_map[ninja_path] = target


@dataclasses.dataclass
class BazelRbeSettings(object):
    enabled: bool
    exec_strategy: str | None

    @staticmethod
    def create_from_build_dir(build_dir: Path) -> "BazelRbeSettings":
        """Create instance from content of Ninja build directory.

        Args:
            build_dir: Ninja build directory, populated by `fx gen`.
        Returns:
            New BazelGlobalArguments
        Raises:
            ValueError if the file is missing or invalid.
        """
        # Load and parse using RbeSettings module
        settings = rbe_settings.load(build_dir)
        enabled = settings.bazel_enable
        exec_strategy = settings.bazel_exec_strategy

        if not isinstance(enabled, bool):
            raise ValueError(
                f"'bazel_enable' must be a boolean, not: {enabled}"
            )
        exec_strategy_lookup_map = {
            "remote": "remote",
            "local": "remote_cache_only",
            "nocache": "nocache",
            "": None,
        }
        if not exec_strategy in exec_strategy_lookup_map:
            raise ValueError(
                f"'bazel_exec_strategy' was '{exec_strategy}', but must be empty or one of: {', '.join([key for key in exec_strategy_lookup_map.keys() if key != ''])}\n\n"
            )
        if enabled and not exec_strategy:
            raise ValueError(
                f"A 'bazel_exec_strategy' must be set when 'bazel_rbe_enabled' is true."
            )

        return BazelRbeSettings(
            enabled=enabled,
            exec_strategy=exec_strategy_lookup_map[exec_strategy],
        )


@dataclasses.dataclass
class BazelGlobalArguments(object):
    quiet: bool
    sandbox_debug: bool
    auto_refresh_compdb: bool
    rust_sysroot: Path

    @staticmethod
    def create_from_build_dir(build_dir: Path) -> "BazelGlobalArguments":
        """Create instance from content of Ninja build directory.

        Args:
            build_dir: Ninja build directory, populated by `fx gen`.
        Returns:
            New BazelGlobalArguments
        Raises:
            FileNotFoundError if file is missing.
        """

        # Load settings specified by GN metadata.
        with (build_dir / "bazel_args" / "global_args.json").open("rb") as f:
            content = json.load(f)
            auto_refresh_compdb = content["auto_refresh_compdb"]
            rust_sysroot = (build_dir / content["rust_sysroot"]).resolve()

        # Get settings from the build environment.
        quiet = os.environ.get("FX_BUILD_QUIET") == "1"
        sandbox_debug = os.environ.get("FUCHSIA_DEBUG_BAZEL_SANDBOX") == "1"

        return BazelGlobalArguments(
            quiet=quiet,
            sandbox_debug=sandbox_debug,
            auto_refresh_compdb=auto_refresh_compdb,
            rust_sysroot=rust_sysroot,
        )


# LINT.IfChange(gn_targets_dir)
# Path of the @gn_targets symlink relative to the Bazel workspace directory.
GN_TARGETS_SYMLINK_PATH = "fuchsia_build_generated/gn_targets_dir"
# LINT.ThenChange(//build/bazel/toplevel.MODULE.bazel:gn_targets_dir)


def update_gn_targets_symlink(
    bazel_paths: BazelPaths,
    gn_targets_dir: Path,
    check_license_timestamps: bool = False,
) -> None:
    """Update the (singular) Bazel workspace's symlink to the per-action gn_targets workspace dir.

    This updates the one-and-only Bazel build workspace's symlink to that of the appropriate
    gn_targets directory, so that Bazel has access to the correct inputs from GN for the
    Bazel action about to run.

    Args:
        bazel_paths: The BazelPaths object for the workspace.

        gn_targets_dir: The path to the gn_targets directory the symlink will point to.

        check_license_timestamps: If True, check that the timestamps of the license files
           have been updated by Ninja. This is required if the next Bazel invocation will
           build artifacts.

           On a clean checkout, these files are created with a timestamp of 0,
           to allow Bazel queries to succeed. However, for a Bazel build to be correct,
           the files MUST first be regenerated by Ninja, which will modify their timestamps.
    """
    if check_license_timestamps:
        # LINT.IfChange(all_licenses_spdx_path)
        license_file = gn_targets_dir / "all_licenses.spdx.json"
        # LINT.ThenChange(//build/bazel/scripts/workspace_utils.py:all_licenses_spdx_path)
        license_info = os.stat(license_file)
        assert (
            license_info.st_mtime != 0
        ), f"PANIC: The timestamp of {license_file} is 0. It should have been updated by Ninja.\n"

        # LINT.IfChange(all_license_files)
        license_file_paths = (
            (gn_targets_dir / "all_license_files.txt").read_text().splitlines()
        )
        # LINT.ThenChange(//build/bazel/scripts/workspace_utils.py:all_license_files)
        incorrect_paths: list[Path] = []
        for license_file_path in license_file_paths:
            license_file = gn_targets_dir / license_file_path
            license_info = os.stat(license_file)
            if license_info.st_mtime_ns == 0:
                incorrect_paths.append(license_file)

        assert not incorrect_paths, (
            "PANIC: The timestamp of the following files is 0. It should have been updated by Ninja.:"
            + "\n".join(f"  {path}" for path in sorted(incorrect_paths))
        )

    build_utils.force_symlink(
        bazel_paths.workspace / GN_TARGETS_SYMLINK_PATH,
        gn_targets_dir,
    )


@dataclasses.dataclass(frozen=True)
class AspectManifestOutputs:
    """Aggregated manifest file paths and genquery outputs discovered during a Bazel build."""

    source_files_manifest_paths: list[str] = dataclasses.field(
        default_factory=list
    )
    debug_symbol_manifest_paths: list[str] = dataclasses.field(
        default_factory=list
    )
    rust_analyzer_manifest_paths: list[str] = dataclasses.field(
        default_factory=list
    )
    genquery_output_files: list[str] = dataclasses.field(default_factory=list)

    def to_dict(self) -> dict[str, list[str]]:
        """Return a dictionary mapping manifest categories to lists of output file paths."""
        return {
            "source_files_manifest_paths": list(
                self.source_files_manifest_paths
            ),
            "debug_symbol_manifest_paths": list(
                self.debug_symbol_manifest_paths
            ),
            "rust_analyzer_manifest_paths": list(
                self.rust_analyzer_manifest_paths
            ),
            "genquery_output_files": list(self.genquery_output_files),
        }

    def to_json(self) -> str:
        return json.dumps(self.to_dict(), indent=2)

    def save_to_file(self, path: Path) -> None:
        path.write_text(self.to_json(), encoding="utf-8")

    @classmethod
    def from_dict(cls, data: JSONObject) -> "AspectManifestOutputs":
        return cls(
            source_files_manifest_paths=list(
                data.get("source_files_manifest_paths", [])
            ),
            debug_symbol_manifest_paths=list(
                data.get("debug_symbol_manifest_paths", [])
            ),
            rust_analyzer_manifest_paths=list(
                data.get("rust_analyzer_manifest_paths", [])
            ),
            genquery_output_files=list(data.get("genquery_output_files", [])),
        )

    @classmethod
    def load_from_file(cls, path: Path) -> "AspectManifestOutputs":
        return cls.from_dict(json.loads(path.read_text(encoding="utf-8")))

    @classmethod
    def from_build_event_stream(
        cls,
        stream: bazel_build_events.BuildEventStream,
        execroot: Path | None = None,
    ) -> "AspectManifestOutputs":
        """Extract aspect manifest paths and genqueries from a generic BEP event stream."""
        source_files = [
            f
            for f in stream.get_output_group_files(
                "fuchsia_sources_manifest", execroot
            )
            if "buildfiles_genquery" not in f
        ]

        debug_symbols = [
            f"{build_utils.canonicalize_label(label)},{file_path}"
            for label, file_path in stream.get_target_output_group_files(
                "debug_symbol_manifest", execroot=execroot
            )
            if "buildfiles_genquery" not in file_path
            and file_path.endswith(".debug_symbols.json")
        ]

        rust_manifests = [
            f
            for f in stream.get_output_group_files(
                "fuchsia_rust_analyzer_manifest", execroot
            )
            if "buildfiles_genquery" not in f
        ]

        genqueries = [
            f"{build_utils.canonicalize_label(label)},{file_path}"
            for label, file_path in stream.get_target_output_group_files(
                "default",
                label_predicate=lambda l: "buildfiles_genquery" in l,
                execroot=execroot,
            )
        ]

        return cls(
            source_files_manifest_paths=list(dict.fromkeys(source_files)),
            debug_symbol_manifest_paths=list(dict.fromkeys(debug_symbols)),
            rust_analyzer_manifest_paths=list(dict.fromkeys(rust_manifests)),
            genquery_output_files=list(dict.fromkeys(genqueries)),
        )


def parse_build_event_manifests(
    bep_json_path: Path,
    execroot: Path | None = None,
) -> AspectManifestOutputs:
    """Parse a BEP JSON file and extract manifest paths using AspectManifestOutputs."""
    stream = bazel_build_events.BuildEventStream.from_file(bep_json_path)
    return AspectManifestOutputs.from_build_event_stream(stream, execroot)

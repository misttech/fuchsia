#!/usr/bin/env python3

# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Generate a standalone Bazel launcher script for the Fuchsia Bazel SDK test suite.

This script creates an executable `.bazel` launcher script (hard-linked or
copied from `bazel_launcher.sh`), a companion `.config` file defining the
read-only parameters it needs to run, and a companion `.json` metadata file
describing the repository map for depfile generation.
"""

import argparse
import enum
import json
import os
import platform
import shlex
import shutil
import subprocess
import sys
import typing as T
from pathlib import Path


# The three different types of inputs supported by this script:
# IN_TREE: Used to test the in-tree @fuchsia_sdk and @fuchsia_in_tree_idk repos.
# SDK: Used to test a standalone (OOT) Fuchsia SDK directory.
# IDK: Used to test a standalone Fuchsia IDK directory.
class InputMode(enum.Enum):
    IN_TREE = "in-tree"
    SDK = "sdk"
    IDK = "idk"


_CANONICAL_FUCHSIA_SDK_REPO_NAME = "rules_fuchsia++fuchsia_sdk_ext+fuchsia_sdk"
_CANONICAL_IN_TREE_IDK_REPO_NAME = "fuchsia_in_tree_idk+"
_CANONICAL_RULES_FUCHSIA_REPO_NAME = "rules_fuchsia+"

# LINT.IfChange(bazel_vendor_dir)
_DEFAULT_BAZEL_VENDOR_DIR = "third_party/bazel_vendor/fuchsia_in_tree"
# LINT.ThenChange(//build/bazel/enable_vendor_mode.bazelrc:bazel_vendor_dir)

# LINT.IfChange(bazel_registry_dir)
_DEFAULT_BAZEL_REGISTRY_DIR = "third_party/bazel_registries/bcr.bazel.build"
# LINT.ThenChange(//build/bazel/enable_vendor_mode.bazelrc:bazel_registry_dir)

# Maps from apparent repo names to canonical repo names.
_APPARENT_REPO_NAME_TO_CANONICAL = {
    "fuchsia_sdk": _CANONICAL_FUCHSIA_SDK_REPO_NAME,
    "fuchsia_in_tree_idk": _CANONICAL_IN_TREE_IDK_REPO_NAME,
}

# Default location of @rules_fuchsia directory, relative to Fuchsia source root.
_RULES_FUCHSIA_DEFAULT_DIR = "build/bazel_sdk/bazel_rules_fuchsia"

# Default location of in-tree @fuchsia_sdk repository directory.
_LOCAL_FUCHSIA_SDK_DEFAULT_DIR = (
    f"gen/build/bazel/output_base/external/{_CANONICAL_FUCHSIA_SDK_REPO_NAME}"
)

# Default location if in-tree @fuchsia_in_tree_idk repository.
_LOCAL_IN_TREE_IDK_DEFAULT_DIR = (
    f"gen/build/bazel/output_base/external/{_CANONICAL_IN_TREE_IDK_REPO_NAME}"
)

StrOrPath = str | Path


def _print_error(msg: str) -> int:
    """Print error message to stderr then return 1."""
    print("ERROR: " + msg, file=sys.stderr)
    return 1


def _find_fuchsia_source_dir_from(path: Path) -> Path | None:
    """Try to find the Fuchsia source directory from a starting location."""
    if path.is_file():
        path = path.parent

    path = path.resolve()
    while True:
        if str(path) == "/":
            return None
        if (path / ".jiri_manifest").exists():
            return path
        path = path.parent


def _find_fuchsia_build_dir(fuchsia_source_dir: Path) -> Path | None:
    """Find the current Fuchsia build directory."""
    fx_build_dir = fuchsia_source_dir / ".fx-build-dir"
    if not fx_build_dir.exists():
        return None

    with open(fx_build_dir) as f:
        return fuchsia_source_dir / f.read().strip()


def _flatten_comma_list(items: T.Iterable[str]) -> T.Iterable[str]:
    """Flatten ["a,b", "c,d"] -> ["a", "b", "c", "d"]."""
    for item in items:
        yield from item.split(",")


JSONObject: T.TypeAlias = dict[str, T.Any]


class BazelRepositoryMap(object):
    IGNORED_REPO = Path("IGNORED")

    def __init__(
        self,
        fuchsia_source_dir: Path,
        rules_fuchsia_dir: Path,
        bazel_vendor_dir: Path,
        explicit_fuchsia_sdk: Path | None,
        explicit_fuchsia_in_tree_idk: Path | None,
        workspace_dir: Path,
        output_base: Path,
    ):
        """Initialize instance.

        Args:
            fuchsia_source_dir: Path to Fuchsia source directory.
            rules_fuchsia_dir: Path to the @rules_fuchsia directory.
            bazel_vendor_dir; Path to the Bazel vendor directory.
            explicit_fuchsia_sdk: Optional path to an explicit Fuchsia SDK directory.
            explicit_fuchsia_in_tree_idk: Optional path to an explicit
               Fuchsia in-tree IDK directory.
            workspace_dir: Path to the Bazel workspace directory.
            output_base: Path to the Bazel output base.
        """
        self._fuchsia_source_dir = fuchsia_source_dir
        self._workspace_dir = workspace_dir
        self._output_base = output_base

        # These repository overrides are passed to the Bazel invocation.
        self._overrides: dict[str, Path] = {}

        if explicit_fuchsia_sdk:
            self._overrides[
                _CANONICAL_FUCHSIA_SDK_REPO_NAME
            ] = explicit_fuchsia_sdk.resolve()
        if explicit_fuchsia_in_tree_idk:
            self._overrides[
                _CANONICAL_IN_TREE_IDK_REPO_NAME
            ] = explicit_fuchsia_in_tree_idk.resolve()

        self._overrides[_CANONICAL_RULES_FUCHSIA_REPO_NAME] = rules_fuchsia_dir

        # These repository overrides are used when converting Bazel labels to actual paths.
        self._internal_overrides = self._overrides | {
            "rules_cc": bazel_vendor_dir / "rules_cc+",
            "rules_license": bazel_vendor_dir / "rules_license+",
            "bazel_tools+remote_coverage_tools_extension+remote_coverage_tools": bazel_vendor_dir
            / "bazel_tools+remote_coverage_tools_extension+remote_coverage_tools",
            "bazel_skylib": bazel_vendor_dir / "bazel_skylib+",
            "bazel_features+": bazel_vendor_dir / "bazel_features+",
            "rules_python": bazel_vendor_dir / "rules_python+",
            "platforms": bazel_vendor_dir / "platforms",
            "com_google_googletest": fuchsia_source_dir
            / "third_party/googletest/src",
            "com_google_protobuf": fuchsia_source_dir / "third_party/protobuf",
            "rules_fuchsia": rules_fuchsia_dir,
            "zlib": fuchsia_source_dir / "third_party/zlib",
            "prebuilt_python": self.IGNORED_REPO,
            "fuchsia_clang": self.IGNORED_REPO,
            "bazel_features++version_extension+bazel_features_globals": self.IGNORED_REPO,
            "bazel_features++version_extension+bazel_features_version": self.IGNORED_REPO,
            "bazel_tools": self.IGNORED_REPO,
            "bazel_tools+cc_configure_extension+local_config_cc": self.IGNORED_REPO,
            "platforms+host_platform+host_platform": self.IGNORED_REPO,
            "rules_cc++compatibility_proxy+cc_compatibility_proxy": self.IGNORED_REPO,
            "rules_python++internal_deps+rules_python_internal": self.IGNORED_REPO,
            "rules_python++config+rules_python_internal": self.IGNORED_REPO,
            "rules_python++python+pythons_hub": self.IGNORED_REPO,
            "rules_shell+": self.IGNORED_REPO,
            "package_metadata+": self.IGNORED_REPO,
            "test_fuchsia_products": self.IGNORED_REPO,
        }

        if not explicit_fuchsia_sdk:
            self._internal_overrides[_CANONICAL_FUCHSIA_SDK_REPO_NAME] = (
                self._fuchsia_source_dir / "build/bazel_sdk/bazel_rules_fuchsia"
            )

    def add_override(self, name: str, path: Path) -> None:
        assert (
            name not in self._overrides
        ), f"Override is already registered for {name}: {self._overrides[name]} vs {path}"
        self._overrides[name] = path
        self._internal_overrides[name] = path

    def get_repository_overrides_flags(self) -> T.Sequence[str]:
        """Return a sequence of command-line flags for overriding Bazel external repositories."""
        return [
            f"--override_repository={name}={path}"
            for name, path in self._overrides.items()
        ]

    def resolve_bazel_path(self, bazel_path: str) -> Path | None:
        """Convert a Bazel path label to a real Path or None if it should be ignored.

        Args:
            bazel_path: An input Bazel path label. This must start with @ or //.
        Returns:
            None if the input path should be ignored, otherwise bazel_path.
        Raises:
            ValueError is the input path is malformed or doesn't reference a
            real input file.
        """
        if bazel_path.startswith("//"):
            target_path = bazel_path[2:]
            repo_dir = self._workspace_dir
            repo_name = ""
        elif bazel_path.startswith("@"):
            repo_name, sep, target_path = bazel_path.partition("//")
            if not sep:
                raise ValueError(
                    f"Build file path has invalid repository root: {bazel_path}"
                )
            repo_name = repo_name.removeprefix("@@").removeprefix("@")
            repo_name = _APPARENT_REPO_NAME_TO_CANONICAL.get(
                repo_name, repo_name
            )
            _repo_dir = self._internal_overrides.get(repo_name, None)
            if not _repo_dir:
                raise ValueError(
                    f"Unknown repository name {repo_name} in build file path: {bazel_path}\n"
                    + f"Please modify {__file__} to handle it!"
                )
            repo_dir = _repo_dir
            if repo_dir == self.IGNORED_REPO:
                return None
        else:
            assert False, f"Invalid build file path: {bazel_path}"

        package_dir, colon, target_name = target_path.partition(":")
        if colon == ":":
            if package_dir:
                target_path = f"{package_dir}/{target_name}"
            else:
                target_path = target_name
        else:
            target_path = package_dir + "/" + os.path.basename(package_dir)

        final_path = repo_dir / target_path
        if final_path.exists():
            return final_path.resolve()

        if repo_name:
            external_repo_dir = self._output_base / "external" / repo_name
            final_path = external_repo_dir / target_path
            if not final_path.exists():
                raise ValueError(
                    f"When resolving {bazel_path}: File '{final_path}' does not exist."
                )
            return final_path.resolve()

        raise ValueError(
            f"Unknown input label, please update {__file__} to handle it: {bazel_path}"
        )

    def to_config_dict(self) -> dict[str, str | None]:
        """Serialize repository map configuration to a JSON-compatible dictionary."""
        return {
            "fuchsia_source_dir": str(self._fuchsia_source_dir),
            "rules_fuchsia_dir": str(
                self._overrides[_CANONICAL_RULES_FUCHSIA_REPO_NAME]
            ),
            "bazel_vendor_dir": str(
                self._internal_overrides["platforms"].parent
            ),
            "explicit_fuchsia_sdk": (
                str(self._overrides[_CANONICAL_FUCHSIA_SDK_REPO_NAME])
                if _CANONICAL_FUCHSIA_SDK_REPO_NAME in self._overrides
                else None
            ),
            "explicit_fuchsia_in_tree_idk": (
                str(self._overrides[_CANONICAL_IN_TREE_IDK_REPO_NAME])
                if _CANONICAL_IN_TREE_IDK_REPO_NAME in self._overrides
                else None
            ),
            "workspace_dir": str(self._workspace_dir),
            "output_base": str(self._output_base),
        }

    @classmethod
    def from_config_dict(cls, data: JSONObject) -> "BazelRepositoryMap":
        """Reconstruct a BazelRepositoryMap instance from a dictionary."""
        return cls(
            fuchsia_source_dir=Path(data["fuchsia_source_dir"]),
            rules_fuchsia_dir=Path(data["rules_fuchsia_dir"]),
            bazel_vendor_dir=Path(data["bazel_vendor_dir"]),
            explicit_fuchsia_sdk=(
                Path(data["explicit_fuchsia_sdk"])
                if data.get("explicit_fuchsia_sdk")
                else None
            ),
            explicit_fuchsia_in_tree_idk=(
                Path(data["explicit_fuchsia_in_tree_idk"])
                if data.get("explicit_fuchsia_in_tree_idk")
                else None
            ),
            workspace_dir=Path(data["workspace_dir"]),
            output_base=Path(data["output_base"]),
        )


def _write_launcher_files(
    launcher_path: Path,
    workspace_dir: Path,
    fuchsia_source_dir: Path,
    bazel_bin: Path,
    output_base: Path,
    output_user_root: Path | None,
    python_prebuilt_dir: Path,
    python_version_file: Path | None,
    clang_version_file: Path | None,
    downloader_config_file: Path,
    bazel_vendor_dir: Path,
    bazel_registry_dir: Path,
    startup_flags: T.Sequence[str],
    repo_override_flags: T.Sequence[str],
    bazel_config_args: T.Sequence[str],
    extra_env: dict[str, str],
    bazel_repo_map: BazelRepositoryMap,
) -> None:
    """Write launcher configuration, link or copy launcher script, and write companion .json."""
    launcher_path = launcher_path.absolute()
    launcher_path.parent.mkdir(parents=True, exist_ok=True)

    bazel_launcher_script = (
        Path(__file__).parent / "bazel_launcher.sh"
    ).absolute()
    if launcher_path == bazel_launcher_script:
        raise ValueError(
            f"Launcher path cannot be the template script itself: {launcher_path}"
        )

    launcher_path.unlink(missing_ok=True)
    try:
        launcher_path.hardlink_to(bazel_launcher_script)
    except OSError:
        shutil.copy2(bazel_launcher_script, launcher_path)
    launcher_path.chmod(0o755)

    def fmt_array(name: str, items: T.Sequence[str]) -> str:
        if not items:
            return f"readonly {name}=()"
        lines = [f"readonly {name}=("]
        for item in items:
            lines.append(f"  {shlex.quote(str(item))}")
        lines.append(")")
        return "\n".join(lines)

    def quote_or_empty(val: T.Any) -> str:
        return shlex.quote(str(val)) if val else '""'

    explicit_sdk = bazel_repo_map.to_config_dict().get("explicit_fuchsia_sdk")

    config_lines = [
        "# AUTO-GENERATED BY //build/bazel_sdk/tests/scripts/generate_bazel_launcher.py - DO NOT EDIT!",
        "",
        f"readonly _WORKSPACE_DIR={shlex.quote(str(workspace_dir))}",
        f"readonly _FUCHSIA_SOURCE_DIR={shlex.quote(str(fuchsia_source_dir))}",
        f"readonly _BAZEL_BIN={shlex.quote(str(bazel_bin))}",
        f"readonly _OUTPUT_BASE={shlex.quote(str(output_base))}",
        f"readonly _OUTPUT_USER_ROOT={quote_or_empty(output_user_root)}",
        f"readonly _PYTHON_PREBUILT_DIR={shlex.quote(str(python_prebuilt_dir))}",
        f"readonly _PYTHON_VERSION_FILE={quote_or_empty(python_version_file)}",
        f"readonly _CLANG_VERSION_FILE={quote_or_empty(clang_version_file)}",
        f"readonly _DOWNLOADER_CONFIG_FILE={shlex.quote(str(downloader_config_file))}",
        f"readonly _BAZEL_VENDOR_DIR={shlex.quote(str(bazel_vendor_dir))}",
        f"readonly _BAZEL_REGISTRY_DIR={shlex.quote(str(bazel_registry_dir))}",
        f"readonly _EXPLICIT_FUCHSIA_SDK={quote_or_empty(explicit_sdk)}",
        "",
        fmt_array("_STARTUP_FLAGS", startup_flags),
        "",
        fmt_array("_REPO_OVERRIDE_FLAGS", repo_override_flags),
        "",
        fmt_array("_CONFIG_ARGS", bazel_config_args),
    ]

    if extra_env:
        config_lines.append("")
        for k, v in sorted(extra_env.items()):
            config_lines.append(f"export {k}={shlex.quote(str(v))}")

    config_lines.append("")
    config_path = Path(f"{launcher_path}.config")
    config_path.write_text("\n".join(config_lines))

    metadata_path = Path(f"{launcher_path}.json")
    metadata_path.write_text(
        json.dumps(bazel_repo_map.to_config_dict(), indent=2, sort_keys=True)
    )


def add_launcher_arguments(
    parser: argparse.ArgumentParser,
    mutex_group: T.Any,
) -> None:
    """Register CLI arguments used to configure and generate a Bazel launcher."""
    parser.add_argument("--bazel", type=Path, help="Specify bazel binary.")
    mutex_group.add_argument(
        "--fuchsia_sdk_directory",
        type=Path,
        help="Specify Fuchsia SDK directory.",
    )
    mutex_group.add_argument(
        "--fuchsia_idk_directory",
        type=Path,
        help="Specify Fuchsia IDK directory.",
    )
    mutex_group.add_argument(
        "--fuchsia-in-tree-sdk",
        action="store_true",
        help="Run against the in-tree @fuchsia_sdk and @fuchsia_in_tree_idk repositories.",
    )
    parser.add_argument(
        "--fuchsia_source_dir",
        type=Path,
        help="Specify Fuchsia source directory (default is auto-detected).",
    )
    parser.add_argument(
        "--fuchsia_build_dir",
        type=Path,
        help="Specify Fuchsia build directory (default is auto-detected).",
    )
    parser.add_argument(
        "--rules_fuchsia_dir",
        type=Path,
        help="Specify @rules_fuchsia directory (default is auto-detected).",
    )
    parser.add_argument(
        "--in_tree_fuchsia_sdk",
        type=Path,
        help="Specify alternative in-tree @fuchsia_sdk source repository, when using --fuchsia-in-tree-sdk",
    )
    parser.add_argument(
        "--in_tree_fuchsia_idk",
        type=Path,
        help="Specify alternative path to in-tree @fuchsia_in_tree_idk when using --fuchsia-in-tree-sdk",
    )
    parser.add_argument(
        "--workspace_dir",
        "--workspace-dir",
        dest="workspace_dir",
        type=Path,
        help="Use specific Bazel workspace directory (defaults to //build/bazel_sdk/tests).",
    )
    parser.add_argument(
        "--output_base",
        type=Path,
        help="Use specific Bazel output base directory.",
    )
    parser.add_argument(
        "--output_user_root",
        type=Path,
        help="Use specific Bazel output user root directory.",
    )
    parser.add_argument(
        "--prebuilt-python-version-file",
        type=Path,
        help="Optional path to version file for prebuilt python toolchain.",
    )
    parser.add_argument(
        "--prebuilt-clang-version-file",
        type=Path,
        help="Optional path to version file for prebuilt Clang toolchain.",
    )
    parser.add_argument(
        "--target_cpu",
        help="Target cpu name, using Fuchsia conventions (default is auto-detected).",
    )
    parser.add_argument(
        "--bazel-vendor-dir",
        type=Path,
        help=f"Specify Bazel vendor directory to use or write to. Defaults to $FUCHSIA/{_DEFAULT_BAZEL_VENDOR_DIR}.",
    )
    parser.add_argument(
        "--bazel-registry-dir",
        type=Path,
        help=f"Specify Bazel registry directory to use. Ignored in vendor mode. Defaults to $FUCHSIA/{_DEFAULT_BAZEL_REGISTRY_DIR}.",
    )
    parser.add_argument(
        "--bazelrc",
        help="Additional Bazel configuration file to load",
        type=Path,
        default=[],
        metavar="FILE",
        action="append",
    )
    parser.add_argument(
        "--bazel-config",
        help="Additional Bazel --config options, comma-separated, repeatable",
        default=[],
        metavar="CFG",
        action="append",
    )


def generate_launcher_from_args(
    args: argparse.Namespace,
    parser: argparse.ArgumentParser,
    launcher_path: Path | None = None,
) -> tuple[int, Path | None, BazelRepositoryMap | None]:
    """Generate the launcher script and repository map from parsed CLI args.

    Returns:
        (exit_code, launcher_path, bazel_repo_map)
    """
    if args.fuchsia_source_dir:
        fuchsia_source_dir = args.fuchsia_source_dir
    else:
        _fuchsia_source_dir = _find_fuchsia_source_dir_from(Path(__file__))
        if not _fuchsia_source_dir:
            return (
                _print_error(
                    "Cannot find Fuchsia source directory, please use --fuchsia_source_dir=DIR"
                ),
                None,
                None,
            )
        fuchsia_source_dir = _fuchsia_source_dir

    if not fuchsia_source_dir.exists():
        return (
            _print_error(
                f"Fuchsia source directory does not exist: {fuchsia_source_dir}"
            ),
            None,
            None,
        )

    fuchsia_source_dir = fuchsia_source_dir.resolve()

    fuchsia_build_dir = args.fuchsia_build_dir
    if fuchsia_build_dir is None:
        fuchsia_build_dir = _find_fuchsia_build_dir(fuchsia_source_dir)

    rules_fuchsia_dir = args.rules_fuchsia_dir
    if rules_fuchsia_dir is None:
        rules_fuchsia_dir = fuchsia_source_dir / _RULES_FUCHSIA_DEFAULT_DIR
    rules_fuchsia_dir = rules_fuchsia_dir.resolve()

    def check_fuchsia_build_dir(
        print_error: T.Callable[[str], T.Any] | None = None,
    ) -> bool:
        if not fuchsia_build_dir:
            if print_error:
                print_error(
                    "Cannot auto-detect Fuchsia build directory, use --fuchsia_build_dir=DIR"
                )
            return False
        if not fuchsia_build_dir.exists():
            if print_error:
                print_error(
                    f"Fuchsia build directory does not exist: {fuchsia_build_dir}"
                )
            return False
        return True

    input_mode: None | InputMode = None
    if args.fuchsia_in_tree_sdk:
        input_mode = InputMode.IN_TREE
        if not (args.in_tree_fuchsia_sdk and args.in_tree_fuchsia_idk):
            if not check_fuchsia_build_dir(print_error=_print_error):
                return (1, None, None)
    elif args.fuchsia_sdk_directory:
        input_mode = InputMode.SDK
        if not args.fuchsia_sdk_directory.exists():
            return (
                _print_error(
                    f"Fuchsia SDK directory does not exist: {args.fuchsia_sdk_directory}"
                ),
                None,
                None,
            )
    elif args.fuchsia_idk_directory:
        input_mode = InputMode.IDK
        if not args.fuchsia_idk_directory.exists():
            return (
                _print_error(
                    f"Fuchsia IDK directory does not exist: {args.fuchsia_idk_directory}"
                ),
                None,
                None,
            )
    else:
        assert False, "Internal error: Invalid build mode!"

    # Compute Fuchsia host tag
    u = platform.uname()
    host_os = {
        "Linux": "linux",
        "Darwin": "mac",
        "Windows": "win",
    }.get(u.system, u.system)

    host_cpu = {
        "x86_64": "x64",
        "AMD64": "x64",
        "aarch64": "arm64",
    }.get(u.machine, u.machine)

    host_tag = f"{host_os}-{host_cpu}"

    if args.bazel:
        bazel = args.bazel.resolve()
    else:
        bazel = (
            fuchsia_source_dir / f"prebuilt/third_party/bazel/{host_tag}/bazel"
        )

    if args.target_cpu:
        target_cpu = args.target_cpu
    else:
        target_cpu = ""
        if check_fuchsia_build_dir():
            assert fuchsia_build_dir
            args_json = fuchsia_build_dir / "args.json"
            if args_json.exists():
                with open(args_json) as f:
                    target_cpu = json.load(f)["target_cpu"]

        if not target_cpu:
            parser.error(
                "Cannot auto-detect --target_cpu, use --target_cpu=CPU"
            )

    script_dir = Path(__file__).parent.resolve()
    tests_source_dir = script_dir.parent

    if args.workspace_dir:
        workspace_dir = args.workspace_dir.resolve()
    else:
        workspace_dir = tests_source_dir

    downloader_config_file = (
        fuchsia_source_dir / "build/bazel/config/no_downloads_allowed.config"
    )
    bazel_vendor_dir = (
        args.bazel_vendor_dir or fuchsia_source_dir / _DEFAULT_BAZEL_VENDOR_DIR
    ).resolve()

    bazel_registry_dir = (
        args.bazel_registry_dir
        or fuchsia_source_dir / _DEFAULT_BAZEL_REGISTRY_DIR
    ).resolve()

    python_prebuilt_dir = (
        fuchsia_source_dir / f"prebuilt/third_party/python3/{host_tag}"
    )
    python_version_file = args.prebuilt_python_version_file
    if not python_version_file:
        python_version_file = (
            python_prebuilt_dir / ".versions/cpython3.cipd_version"
        )

    clang_version_file = args.prebuilt_clang_version_file
    if not clang_version_file:
        clang_version_file = (
            fuchsia_source_dir
            / f"prebuilt/third_party/clang/{host_tag}/.versions/clang.cipd_version"
        )

    startup_flags = [
        "--nohome_rc",
    ]
    for rc in args.bazelrc:
        startup_flags.append(f"--bazelrc={rc.resolve()}")

    output_user_root = None
    if args.output_user_root:
        output_user_root = args.output_user_root.resolve()
        output_user_root.mkdir(parents=True, exist_ok=True)

    if args.output_base:
        output_base = args.output_base.resolve()
        output_base.mkdir(parents=True, exist_ok=True)
    else:
        info_cmd = [str(bazel), "--batch"] + startup_flags
        if output_user_root:
            info_cmd.append(f"--output_user_root={output_user_root}")
        info_cmd += ["info", "output_base"]

        info_env = os.environ.copy()
        info_env["BAZEL_DO_NOT_DETECT_CPP_TOOLCHAIN"] = "1"
        info_env[
            "PATH"
        ] = f"{python_prebuilt_dir}/bin:{info_env.get('PATH', '')}"
        info_env.setdefault("USER", "unused-bazel-build-user")

        ret = subprocess.run(
            info_cmd,
            cwd=workspace_dir,
            env=info_env,
            capture_output=True,
            text=True,
            check=True,
        )
        output_base = Path(ret.stdout.splitlines()[0])

    explicit_fuchsia_sdk = None
    explicit_fuchsia_in_tree_idk = None
    if input_mode == InputMode.IN_TREE:
        explicit_fuchsia_sdk = args.in_tree_fuchsia_sdk
        if not explicit_fuchsia_sdk:
            assert fuchsia_build_dir
            explicit_fuchsia_sdk = (
                fuchsia_build_dir / _LOCAL_FUCHSIA_SDK_DEFAULT_DIR
            )
        explicit_fuchsia_in_tree_idk = args.in_tree_fuchsia_idk
        if not explicit_fuchsia_in_tree_idk:
            assert fuchsia_build_dir
            explicit_fuchsia_in_tree_idk = (
                fuchsia_build_dir / _LOCAL_IN_TREE_IDK_DEFAULT_DIR
            )
    elif input_mode == InputMode.SDK:
        explicit_fuchsia_sdk = args.fuchsia_sdk_directory

    bazel_repo_map = BazelRepositoryMap(
        fuchsia_source_dir=fuchsia_source_dir,
        rules_fuchsia_dir=rules_fuchsia_dir,
        bazel_vendor_dir=bazel_vendor_dir,
        explicit_fuchsia_sdk=explicit_fuchsia_sdk,
        explicit_fuchsia_in_tree_idk=explicit_fuchsia_in_tree_idk,
        workspace_dir=workspace_dir,
        output_base=output_base,
    )

    repo_override_flags = bazel_repo_map.get_repository_overrides_flags()

    bazel_config_args = [
        f"--config=fuchsia_{target_cpu}",
        "--java_runtime_version=embedded_jdk",
        "--tool_java_runtime_version=embedded_jdk",
        "--experimental_writable_outputs",
    ]
    bazel_config_args += [
        f"--config={cfg}" for cfg in _flatten_comma_list(args.bazel_config)
    ]
    bazel_config_args += [
        "--verbose_failures",
    ]

    launcher_extra_env: dict[str, str] = {}
    if input_mode == InputMode.SDK:
        launcher_extra_env["LOCAL_FUCHSIA_SDK_DIRECTORY"] = str(
            args.fuchsia_sdk_directory.resolve()
        )
    elif input_mode == InputMode.IDK:
        launcher_extra_env["LOCAL_FUCHSIA_IDK_DIRECTORY"] = str(
            args.fuchsia_idk_directory.resolve()
        )

    final_launcher_path = (
        launcher_path.absolute()
        if launcher_path
        else (
            output_base.parent / f"{output_base.name}.launcher.bazel"
        ).absolute()
    )

    _write_launcher_files(
        launcher_path=final_launcher_path,
        workspace_dir=workspace_dir,
        fuchsia_source_dir=fuchsia_source_dir,
        bazel_bin=bazel,
        output_base=output_base,
        output_user_root=output_user_root,
        python_prebuilt_dir=python_prebuilt_dir,
        python_version_file=(
            python_version_file.resolve()
            if python_version_file and python_version_file.exists()
            else None
        ),
        clang_version_file=(
            clang_version_file.resolve()
            if clang_version_file and clang_version_file.exists()
            else None
        ),
        downloader_config_file=downloader_config_file,
        bazel_vendor_dir=bazel_vendor_dir,
        bazel_registry_dir=bazel_registry_dir,
        startup_flags=startup_flags,
        repo_override_flags=repo_override_flags,
        bazel_config_args=bazel_config_args,
        extra_env=launcher_extra_env,
        bazel_repo_map=bazel_repo_map,
    )

    return (0, final_launcher_path, bazel_repo_map)


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--output",
        "--launcher-path",
        dest="output",
        type=Path,
        required=True,
        help="Output path for the generated Bazel launcher script.",
    )
    mutex_group = parser.add_mutually_exclusive_group(required=True)
    add_launcher_arguments(parser, mutex_group)
    args = parser.parse_args()

    rc, _, _ = generate_launcher_from_args(args, parser, args.output)
    return rc


if __name__ == "__main__":
    sys.exit(main())

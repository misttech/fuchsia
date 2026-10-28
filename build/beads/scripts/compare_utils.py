# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import concurrent.futures
import dataclasses
import os
import pathlib
import shlex
import typing as T

from build.api import gn_ninja_outputs
from build.bazel.scripts import bazel_build_args, build_utils
from build.bazel.scripts.build_utils import BazelLauncher, NinjaRunner
from build.beads.scripts import (
    normalize_clang_args,
    normalize_rustc_args,
    path_normalizer,
    shell_utils,
)

# Root directory of the Fuchsia source tree.
_FUCHSIA_DIR = pathlib.Path(__file__).parent.parent.parent.parent

# When packaged into a python_binary zipapp (.pyz), bazel_build_args._EXPAND_BUILD_ARGS_JSON_CQUERY_PATH
# resolves relative to the zip archive, which does not exist on the filesystem for Bazel cquery.
# Point it directly to the file in the Fuchsia source tree if the default path doesn't exist.
if not os.path.exists(bazel_build_args._EXPAND_BUILD_ARGS_JSON_CQUERY_PATH):
    try:
        _cquery_file = (
            build_utils.find_fuchsia_dir()
            / "build/bazel/starlark/expand_build_args_json.cquery"
        )
        if _cquery_file.exists():
            bazel_build_args._EXPAND_BUILD_ARGS_JSON_CQUERY_PATH = str(
                _cquery_file
            )
    except ValueError:
        pass

# Supported action types for querying build commands.
ACTION_RUSTC = "rustc"
ACTION_C_COMPILE = "c_compile"
ACTION_CPP_COMPILE = "cpp_compile"
ACTION_ASSEMBLE = "assemble"
ACTION_CPP_LINK = "cpp_link"

VALID_ACTION_TYPES = (
    ACTION_RUSTC,
    ACTION_C_COMPILE,
    ACTION_CPP_COMPILE,
    ACTION_ASSEMBLE,
    ACTION_CPP_LINK,
)

# Supported Bazel configuration names for querying build commands.
BAZEL_CONFIG_HOST = "host"
BAZEL_CONFIG_FUCHSIA_PLATFORM = "fuchsia_platform"
BAZEL_CONFIG_FUCHSIA_SDK = "fuchsia_sdk"

VALID_BAZEL_CONFIGS = (
    BAZEL_CONFIG_HOST,
    BAZEL_CONFIG_FUCHSIA_PLATFORM,
    BAZEL_CONFIG_FUCHSIA_SDK,
)

# An optional callable to receive debug messages
DebugHook: T.TypeAlias = T.Optional[T.Callable[[str], None]]


JSONObject: T.TypeAlias = dict[str, T.Any]


TV = T.TypeVar("TV")


def _validate_value(value: T.Any, expected_type: type[TV], name: str) -> TV:
    """Validate the type of a given value.

    Args:
      value: Input value
      expected_type: Expected type value (e.g. 'str')
      name: Description used for exception message.
    Returns:
      The input value
    Raises:
      ValueError if |value| does not match |expected_type|.
    """
    if not isinstance(value, expected_type):
        raise ValueError(
            f"Invalid {name} type, expected {expected_type}, got {type(value)}"
        )
    return value


def _validate_action_type(action_type: str) -> str:
    """Validate that action_type is one of the supported VALID_ACTION_TYPES.

    Args:
        action_type: Action type string to validate.
    Returns:
        the input value.
    Raises:
        ValueError: If action_type is not in VALID_ACTION_TYPES.
    """
    if action_type not in VALID_ACTION_TYPES:
        raise ValueError(
            f"Invalid action_type '{action_type}'. Must be one of: {', '.join(VALID_ACTION_TYPES)}"
        )
    return action_type


def _validate_bazel_config(bazel_config: str) -> str:
    """Validate that bazel_config is one of the supported VALID_BAZEL_CONFIGS.

    Args:
        bazel_config: Bazel config string to validate. Supported values:
            - "host": Build host artifacts.
            - "fuchsia_platform": Build Fuchsia platform artifacts.
            - "fuchsia_sdk": Build Fuchsia artifacts using the in-tree Bazel SDK.
    Returns:
        the input value.
    Raises:
        ValueError: If bazel_config is not in VALID_BAZEL_CONFIGS.
    """
    if bazel_config not in VALID_BAZEL_CONFIGS:
        raise ValueError(
            f"Invalid bazel_config '{bazel_config}'. Must be one of: {', '.join(VALID_BAZEL_CONFIGS)}"
        )
    return bazel_config


def action_type_to_mnemonic(action_type: str) -> str:
    """Convert an action type to the corresponding Bazel action mnemonic.

    Args:
        action_type: One of VALID_ACTION_TYPES.

    Returns:
        Bazel mnemonic string (e.g. CppCompile, CppLink, Rustc).
    """
    _validate_action_type(action_type)
    if action_type in (ACTION_C_COMPILE, ACTION_CPP_COMPILE, ACTION_ASSEMBLE):
        return "CppCompile"
    elif action_type == ACTION_CPP_LINK:
        return "CppLink"
    elif action_type == ACTION_RUSTC:
        return "Rustc"
    return action_type


@dataclasses.dataclass(frozen=True)
class ConfiguredBazelLabel:
    """A Bazel target label with an associated --config value."""

    label: str
    config: str

    def __post_init__(self) -> None:
        _validate_bazel_config(self.config)


class GnCommandMap(dict[str, str]):
    """A type mapping GN labels to the command generating their outputs."""


class BazelCommandMap(dict[ConfiguredBazelLabel, str]):
    """A type mapping ConfiguredBazelLabel values to the command generating their outputs."""


@dataclasses.dataclass
class CompareCommandsQuery:
    """Specification for querying build commands from Ninja and/or Bazel.

    At least one of `gn` or `bazel` must be set. When `bazel` is set,
    `bazel_config` must also be set to one of VALID_BAZEL_CONFIGS.

    Attributes:
        gn: Optional GN label (e.g. "//src/foo:bar"). Required for Ninja queries.
        bazel: Optional Bazel label (e.g. "//src/foo:bar"). Required for Bazel queries.
        bazel_config: Optional Bazel config name (one of VALID_BAZEL_CONFIGS:
            "host", "fuchsia_platform", or "fuchsia_sdk"). Required when `bazel` is set.
        action_type: Action type to query (one of VALID_ACTION_TYPES). Defaults to ACTION_RUSTC.
        source: Optional source file path to match for compilation actions.
        allow_differences: Optional bool, set to True to ignore differences in final report.
    """

    gn: str = ""
    bazel: None | ConfiguredBazelLabel = None
    action_type: str = ACTION_RUSTC
    source: T.Optional[str] = None
    allow_differences: bool = False

    def __post_init__(self) -> None:
        if not self.gn and not self.bazel:
            raise ValueError(
                "CompareCommandsQuery requires at least one of 'gn' or 'bazel' to be set."
            )
        _validate_action_type(self.action_type)

    @property
    def bazel_label(self) -> str:
        """Return bazel label if provided or empty string."""
        return self.bazel.label if (self.bazel is not None) else ""

    @property
    def bazel_config(self) -> str:
        """Return bazel config if provided or empty string."""
        return self.bazel.config if (self.bazel is not None) else ""

    @classmethod
    def from_dict(cls, data: JSONObject) -> "CompareCommandsQuery":
        """Construct a CompareCommandsQuery from a dictionary.

        Supported dictionary keys:
            - "gn": Optional GN label string.
            - "bazel": Optional Bazel label string.
            - "bazel_config": Bazel configuration string (required when "bazel" is set).
              Supported values are:
                - "host": Build host artifacts.
                - "fuchsia_platform": Build Fuchsia platform artifacts.
                - "fuchsia_sdk": Build Fuchsia artifacts using the in-tree Bazel SDK.
            - "type": Optional action type string (defaults to "rustc").
              Supported values are: "rustc", "c_compile", "cpp_compile", "assemble", "cpp_link".
            - "source": Optional source file path string.
            - "allow_differences": Optional boolean (defaults to False).
        """
        gn = _validate_value(data.get("gn", ""), str, "gn")

        bazel: None | ConfiguredBazelLabel = None
        bazel_label = _validate_value(data.get("bazel", ""), str, "bazel")
        if bazel_label:
            bazel_config = _validate_value(
                data.get("bazel_config", ""), str, "bazel_config"
            )
            bazel = ConfiguredBazelLabel(bazel_label, bazel_config)

        action_type = _validate_action_type(
            _validate_value(data.get("type", ACTION_RUSTC), str, "action type")
        )

        source = data.get("source")
        if source is not None:
            _validate_value(source, str, "source")

        allow_differences = _validate_value(
            data.get("allow_differences", False), bool, "allow_differences"
        )

        return cls(
            gn=gn,
            bazel=bazel,
            action_type=action_type,
            source=source,
            allow_differences=allow_differences,
        )


def _output_in_tokens(filepath: str, tokens: list[str]) -> bool:
    """Returns True if Ninja-build-dir-relative |filepath| appears in |tokens|.

    Unlike source files (which may be specified as suffixes like 'foo.cc'),
    expected output paths from ninja_outputs.json are already relative to the
    Ninja build directory (e.g. 'obj/foo/bar.o' vs 'host_x64/obj/foo/bar.o').
    Matching by exact token (or flag value after '=' or '-o') avoids false
    positives where a default-toolchain path ('obj/...') matches a
    host-toolchain path ('host_x64/obj/...').
    """
    return any(
        t == filepath or t.endswith(f"={filepath}") or t == f"-o{filepath}"
        for t in tokens
    )


def _matches_compile_action(
    tokens: list[str],
    valid_extensions: tuple[str, ...],
    source_file: str | None = None,
    expected_outputs: list[str] | None = None,
) -> bool:
    """Check whether a compile command matches the expected compilation characteristics.

    All paths are relative to the Ninja build directory.

    A compile action must compile with '-c'. If expected_outputs is given, at least one output
    must match or appear in the command. Paths in expected_outputs are relative to the Ninja
    build directory (e.g. 'obj/foo/bar.o', as returned by the GN ninja outputs database). If
    source_file is given, that source file must be among the command tokens. Otherwise, any
    source with one of valid_extensions must be present.

    Args:
        tokens: the command line as a series of string tokens, this must include "-c"
          for actions to pass this check.

        valid_extensions: A sequence of source file extensions that must appear
          in the command. E.g. (".cc", ".cpp").

        expected_outputs: optional list of expected command outputs as paths
          relative to the Ninja build directory (e.g. 'obj/foo/bar.o').
          If provided, at least one object file from this list must be part
          of the command.

        source_file: Optional source file path. If provided, Then 'tokens' must include an
          item with this exact path, or one that ends with "/{source_file}"
          to verify it appears in the command.

        valid_extensions: a sequence of valid file extensions for this command's
          source files. Only used if source_file is not provided.

    Returns:
      True if the command matches a C/C++ compiler or assembler action.
    """
    if "-c" not in tokens:
        return False

    def _path_in_tokens(filepath: str) -> bool:
        """Returns True if |filepath| is used in command tokens."""
        return any(t == filepath or t.endswith(f"/{filepath}") for t in tokens)

    if expected_outputs:
        obj_outputs = [
            out for out in expected_outputs if out.endswith((".o", ".obj"))
        ]
        if obj_outputs and not any(
            _output_in_tokens(out, tokens) for out in obj_outputs
        ):
            return False

    if source_file:
        return _path_in_tokens(source_file)
    else:
        return any(t.endswith(valid_extensions) for t in tokens)


def _matches_action_type(
    cmd: str,
    action_type: str,
    source_file: T.Optional[str] = None,
    expected_outputs: T.Optional[list[str]] = None,
    tokens: T.Optional[list[str]] = None,
) -> bool:
    """Check whether a command matches the specified action type and criteria.

    Filtering criteria are applied consistently:
    1. action_type must match:
       - 'c_compile': contains '-c' and compiling C source (.c)
       - 'cpp_compile': contains '-c' and compiling C++ source (.cc, .cpp, .cxx, .C)
       - 'assemble': contains '-c' and compiling Assembly source (.S, .s, .asm)
       - 'cpp_link': does not contain '-c', produces target output or links objects
       - 'rustc': invokes rustc
    2. If source_file is specified, it must be matched by the command.
    3. If expected_outputs is specified:
       - For cpp_link and rustc, at least one expected output must appear in cmd.
       - For compile actions, if expected_outputs is provided, at least one output
         should appear in cmd if outputs are present.
       Note on path relativity: Paths in expected_outputs are relative to the
       Ninja build directory (e.g. 'obj/foo/bar.o' or 'host_x64/my_bin', as returned
       by gn_ninja_outputs.load_from_build_dir(...).gn_label_to_paths(...)). They
       are NOT relative to the Fuchsia source root or Bazel execroot.

    Args:
        cmd: The shell command string to inspect.
        action_type: The expected action type (one of VALID_ACTION_TYPES).
        source_file: Optional source filename to match (e.g. 'foo.cc' or 'bar.c').
        expected_outputs: Optional list of expected output file paths. Paths in
            this list are relative to the Ninja build directory (e.g. 'obj/src/main.o'
            or 'host_x64/my_bin', as returned by the GN ninja outputs database).
        tokens: Optional pre-tokenized command tokens (`shlex.split(cmd)`).

    Returns:
        True if the command matches all applicable criteria; False otherwise.
    """
    _validate_action_type(action_type)
    if tokens is None:
        tokens = shlex.split(cmd)

    if action_type == ACTION_C_COMPILE:
        if not _matches_compile_action(
            tokens,
            (".c",),
            source_file=source_file,
            expected_outputs=expected_outputs,
        ):
            return False

    elif action_type == ACTION_CPP_COMPILE:
        if not _matches_compile_action(
            tokens,
            (".cc", ".cpp", ".cxx", ".C"),
            source_file=source_file,
            expected_outputs=expected_outputs,
        ):
            return False

    elif action_type == ACTION_ASSEMBLE:
        if not _matches_compile_action(
            tokens,
            (".S", ".s", ".asm"),
            source_file=source_file,
            expected_outputs=expected_outputs,
        ):
            return False

    elif action_type == ACTION_CPP_LINK:
        # Link commands must not be compile actions (-c)
        if "-c" in tokens:
            return False
        if expected_outputs and not any(
            _output_in_tokens(out, tokens) for out in expected_outputs
        ):
            return False

    elif action_type == ACTION_RUSTC:
        if "rustc" not in cmd:
            return False
        if source_file and not any(
            t == source_file or t.endswith(f"/{source_file}") for t in tokens
        ):
            return False
        if expected_outputs and not any(
            _output_in_tokens(out, tokens) for out in expected_outputs
        ):
            return False

    return True


def query_ninja_commands(
    ninja_runner: NinjaRunner,
    queries: T.Sequence[CompareCommandsQuery],
    debug: DebugHook = None,
) -> GnCommandMap:
    """Fetch ninja build commands for multiple targets in a single Ninja invocation.

    Args:
        ninja_runner: The NinjaRunner instance to use.
        queries: Sequence of CompareCommandsQuery objects.

    Returns:
        A GnCommandMap mapping GN labels to their corresponding Ninja build commands.

    Raises:
        RuntimeError or ValueError if there is a problem.
    """
    if not queries:
        return GnCommandMap()

    database = gn_ninja_outputs.load_from_build_dir(ninja_runner.build_dir)
    if database is None:
        raise RuntimeError(
            f"Failed to load ninja outputs from {ninja_runner.build_dir}"
        )

    all_outputs_to_query: list[str] = []
    query_outputs: list[tuple[CompareCommandsQuery, list[str]]] = []

    for query in queries:
        if not query.gn:
            raise ValueError(
                "CompareCommandsQuery 'gn' label must be set for Ninja queries."
            )
        gn_label = query.gn
        action_type = query.action_type
        _validate_action_type(action_type)
        outputs = database.gn_label_to_paths(gn_label)
        if not outputs:
            raise ValueError(
                f"Could not find outputs for label {gn_label} in Ninja outputs database"
            )
        query_outputs.append((query, outputs))
        all_outputs_to_query.extend(outputs)

    # Deduplicate query outputs while preserving order
    seen_outputs: set[str] = set()
    deduped_outputs: list[str] = []
    for out in all_outputs_to_query:
        if out not in seen_outputs:
            seen_outputs.add(out)
            deduped_outputs.append(out)

    ninja_cmd = ["-t", "commands", "-s"] + deduped_outputs
    if debug:
        debug(
            f"Running batched ninja command with {len(deduped_outputs)} outputs"
        )

    ninja_cmd_output = ninja_runner.run_and_extract_output(ninja_cmd)
    commands = ninja_cmd_output.strip().splitlines()
    tokenized_commands = [(cmd, shlex.split(cmd)) for cmd in commands]

    results = GnCommandMap()
    for query, outputs in query_outputs:
        gn_label = query.gn
        action_type = query.action_type
        source_file = query.source

        matched_cmd = None
        for cmd, tokens in tokenized_commands:
            if _matches_action_type(
                cmd,
                action_type,
                source_file=source_file,
                expected_outputs=outputs,
                tokens=tokens,
            ):
                matched_cmd = cmd
                break

        if matched_cmd:
            results[gn_label] = matched_cmd

    return results


def _normalize_label(label: str) -> str:
    """Remove optional @@ and @ prefix for root workspace labels."""
    if label.startswith("@@//"):
        return label[2:]
    if label.startswith("@//"):
        return label[1:]
    return label


def _is_pic_compile_action(tokens: list[str]) -> bool:
    """Returns True if a compile command outputs a .pic.o object file.

    When the `supports_pic` feature is enabled on a Bazel C++ toolchain,
    `cc_library` and `cc_binary` register two `CppCompile` actions per source
    file in the action graph: one producing `<src>.pic.o` (with `-fPIC`, listed
    first in `aquery`) and one producing `<src>.o` (without `-fPIC`, archived
    into `lib<target>.a`).
    """
    for i, token in enumerate(tokens):
        if token == "-o" and i + 1 < len(tokens):
            return tokens[i + 1].endswith(".pic.o")
        if token.startswith(("-o=", "--output=")):
            return token.partition("=")[2].endswith(".pic.o")
        if token.startswith("-o") and len(token) > 2:
            return token[2:].endswith(".pic.o")
    return False


def query_bazel_commands(
    bazel_launcher: BazelLauncher,
    bazel_execroot: str | pathlib.Path,
    queries: T.Sequence[CompareCommandsQuery],
    read_response_files: bool = False,
    debug: DebugHook = None,
) -> BazelCommandMap:
    """Query Bazel for the command lines of target actions for multiple targets.

    Queries are grouped by their `bazel.config` value (supported values: "host",
    "fuchsia_platform", "fuchsia_sdk"), and a single Bazel invocation is performed
    per configuration.

    Args:
        bazel_launcher: The BazelLauncher instance to use.
        bazel_execroot: Path to the Bazel execroot directory.
        queries: Sequence of CompareCommandsQuery objects.
        read_response_files: Whether to read response files directly from disk instead of using queries.

    Returns:
        A BazelCommandMap mapping (Bazel label, Bazel config) pairs to their corresponding command lines.
    """
    if not queries:
        return BazelCommandMap()

    queries_by_config: dict[str, list[CompareCommandsQuery]] = {}
    for query in queries:
        if not query.bazel:
            raise ValueError(
                "CompareCommandsQuery 'bazel' label must be set for Bazel queries."
            )
        queries_by_config.setdefault(query.bazel.config, []).append(query)

    result = BazelCommandMap()
    missing_labels = []

    for bazel_config, config_queries in queries_by_config.items():
        mnemonics: list[str] = []
        seen_labels: set[str] = set()
        unique_labels: list[str] = []
        for query in config_queries:
            m = action_type_to_mnemonic(query.action_type)
            if m not in mnemonics:
                mnemonics.append(m)
            assert query.bazel  # make mypy happy
            l = _normalize_label(query.bazel.label)
            if l not in seen_labels:
                seen_labels.add(l)
                unique_labels.append(l)

        mnemonic_expr = "|".join(mnemonics)
        bazel_target = 'mnemonic("{}", {})'.format(
            mnemonic_expr, " + ".join(unique_labels)
        )
        config_args = [
            f"--config={bazel_config}",
            "--config=quiet",
            # Ensure that the labels returned by get_bazel_expanded_actions()
            # have a canonical label, which means a @@// prefix for root workspace files.
            "--consistent_labels",
        ]
        if debug:
            debug(
                f"Fetching expanded Bazel commands for targets (config={bazel_config}) using get_bazel_expanded_actions with target: {bazel_target}"
            )
        try:
            expanded_actions = bazel_build_args.get_bazel_expanded_actions(
                bazel_launcher=bazel_launcher,
                bazel_execroot=str(bazel_execroot),
                bazel_target=bazel_target,
                config_args=config_args,
                filter_mnemonics=mnemonics,
                read_response_files=read_response_files,
            )
        except Exception as e:
            raise ValueError(
                f"Failed to run bazel action expansion for labels: {e}"
            ) from e

        # Group actions by normalized target label
        target_actions_map: dict[
            str, list[bazel_build_args.ExpandedAction]
        ] = {}
        for action in expanded_actions:
            norm_target = _normalize_label(action.target)
            target_actions_map.setdefault(norm_target, []).append(action)

        for query in config_queries:
            assert query.bazel  # make mypy happy
            label = query.bazel.label
            action_type = query.action_type
            source_file = query.source

            norm_label = _normalize_label(label)
            actions = target_actions_map.get(norm_label, [])
            matched_cmd = None
            fallback_cmd = None
            expect_pic = "-shared)" in query.gn

            for action in actions:
                full_args = list(action.env_vars) + action.args
                if not full_args:
                    continue
                cmd_str = shlex.join(full_args)
                if _matches_action_type(
                    cmd_str,
                    action_type,
                    source_file=source_file,
                    tokens=full_args,
                ):
                    if action_type in (
                        ACTION_C_COMPILE,
                        ACTION_CPP_COMPILE,
                        ACTION_ASSEMBLE,
                    ):
                        if _is_pic_compile_action(full_args) == expect_pic:
                            matched_cmd = cmd_str
                            break
                        if fallback_cmd is None:
                            fallback_cmd = cmd_str
                    else:
                        matched_cmd = cmd_str
                        break

            if matched_cmd is None:
                matched_cmd = fallback_cmd

            if matched_cmd:
                result[query.bazel] = matched_cmd
            else:
                missing_labels.append(label)

    if missing_labels:
        raise ValueError(f"Could not find command for labels: {missing_labels}")

    return result


def query_ninja_and_bazel_commands(
    queries: T.Sequence[CompareCommandsQuery],
    ninja_runner: NinjaRunner,
    bazel_launcher: BazelLauncher,
    bazel_execroot: str | pathlib.Path,
    read_response_files: bool = False,
    debug: DebugHook = None,
) -> tuple[GnCommandMap, BazelCommandMap]:
    """Query both GN and Bazel commands in parallel for manifest targets.

    Args:
        queries: Sequence of CompareCommandsQuery specifications.
        ninja_runner: A NinjaRunner used to perform Ninja tool calls.
        bazel_launcher: A BazelLauncher used to perform Bazel queries.
        bazel_execroot: Path to the Bazel execroot directory.
        read_response_files: Optional flag to read response files directly from disk.

    Returns:
        A (GnCommandMap, BazelCommandMap) pair.
    """
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
        ninja_future = executor.submit(
            query_ninja_commands,
            ninja_runner,
            queries,
            debug=debug,
        )
        bazel_future = executor.submit(
            query_bazel_commands,
            bazel_launcher,
            bazel_execroot,
            queries,
            read_response_files=read_response_files,
            debug=debug,
        )
        gn_cmds: GnCommandMap = ninja_future.result()
        bazel_cmds: BazelCommandMap = bazel_future.result()

    return gn_cmds, bazel_cmds


@dataclasses.dataclass
class CompareCommandsResult:
    """Result of comparing the commands for a single CompareCommandsQuery."""

    query: CompareCommandsQuery
    error: str = ""  # non-empty if an error occurred during the comparison.
    gn_cmd_str: str = ""
    bazel_cmd_str: str = ""
    normalized_gn_args: list[str] = dataclasses.field(default_factory=list)
    normalized_bazel_args: list[str] = dataclasses.field(default_factory=list)


def compare_gn_and_bazel_commands_for(
    target_queries: list[CompareCommandsQuery],
    bazel_paths: build_utils.BazelPaths,
    read_response_files: bool = False,
    debug: DebugHook = None,
) -> list[CompareCommandsResult]:
    fuchsia_dir = bazel_paths.fuchsia_dir
    build_dir = bazel_paths.build_dir

    ninja_runner = build_utils.NinjaRunner(bazel_paths.ninja_path, build_dir)
    bazel_launcher = build_utils.BazelLauncher(bazel_paths.launcher)

    (
        gn_cmds_map,
        bazel_cmds_map,
    ) = query_ninja_and_bazel_commands(
        target_queries,
        ninja_runner,
        bazel_launcher,
        bazel_paths.execroot,
        read_response_files=read_response_files,
        debug=debug,
    )

    gn_path_normalizer = path_normalizer.GnPathNormalizer(
        fuchsia_dir, build_dir
    )
    bazel_path_normalizer = path_normalizer.BazelPathNormalizer(bazel_paths)

    result = []
    for query in target_queries:
        gn_label = query.gn
        assert query.bazel
        bazel_label = query.bazel.label
        action_type = query.action_type

        gn_cmd_raw = gn_cmds_map.get(gn_label, "")
        bazel_cmd_raw = bazel_cmds_map.get(query.bazel, "")

        if not gn_cmd_raw or not bazel_cmd_raw:
            result.append(
                CompareCommandsResult(
                    query=query,
                    error=f"Failed to get GN or Bazel command for {gn_label} vs {bazel_label}.",
                    gn_cmd_str=gn_cmd_raw,
                    bazel_cmd_str=bazel_cmd_raw,
                )
            )
            continue

        gn_cmd = shell_utils.ShellCommand(gn_cmd_raw)
        bazel_cmd = shell_utils.ShellCommand(bazel_cmd_raw)

        if action_type == ACTION_RUSTC:
            tool_candidates = ["rustc"]
        else:
            tool_candidates = ["clang++", "clang", "ld.ldd", "lld", "llvm-ar"]

        gn_tool_cmd = gn_cmd
        for tool in tool_candidates:
            tool_cmd = shell_utils.find_command_with_tool(gn_cmd.split(), tool)
            if tool_cmd:
                gn_tool_cmd = tool_cmd
                break

        bazel_tool_cmd = bazel_cmd
        for tool in tool_candidates:
            tool_cmd = shell_utils.find_command_with_tool(
                bazel_cmd.split(), tool
            )
            if tool_cmd:
                bazel_tool_cmd = tool_cmd
                break

        if action_type == ACTION_RUSTC:
            normalized_gn_args = normalize_rustc_args.normalize_rustc_cmd(
                str(gn_tool_cmd), gn_path_normalizer
            )
            normalized_bazel_args = normalize_rustc_args.normalize_rustc_cmd(
                str(bazel_tool_cmd), bazel_path_normalizer
            )
        else:
            normalized_gn_args = normalize_clang_args.normalize_clang_cmd(
                str(gn_tool_cmd), gn_path_normalizer, action_type=action_type
            )
            normalized_bazel_args = normalize_clang_args.normalize_clang_cmd(
                str(bazel_tool_cmd),
                bazel_path_normalizer,
                action_type=action_type,
            )

        result.append(
            CompareCommandsResult(
                query=query,
                gn_cmd_str=gn_cmd_raw,
                bazel_cmd_str=bazel_cmd_raw,
                normalized_gn_args=normalized_gn_args,
                normalized_bazel_args=normalized_bazel_args,
            )
        )

    return result

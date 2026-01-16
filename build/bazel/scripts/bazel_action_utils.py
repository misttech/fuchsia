# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Helper classes to deal with @gn_targets dependencies in the Fuchsia workspace."""

import dataclasses
import json
import os
import sys
import typing as T
from pathlib import Path

sys.path.insert(0, os.path.dirname(__file__))
import build_utils
import stdio_redirection
from build_utils import BazelPaths


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
    gn_targets_dir: str
    stamp_path: str
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
            gn_targets_dir = entry["gn_targets_dir"]
            stamp_path = entry["stamp_path"]
            target_info = self._targets.setdefault(
                (bazel_target, bazel_platform_label),
                BazelTargetInfo(
                    bazel_target=bazel_target,
                    bazel_platform_label=bazel_platform_label,
                    bazel_platform_config=bazel_platform_config,
                    gn_targets_dir=gn_targets_dir,
                    stamp_path=stamp_path,
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

    def get_info(
        self, target: str, platform: str | None
    ) -> BazelTargetInfo | None:
        """Retrieve BazelTargetInfo matching a given Bazel target label."""
        return self._targets.get((target, platform))


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
            FileNotFoundError if file is missing.
        """
        with (build_dir / "rbe_settings.json").open("rb") as f:
            content = json.load(f)

            # LINT.IfChange(BazelRbeSettings)
            final_settings = content["final"]
            enabled = final_settings["bazel_enable"]
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
            exec_strategy = final_settings["bazel_exec_strategy"]
            if not exec_strategy in exec_strategy_lookup_map:
                raise ValueError(
                    f"'bazel_exec_strategy' was '{exec_strategy}', but must be empty or one of: {', '.join([key for key in exec_strategy_lookup_map.keys() if key != ''])}\n\n"
                )
            if enabled and not exec_strategy:
                raise ValueError(
                    f"A 'bazel_exec_strategy' must be set when 'bazel_rbe_enabled' is true."
                )

            # LINT.ThenChange(//build/rbe/BUILD.gn:FinalRbeSettings)
            return BazelRbeSettings(
                enabled=enabled,
                exec_strategy=exec_strategy_lookup_map[exec_strategy],
            )


# LINT.IfChange(gn_targets_dir)
# Path of the @gn_targets symlink relative to the Bazel workspace directory.
GN_TARGETS_SYMLINK_PATH = "fuchsia_build_generated/gn_targets_dir"
# LINT.ThenChange(//build/bazel/toplevel.MODULE.bazel:gn_targets_dir)


def update_gn_targets_symlink(
    bazel_paths: BazelPaths,
    gn_targets_dir: Path,
    check_license_timestamp: bool = False,
) -> None:
    """Update the (singular) Bazel workspace's symlink to the per-action gn_targets workspace dir.

    This updates the one-and-only Bazel build workspace's symlink to that of the appropriate
    gn_targets directory, so that Bazel has access to the correct inputs from GN for the
    Bazel action about to run.
    """
    if check_license_timestamp:
        # LINT.IfChange(all_licenses_spdx_path)
        license_file = gn_targets_dir / "all_licenses.spdx.json"
        # LINT.ThenChange(//build/bazel/scripts/workspace_utils.py:all_licenses_spdx_path)
        license_info = os.stat(license_file)
        assert (
            license_info.st_mtime != 0
        ), f"PANIC: The timestamp of {license_file} is 0. It should have been updated by Ninja.\n"

    build_utils.force_symlink(
        bazel_paths.workspace / GN_TARGETS_SYMLINK_PATH,
        gn_targets_dir,
    )


def find_prefix_in_input(
    prefix: str | bytes, input: str | bytes
) -> tuple[int, int]:
    """Find the first occurrence of a given prefix in input.

    Args:
        prefix: A non-empty prefix string.
        input: An input string.
    Returns:
        There are three possible cases that determine the result
        of this function:

        - Full match:

          When the full prefix is found in the input, return (2, pos)
          where |pos| is the prefix's index in the input sequence.

        - Partial match:

          When the full prefix is not found in the input, but the
          input ends with a few characters from the prefix, return
          (1, pos) where |pos| is the position of the first
          potential prefix character.

        - No match:

          When the full prefix does not appear in the input, and
          the last input characters cannot possibly match the first
          characters of the prefix, return (0, len(input))

    Examples:
        ("foo", "-------") -> (0, 8)  no match
        ("foo", "--foo--") -> (2, 2)  full match
        ("foo", "-----fo") -> (1, 5)  partial match
    """
    assert type(input) == type(
        prefix
    ), f"prefix and input should be of the same time, got {type(prefix)} and {type(input)}"
    prefix_len = len(prefix)
    input_len = len(input)
    assert prefix_len > 0, f"Empty prefix is not supported"
    prefix_first_char = prefix[0]
    from_pos = 0
    while True:
        pos = input.find(prefix_first_char, from_pos)  # type: ignore
        if pos < 0:
            return (0, input_len)  # No match

        n = 1
        while True:
            if n == prefix_len:
                return (2, pos)  # Full match

            if pos + n >= input_len:
                return (1, pos)  # Partial match

            if input[pos + n] != prefix[n]:
                break

            n += 1

        from_pos = pos + n


class BazelStderrDebugLineFilter(stdio_redirection.OutputSink):
    """A OutputSink that can filter DEBUG lines from Bazel's stderr output.

    There is no way to get the path of the output file using cquery, because
    that command ignores aspect-generated providers.
    See https://github.com/bazelbuild/bazel/issues/22528

    A work-around is to use print() in the aspect's implementation rule, to
    print the execroot-related path to stderr, then ensure the caller can process
    the line to extract the file location.

    All print() statements end up as a line that looks like:

    DEBUG: <path>:<line>:<column>: <message>\r\n

    Where the 'DEBUG: ' prefix may be colored by ANSI VT Code sequences when
    stdout is a tty, in which case the output will be:

    \x1b[33mDEBUG: \x1b[0m <path>:<line>:<column>: <message>\r\n

    Moreover, when running in an interactive terminal, Bazel will prepend
    cursor-controlling VT Code sequences, so the input line would look like:

    \r\x1b[1A\x1b[K\x1b[1A\x1b[K\x1b[33mDEBUG: \x1b[0m <path>:<line>:<column>: <message>\r\n

    It is crucial to conserve the prefix commands before the colored DEBUG prefix
    to ensure that Bazel's progressive status updates are maintained properly, even
    if the line if filtered out from the final output.

    The point of this class is to detect such DEBUG lines, and pass them to
    a user-provided filtering function, which may extract information from it,
    and will return True to indicate that the line should be omitted from the
    actual output visible to the end user.

    An example usage would be the following:

        # Assume that an aspect uses print("MY_DATA=<some_data>")

        extracted_data = []

        def my_data_line_filter(line: bytes) -> bool:
            # Filter debug line. This always begin with a DEBUG prefix,
            # potentially colored, but without any cursor-control VT
            # sequences before that, and typically ends with \r\n.
            data_prefix = 'MY_DATA='
            pos = line.find(data_prefix)
            if pos < 0:
                return False   # keep this line
            extracted_data.append(line[pos + len(data_prefix):].decode("utf-8").strip())
            return True  # Skip this line

        # Run Bazel command through a pipe or pty, while sending filtered output
        # to the original stderr.

        filter_sink = BazelStderrDebugLineFilter(
            stdio_redirection.StderrOutputSink(),
            my_data_line_filter
        )

        use_pty = os.isatty(sys.stderr.fileno())
        with stdio_redirection.PipeOutputSink(filter_sink, use_pty) as stderr_sink:
            subprocess.run([..bazel.command.args], check=True, stderr=stderr_sink.get_write_fd())

        ... extracted_data will contain the extracted data here
    """

    # The line prefix used when running in an interactive terminal.
    DEBUG_PREFIX_COLORED = b"\x1b[33mDEBUG: \x1b[0m"

    # The line prefix when running in a non-interactive terminal.
    DEBUG_PREFIX = b"DEBUG: "

    def __init__(
        self,
        output: stdio_redirection.OutputSink,
        debug_line_filter: T.Callable[[bytes], bool] = lambda x: False,
    ) -> None:
        """Create instance.

        Args:
            output: The final OutputSink that will receive filtered output.
            debug_line_filter: A optional callable that receives a single DEBUG line,
                potentially newline terminated, and return True to indicate that it should
                be omitted from the output, or False to keep it. By default all lines
                are kept.
        """
        self._output = output
        self._debug_line_filter = debug_line_filter
        # Buffered data that was not processed yet due to insufficient data.
        self._buffer = b""
        # This will be non-empty if the buffer starts with one recognized prefix.
        self._prefix_start = b""

    def write(self, data: bytes) -> bool:
        while True:
            if self._buffer:
                data = self._buffer + data
                self._buffer = b""

            if not data:
                return True

            if self._prefix_start:
                assert data.startswith(
                    self._prefix_start
                ), f"Unexpected data (expected initial {self._prefix_start!r}): {data!r}"
                next_newline = data.find(10, len(self._prefix_start))
                if next_newline < 0:
                    # Not enough data yet, just store in buffer then wait.
                    self._buffer = data
                    return True

                if not self._debug_line_filter(data[0 : next_newline + 1]):
                    # Line is not filtered, send it directly then loop with the rest.
                    if not self._output.write(data[0 : next_newline + 1]):
                        return True

                self._prefix_start = b""
                data = data[next_newline + 1 :]
                continue
            else:
                colored_match, colored_pos = find_prefix_in_input(
                    self.DEBUG_PREFIX_COLORED, data
                )
                regular_match, regular_pos = find_prefix_in_input(
                    self.DEBUG_PREFIX, data
                )

                pos = min(colored_pos, regular_pos)
                if pos > 0:
                    # There are characters before the first prefix, send them directly then loop.
                    if not self._output.write(data[0:pos]):
                        return False
                    data = data[pos:]
                    continue

                if colored_match == 2:
                    assert colored_pos == 0
                    self._prefix_start = self.DEBUG_PREFIX_COLORED
                    continue

                if regular_match == 2:
                    assert regular_pos == 0
                    self._prefix_start = self.DEBUG_PREFIX
                    continue

                # Partial matches only, store data in buffer then exit.
                self._buffer = data
                return True

    def close(self) -> None:
        if self._buffer:
            self._output.write(self._buffer)
            self._buffer = b""

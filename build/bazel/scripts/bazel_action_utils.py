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


@dataclasses.dataclass
class BazelBuildActionInfo(object):
    """Model an entry in the bazel_build_action_targets.json file.

    See //BUILD.gn for schema description.
    """

    # LINT.IfChange(bazel_build_actions)
    gn_target: str
    bazel_targets: list[str]
    no_sdk: bool
    gn_targets_dir: str
    gn_targets_manifest: str
    gn_targets_licenses_spdx: str
    debug_symbols_manifest: str
    bazel_command_file: str = ""
    bazel_compdb_file: str = ""
    bazel_rust_project_json: str = ""
    path_mapping: str = ""
    timings_file: str = ""
    build_events_log_json: str = ""
    # LINT.ThenChange(//BUILD.gn:bazel_build_actions)


class BazelBuildActionsMap(object):
    """A class used to model a map of Bazel top-level targets to the GN bazel_action() labels that builds them.

    This is built from the content of the //:bazel_build_action_targets generated_file() output.
    This does not provide information about the dependencies of the top-level Bazel actions,
    for these, an actual Bazel query is needed, see BazelBuildActionQuery below.
    """

    # LINT.IfChange(gn_targets_dir)
    # Path of the @gn_targets symlink relative to the Bazel workspace directory.
    GN_TARGETS_SYMLINK_PATH = "fuchsia_build_generated/gn_targets_dir"
    # LINT.ThenChange(//build/bazel/toplevel.MODULE.bazel:gn_targets_dir)

    def __init__(self, json_content: list[dict[str, T.Any]]) -> None:
        self._targets: dict[str, BazelBuildActionInfo] = {}
        self._bazel_to_gn: dict[str, str] = {}
        for entry in json_content:
            info = BazelBuildActionInfo(**entry)
            self._targets[info.gn_target] = info
            for bazel_target in info.bazel_targets:
                self._bazel_to_gn[bazel_target] = info.gn_target

    @staticmethod
    def create_from_build_dir(build_dir: Path) -> "BazelBuildActionsMap":
        """Create instance from content of Ninja build directory.

        Args:
            build_dir: Ninja build directory, populated by `fx gen`.
        Returns:
            New BazelBuildActionsMap
        Raises:
            FileNotFoundError if file is missing.
        """
        with (build_dir / "bazel_build_action_targets.json").open("rb") as f:
            content = json.load(f)
        return BazelBuildActionsMap(content)

    @property
    def bazel_targets(self) -> list[str]:
        return sorted(self._bazel_to_gn.keys())

    @property
    def gn_targets(self) -> list[str]:
        return sorted(set(self._bazel_to_gn.values()))

    def label_items(self) -> T.Iterable[tuple[str, BazelBuildActionInfo]]:
        return self._targets.items()

    def get_info(self, gn_label: str) -> T.Optional[BazelBuildActionInfo]:
        """Retrieve BazelBuildActionInfo matching a given GN label."""
        return self._targets.get(gn_label)

    def find_gn_target_for(self, bazel_target: str) -> str:
        """Retrieve the GN target label used to build a given top-level Bazel target.

        Args:
            bazel_target: A top-level Bazel target that is built by one of
               the GN bazel_action() targets.

        Returns:
            The corresponding GN target label, or an empty string if there is no match.
        """
        return self._bazel_to_gn.get(bazel_target, "")

    def update_gn_targets_symlink(
        self,
        gn_label: str,
        bazel_paths: build_utils.BazelPaths,
        check_license_timestamp: bool = False,
    ) -> Path:
        """Update the @gn_targets symlink for a given bazel_action() GN target.

        Args:
           gn_label: GN label of the bazel_action() target.

           bazel_paths: A BazelPaths instance used to locate the workspace and build directory.

           check_licenses_timestamp: Optional boolean flag. Set it to perform a
               consistency check verifying that the timestamp of the license file in the
               directory is not 0. Raise an AssertionError otherwise.

        Returns:
           None if the GN label does not match a known bazel_action() target reachable from
           //:bazel_build_action_targets. Otherwise, the symlink's target after the update is
           complete.

        Raises:
           ValueError if the GN label does not match a bazel_action() target reachable from
           //:bazel_build_action_targets.

           AssertionError if the @gn_targets directory does not exist. This corresponds to
           a bug in the Fuchsia build rules.

           AssertionError if check_licenses_timestamp is True and the timestamp of
           @gn_targets//:all_licenses.spdx is 0. This corresponds to a bug in the Fuchsia
           build rules.
        """
        info = self._targets.get(gn_label)
        if not info:
            raise ValueError(
                f"The GN label {gn_label} is not reachable from //:bazel_build_action_targets\n\n"
                + "To fix this, ensure that the target is reachable from //:default, //:root_targets or listed in\n\n"
                "the 'extra_bazel_build_action_labels' build configuration variable.\n"
            )

        gn_targets_dir = bazel_paths.ninja_build_dir / info.gn_targets_dir
        assert (
            gn_targets_dir.exists()
        ), f"Missing @gn_targets_dir for {gn_label}: {gn_targets_dir}"

        if check_license_timestamp:
            # LINT.IfChange(all_licenses_spdx_path)
            license_file = gn_targets_dir / "all_licenses.spdx.json"
            # LINT.ThenChange(//build/bazel/scripts/workspace_utils.py:all_licenses_spdx_path)
            license_info = os.stat(license_file)
            assert (
                license_info.st_mtime != 0
            ), f"PANIC: The timestamp of {license_file} is 0. It should have been updated by Ninja.\n"

        build_utils.force_symlink(
            bazel_paths.workspace / self.GN_TARGETS_SYMLINK_PATH,
            gn_targets_dir,
        )
        return gn_targets_dir


class BazelBuildActionQuery(object):
    """Convenience class to wrap Bazel query operations.

    Useful when trying to find the wrapping GN bazel_action() target for a
    given Bazel target, GN target, or in case of failure, a message explaining its reason.
    """

    def __init__(
        self, bazel_target: str, actions_map: BazelBuildActionsMap
    ) -> None:
        """Create instance.

        Args:
            bazel_target: Bazel target label.
            actions_map: A BazelBuildActionsMap instance.
        """
        self._bazel_target = bazel_target
        self._actions_map = actions_map

    def make_query_command(self, bazel_pre_cmd_args: list[str]) -> list[str]:
        """Return the query command to be performed.

        Note that this forces @gn_targets to be empty, which will generate errors
        that are ignored through the use of --keep_going. The error output *must*
        be filtered with filter_query_errors() to detect real errors, before
        calling process_query_output().

        Args:
            bazel_pre_cmd_args: List of command-line arguments that appear before the 'query'
               command. At a minimum, this would be a list with a single item with the path
               of the Bazel binary / launcher script.
        Returns:
            A string list representing a command to be passed to a function
            like subprocess.run().
        """
        query_expr = "allpaths(set(%s), %s)" % (
            " ".join(self._actions_map.bazel_targets),
            self._bazel_target,
        )

        return bazel_pre_cmd_args + [
            "query",
            "--config=no_gn_targets",
            "--config=quiet",
            "--keep_going",
            query_expr,
        ]

    @staticmethod
    def filter_query_errors(errors: str) -> str:
        """Filter the errors generated by the invocation of a make_query_command() result.

        This removes all errors that are due to the empty @gn_targets repository,
        but leaves any others, to catch unexpected breakages due to developer changes.

        Args:
            errors: The query's command stderr output as a string.
        Returns:
            The error output without expected errors related to  @gn_targets.
            A non-empty value means that real errors were detected during the query.
        """
        real_errors = []
        for error in errors.splitlines():
            if (
                error.startswith("ERROR: ")
                and "no such package '@@gn_targets+//" in error
            ):
                continue
            if error.startswith(
                "Starting local Bazel server and connecting to it..."
            ):
                continue
            if error.startswith('ERROR: Evaluation of query "allpaths(set('):
                continue
            if error.startswith(
                "WARNING: --keep_going specified, ignoring errors."
            ):
                continue
            real_errors.append(error)

        return "\n".join(real_errors)

    def process_query_output(self, query_result: str) -> list[str]:
        """Parse the result of a Bazel query to get a list of GN  bazel_action() labels.

        Args:
            query_result: The stdout of running the Bazel cquery command generated
                by make_query_command() as a string.
        Returns:
            A list of GN target strings, each pointing to a target definition
            using bazel_action() or one of its wrappers, that depend on this
            instance's bazel target.
        """
        lines = query_result.splitlines()
        gn_targets = set()

        for line in lines:
            gn_target = self._actions_map.find_gn_target_for(line)
            if gn_target:
                gn_targets.add(gn_target)

        return sorted(gn_targets)


def find_gn_bazel_action_infos_for(
    bazel_target: str,
    actions_map: BazelBuildActionsMap,
    bazel_launcher: build_utils.BazelLauncher,
    log: T.Optional[build_utils.LogFunc] = None,
    log_err: T.Optional[build_utils.LogFunc] = None,
) -> list[BazelBuildActionInfo]:
    """Find the BazelBuildActionInfo instances matching a given Bazel target.

    Find all GN bazel_action() target whose top-level bazel targets
    depend on a given |bazel_target|, and return the corresponding
    BazelBuildActionInfo items in a list.

    This is the main logic around `fx bazel-tool set_gn_targets`.

    Args:
        bazel_target: Bazel target label to look for. Must start
           with // or @.

        actions_map: A BazelBuildActionsMap instance used as input.

        bazel_launcher: A build_utils.BazelLauncher instance.

        log: Optional LogFunc instance to log individual steps.
        log_err: Optional LogFunc instance to log error messages.

    Returns:
        A list of BazelBuildActionInfo items. In case of failure,
        errors will be sent to |log_err| and the function will return
        an empty list.

        An empty list without errors means the target is not a known
        dependency of the actions_map's GN and Bazel targets.

    Raises:
        AssertionError if |bazel_target| has invalid format.
    """
    # Check inputs.
    if not bazel_target.startswith(("@", "//")):
        if log_err:
            log_err(f"Target label must start with // or @: {bazel_target}")
        return []

    if "(" in bazel_target:
        if log_err:
            log_err(
                f"Target label cannot include GN toolchain suffix: {bazel_target}"
            )
        return []

    gn_target = actions_map.find_gn_target_for(bazel_target)
    if gn_target:
        if log:
            log(
                f"Bazel target {bazel_target} maps directly to GN target {gn_target}"
            )
        info = actions_map.get_info(gn_target)
        assert info  # Appease mypy
        return [info]

    action_query = BazelBuildActionQuery(bazel_target, actions_map)
    query_command = action_query.make_query_command([])
    ret = bazel_launcher.run_bazel_command(
        query_command, **bazel_launcher.CAPTURE_KWARGS
    )
    if ret.returncode != 0:
        ret.stderr = action_query.filter_query_errors(ret.stderr)
        if ret.stderr:
            # Report unexpected errors directly.
            if log_err:
                log_err(
                    f"Bazel query returned unexpected errors:\n%s\n"
                    % ret.stderr
                )
            return []

    gn_targets = action_query.process_query_output(ret.stdout)
    if log:
        log(f"Bazel query result: {gn_targets}")

    return [
        info
        for gn_target, info in actions_map.label_items()
        if gn_target in gn_targets
    ]


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
        pos = input.find(prefix_first_char, from_pos)
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
                ), f"Unexpected data (expected initial {self._prefix_start}): {data}"
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

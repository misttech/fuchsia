#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Collect Ninja trace information for analysis in chrome://tracing."""

import argparse
import dataclasses
import gzip
import json
import os
import subprocess
from concurrent import futures
from pathlib import Path
from typing import Any, Iterable

JsonTrace = dict[str, Any]

NINJA_BUILD_TRACE_BASENAME = "ninja_build_trace.json.gz"
COMPDB_BASENAME = "compdb.json"
GRAPH_BASENAME = "graph.dot"
NINJATRACE_BASENAME = "ninjatrace.json.gz"
NINJA_SUBBUILDS_JSON = "ninja_subbuilds.json"


def _subbuild_ninja_target(build_dir: Path) -> str:
    """Returns the name of the ninja target from the main build, based on the
    subbuild directory."""
    # LINT.IfChange
    return str(build_dir.name) + ".stamp"
    # LINT.ThenChange(//build/subbuild.gni)


def load_compressed_trace(trace_path: Path) -> list[JsonTrace]:
    with gzip.open(trace_path) as f:
        return json.load(f)


def write_compressed_trace(
    trace_path: Path, trace_data: list[JsonTrace]
) -> None:
    with gzip.open(trace_path, "wt") as f:
        json.dump(trace_data, f)


@dataclasses.dataclass
class Tracer:
    """Helper class that closes over some static configuration."""

    ninja_path: Path
    ninjatrace_path: Path
    rbe_rpl_path: Path
    rpl2trace_path: Path
    save_temps: bool

    def trace_build_dir(
        self,
        build_dir: Path,
    ) -> Path:
        """Generate the Ninja trace for a single build directory (either the
        main build or a subbuild).

        The resulting trace will be written to `build_dir / ninjatrace.json`,
        but will also be parsed and returned."""
        ninja_build_trace = build_dir / NINJA_BUILD_TRACE_BASENAME
        trace = build_dir / NINJATRACE_BASENAME

        ninjatrace_args: list[str | os.PathLike[str]] = [
            self.ninjatrace_path,
            "-ninjabuildtrace",
            ninja_build_trace,
            "-trace-json",
            trace,
            "-critical-path",
        ]

        if self.rbe_rpl_path:
            ninjatrace_args.extend(
                [
                    "-rbe-rpl-path",
                    self.rbe_rpl_path,
                    "-rpl2trace-path",
                    self.rpl2trace_path,
                ]
            )
        subprocess.run(ninjatrace_args, check=True)

        return trace

    def find_and_merge_subbuilds(self, main_build_dir: Path) -> None:
        """Given the main build dir and its trace, find any subbuilds referenced
        by that trace, build them, load them, and merge them into a single
        trace file."""

        # ninja_subbuilds.json is a build API module listing the set of
        # directories that _could_ contain subbuilds, but those subbuilds may
        # not have actually run as part of the last build.
        with (main_build_dir / NINJA_SUBBUILDS_JSON).open() as f:
            possible_subbuild_dirs = [
                Path(b["build_dir"]) for b in json.load(f)
            ]

        # If there aren't any subbuilds possible, exit early so we don't spend the time
        # to read in the main build trace and write it back out again.
        if not possible_subbuild_dirs:
            return

        # Load the main build traces
        main_build_traces = load_compressed_trace(
            main_build_dir / NINJATRACE_BASENAME
        )

        traces_by_name = {t["name"]: t for t in main_build_traces}

        # Filter out subbuild dirs that aren't referenced by the main trace
        # file.
        filtered_subbuild_dirs = [
            b
            for b in possible_subbuild_dirs
            if _subbuild_ninja_target(b) in traces_by_name
        ]

        # If there weren't any subbuilds in the last build, then exit early, leaving the
        # existing trace file as-is:
        if not filtered_subbuild_dirs:
            return

        # Generate traces for the subbuild dirs in parallel
        pool = futures.ThreadPoolExecutor()
        subbuild_dir_and_traces: Iterable[
            tuple[Path, list[JsonTrace]]
        ] = pool.map(
            lambda subbuild_dir: (
                subbuild_dir,
                # Subbuilds don't need extra targets (as of this writing).
                load_compressed_trace(
                    self.trace_build_dir(
                        build_dir=main_build_dir / subbuild_dir
                    )
                ),
            ),
            filtered_subbuild_dirs,
        )

        merged_traces = [*main_build_traces]
        for subbuild_dir, subbuild_traces in subbuild_dir_and_traces:
            target_in_main_build_trace = traces_by_name[
                _subbuild_ninja_target(subbuild_dir)
            ]
            assert target_in_main_build_trace, (
                "We already filtered out subbuilds not mentioned in the top-level build: %s"
                % subbuild_dir
            )
            for t in subbuild_traces:
                merged_traces += [
                    {
                        **t,
                        # Rewrite the trace to set "pid" to indicate the subbuild.
                        "pid": subbuild_dir.name,
                        # And offset the time by the start time of the subbuild
                        # action in the main build./
                        "ts": t["ts"] + target_in_main_build_trace["ts"],
                    }
                ]

        write_compressed_trace(
            main_build_dir / NINJATRACE_BASENAME, merged_traces
        )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--extra-ninja-targets",
        nargs="*",
        help="""\
If you ran a full `fx build`, ignore this flag. If you built a specific set of
ninja targets (e.g. `fx build my_target other_target`), some of which aren't
depended on `//:default`, list those targets here. Otherwise they might not show
up in the resulting traces.""",
    )
    parser.add_argument(
        "--save-temps",
        action="store_true",
        help="""\
if set, keep the intermediate compdb.json and graph.dot files
in each build directory.  The are only needed temporarily to produce the
final ninjatrace.json, and can be large at O(100)s of MBs.""",
    )
    parser.add_argument(
        "--fuchsia-build-dir",
        type=Path,
        required=True,
        help="Path to the Fuchsia build directory.",
    )
    parser.add_argument(
        "--ninja-path",
        type=Path,
        required=True,
        help="Path to the prebuilt ninja binary.",
    )
    parser.add_argument(
        "--ninjatrace-path",
        type=Path,
        required=True,
        help="Path to the prebuilt ninjatrace binary.",
    )
    parser.add_argument(
        "--rbe-rpl-path",
        help="when provided, interleave remote execution stats from RBE into the main trace",
    )
    parser.add_argument(
        "--rpl2trace-path",
        type=Path,
        help="Path to the prebuilt rpl2trace tool.",
    )
    parser.add_argument(
        "--subbuilds-in-place",
        action="store_true",
        help="""\
If set, merge traces from subbuilds with traces from the main build and
include these traces in the main build's ninjatrace.json file. Must not be
specified if --subbuilds-output-path is set.""",
    )
    args = parser.parse_args()

    tracer = Tracer(
        ninja_path=args.ninja_path,
        ninjatrace_path=args.ninjatrace_path,
        rbe_rpl_path=args.rbe_rpl_path,
        rpl2trace_path=args.rpl2trace_path,
        save_temps=args.save_temps,
    )

    fuchsia_build_dir = args.fuchsia_build_dir

    # Convert the trace for the main build
    outpath = tracer.trace_build_dir(fuchsia_build_dir)

    if args.subbuilds_in_place:
        tracer.find_and_merge_subbuilds(fuchsia_build_dir)

    print(f"Now visit chrome://tracing and load {str(outpath)}")


if __name__ == "__main__":
    main()

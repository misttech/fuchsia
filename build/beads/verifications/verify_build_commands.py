#!/usr/bin/env fuchsia-vendored-python

# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""
Verifies build commands for a list of GN and Bazel target pairs defined in a manifest file.
"""

import argparse
import dataclasses
import json
import pathlib
import sys
import typing as T

_DEBUG = False

from build.bazel.scripts import build_utils
from build.beads.scripts import compare_utils, flags_differences
from build.beads.scripts.compare_utils import CompareCommandsResult

_FUCHSIA_DIR = pathlib.Path(__file__).parent.parent.parent.parent

BEADS_TOOLCHAINS_FILENAME = "beads_toolchains.json"


def debug(s: T.Any) -> None:
    if _DEBUG:
        print(f"DEBUG: {s}", file=sys.stderr)


@dataclasses.dataclass
class VerificationRunResult:
    """Result of verifying build commands for a single build directory."""

    build_dir: pathlib.Path
    all_success: bool
    differences_count: int
    unexpected_differences_count: int
    errors: list[str]
    report_text: str
    error_report_text: str
    results: list[CompareCommandsResult]


def resolve_gn_label_for_toolchains(
    gn_label: str,
    toolchains_info: dict[str, str],
) -> str:
    """Rewrite a GN label from the manifest to match the active build's toolchains.

    In variant builds (e.g. ASan, LTO, ThinLTO), Fuchsia and Host targets are
    instantiated in variant-specific toolchains (recorded in beads_toolchains.json)
    rather than the base default_toolchain or host_x64 toolchain.
    """
    if not gn_label or not toolchains_info:
        return gn_label

    default_tc = toolchains_info.get("default_toolchain", "")
    fuchsia_tc = toolchains_info.get("fuchsia_toolchain", "")
    host_tc = toolchains_info.get("host_toolchain", "")
    host_variant_tc = toolchains_info.get("host_variant_toolchain", "")

    if "(" not in gn_label:
        if fuchsia_tc and fuchsia_tc != default_tc:
            return f"{gn_label}({fuchsia_tc})"
        return gn_label

    base_label, _, tc_part = gn_label.partition("(")
    tc = tc_part.rstrip(")")
    if tc == host_tc or tc in (
        "//build/toolchain:host_x64",
        "//build/toolchain:host_arm64",
    ):
        target_host_tc = host_variant_tc or host_tc
        if target_host_tc and target_host_tc != tc:
            return f"{base_label}({target_host_tc})"
    return gn_label


def resolve_queries_for_build_dir(
    target_queries: T.Sequence[compare_utils.CompareCommandsQuery],
    build_dir: pathlib.Path,
) -> list[compare_utils.CompareCommandsQuery]:
    """Resolve GN labels in target queries using beads_toolchains.json if present."""
    toolchains_file = build_dir / BEADS_TOOLCHAINS_FILENAME
    if not toolchains_file.exists():
        return list(target_queries)

    try:
        toolchains_info = json.loads(toolchains_file.read_text())
    except (OSError, json.JSONDecodeError):
        return list(target_queries)

    resolved: list[compare_utils.CompareCommandsQuery] = []
    for q in target_queries:
        new_gn = resolve_gn_label_for_toolchains(q.gn, toolchains_info)
        if new_gn != q.gn:
            resolved.append(dataclasses.replace(q, gn=new_gn))
        else:
            resolved.append(q)
    return resolved


def summarize_comparison_results(
    build_dir: pathlib.Path,
    results: list[CompareCommandsResult],
    debug_fn: T.Callable[[T.Any], None] = debug,
) -> VerificationRunResult:
    """Aggregate a list of CompareCommandsResult into a VerificationRunResult."""
    all_success = True
    differences_count = 0
    unexpected_differences_count = 0
    errors: list[str] = []

    report_text = ""
    error_report_text = ""

    for result in results:
        if result.error:
            errors.append(result.error)
            all_success = False
            continue

        normalized_gn_args = result.normalized_gn_args
        normalized_bazel_args = result.normalized_bazel_args

        debug_fn(f"GN normalized command:\n{normalized_gn_args}\n")
        debug_fn(f"Bazel normalized command:\n{normalized_bazel_args}\n")

        gn_label = result.query.gn
        bazel_label = result.query.bazel_label
        bazel_config = result.query.bazel_config
        action_type = result.query.action_type
        description = f"{gn_label} vs {bazel_label} ({bazel_config}), action type {action_type}"

        differences = flags_differences.FlagsDifferences.new_from_lists(
            normalized_gn_args, normalized_bazel_args
        )

        if differences.has_differences:
            debug_fn(f"Mismatch for {description}")
            differences_count += 1

            flag_categorizer = (
                flags_differences.categorize_rust_flag
                if action_type == compare_utils.ACTION_RUSTC
                else flags_differences.categorize_clang_flag
            )

            summary = differences.generate_summary(
                title=description, flag_categorizer=flag_categorizer
            )
            report_text += "\n\n" + summary
            if not result.query.allow_differences:
                unexpected_differences_count += 1
                error_report_text += "\n\n" + summary
                all_success = False
        else:
            debug_fn(f"Match for {description}")

    debug_fn(f"Found {differences_count} targets with differences.")

    return VerificationRunResult(
        build_dir=build_dir,
        all_success=all_success,
        differences_count=differences_count,
        unexpected_differences_count=unexpected_differences_count,
        errors=errors,
        report_text=report_text,
        error_report_text=error_report_text,
        results=results,
    )


def verify_build_commands_for_build_dir(
    fuchsia_dir: pathlib.Path,
    build_dir: pathlib.Path,
    target_queries: T.Sequence[compare_utils.CompareCommandsQuery],
    read_response_files: bool = False,
    debug_fn: T.Callable[[T.Any], None] = debug,
) -> VerificationRunResult:
    """Run GN and Bazel command comparison for a single configured build directory."""
    resolved_queries = resolve_queries_for_build_dir(target_queries, build_dir)
    bazel_paths = build_utils.BazelPaths(fuchsia_dir, build_dir)

    results: list[
        CompareCommandsResult
    ] = compare_utils.compare_gn_and_bazel_commands_for(
        resolved_queries,
        bazel_paths,
        read_response_files=read_response_files,
        debug=debug_fn,
    )

    return summarize_comparison_results(build_dir, results, debug_fn=debug_fn)


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Verify GN and Bazel build commands for a list of targets."
    )

    build_utils.BuildPaths.add_parser_arguments(parser)

    parser.add_argument(
        "--manifest",
        required=True,
        type=pathlib.Path,
        help="Path to the manifest file (JSON)",
    )
    parser.add_argument(
        "--read_response_files",
        action="store_true",
        default=False,
        help="Read response files directly from the Bazel execroot instead of querying Starlark providers.",
    )
    parser.add_argument(
        "--verbose",
        "-v",
        action="store_true",
        default=False,
        help="Print verbose output",
    )
    parser.add_argument(
        "--report",
        type=pathlib.Path,
        help="Write detailed differences to report file",
    )
    parser.add_argument("--stamp", type=pathlib.Path, help="Stamp file path")

    args = parser.parse_args()

    global _DEBUG
    _DEBUG = args.verbose

    try:
        paths = build_utils.BuildPaths.from_parser_args(args)
    except ValueError as e:
        parser.error(str(e))

    with open(args.manifest) as f:
        targets = json.load(f)

    target_queries = [
        compare_utils.CompareCommandsQuery.from_dict(t) for t in targets
    ]

    if not target_queries:
        print("No targets to compare.")
        return 0

    debug(f"Fuchsia Dir: {paths.fuchsia_dir}")
    debug(f"Build Dir: {paths.build_dir}")
    debug(f"Manifest Path: {args.manifest}")
    debug(f"Target queries:")
    for target_query in target_queries:
        debug(f"  {str(target_query)}")

    run_result = verify_build_commands_for_build_dir(
        fuchsia_dir=paths.fuchsia_dir,
        build_dir=paths.build_dir,
        target_queries=target_queries,
        read_response_files=args.read_response_files,
        debug_fn=debug,
    )

    for err in run_result.errors:
        print(err, file=sys.stderr)

    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(run_result.report_text)

    if run_result.error_report_text:
        print(
            f"ERROR: Unexpected build command differences found:{run_result.error_report_text}",
            file=sys.stderr,
        )

    if run_result.all_success and args.stamp:
        with open(args.stamp, "w") as f:
            f.write("")

    return 0 if run_result.all_success else 1


if __name__ == "__main__":
    sys.exit(main())

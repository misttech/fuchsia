#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""
Extracts ODPM power metrics from a trace file (FXT or JSON) into fuchsiaperf.json format.
"""

import argparse
import logging
import pathlib

from reporting import metrics
from trace_processing import trace_importing, trace_model
from trace_processing.metrics import odpm_power

logging.basicConfig(level=logging.INFO)


def main() -> None:
    """Takes in a trace file (FXT or JSON) and writes ODPM power metrics in fuchsiaperf.json format."""
    parser = argparse.ArgumentParser(
        description="Extract ODPM power metrics from a Fuchsia trace file."
    )
    parser.add_argument(
        "path_to_trace",
        type=str,
        help="Path to input trace file (.fxt or .json).",
    )
    parser.add_argument(
        "output_path",
        type=str,
        nargs="?",
        help="Path to output file (must end with .fuchsiaperf.json unless --list-rails is used).",
    )
    rail_group = parser.add_mutually_exclusive_group(required=False)
    rail_group.add_argument(
        "--rails",
        nargs="+",
        type=str,
        help="List of ODPM rail names to extract metrics for (e.g. cpu_big gpu).",
    )
    rail_group.add_argument(
        "--all-rails",
        action="store_true",
        help="Extract metrics for all ODPM rails present in the trace.",
    )
    rail_group.add_argument(
        "--list-rails",
        action="store_true",
        help="List all ODPM rails available in the trace and exit.",
    )
    parser.add_argument(
        "--sum-rails",
        action="append",
        metavar="NAME=RAIL1,RAIL2,...",
        help=(
            "Compute summed power metrics across a specified list of rails "
            "(format: NAME=RAIL1,RAIL2,...; produces *_<NAME> metrics). "
            "Can be specified multiple times."
        ),
    )
    parser.add_argument(
        "--test-suite",
        type=str,
        default="Manual",
        help="Test suite name to embed in fuchsiaperf.json (default: Manual).",
    )
    parser.add_argument(
        "--aggregates-only",
        action="store_true",
        help=(
            "Emit single duration-weighted PowerAverage_<suffix> metrics "
            "instead of raw sample series."
        ),
    )
    args = parser.parse_args()

    if not (args.rails or args.all_rails or args.list_rails or args.sum_rails):
        parser.error(
            "one of the arguments --rails --all-rails --list-rails --sum-rails is required"
        )
    if args.list_rails and args.sum_rails:
        parser.error(
            "argument --sum-rails: not allowed with argument --list-rails"
        )
    if not args.list_rails and not args.output_path:
        parser.error(
            "output_path is required unless --list-rails is specified."
        )
    if (
        args.output_path
        and not args.list_rails
        and not args.output_path.endswith(".fuchsiaperf.json")
    ):
        parser.error("output_path must end with .fuchsiaperf.json")

    sum_rails: dict[str, list[str]] = {}
    for spec in args.sum_rails or []:
        if "=" not in spec:
            parser.error(
                f"invalid --sum-rails value '{spec}': expected format NAME=RAIL1,RAIL2,..."
            )
        name, rails_csv = spec.split("=", 1)
        rail_list = [r.strip() for r in rails_csv.split(",") if r.strip()]
        if not name or not rail_list:
            parser.error(
                f"invalid --sum-rails value '{spec}': expected non-empty NAME and at least one rail"
            )
        sum_rails[name] = rail_list

    processor = odpm_power.OdpmPowerMetricsProcessor(
        rails=args.rails or (),
        sum_rails=sum_rails,
        all_rails=args.all_rails or args.list_rails,
        aggregates_only=args.aggregates_only,
    )

    if args.path_to_trace.endswith(".json"):
        path_to_trace_json = args.path_to_trace
    elif args.path_to_trace.endswith(".fxt"):
        path_to_trace_json = trace_importing.convert_trace_file_to_json(
            trace_path=args.path_to_trace,
            patterns=processor.event_patterns,
            categories=processor.category_names,
        )
    else:
        raise ValueError("Trace file must be in either .fxt or .json format")

    model: trace_model.Model = trace_importing.create_model_from_file_path(
        path_to_trace_json
    )

    if args.list_rails:
        available = odpm_power.OdpmPowerMetricsProcessor.list_rails(model)
        print(f"Available ODPM rails ({len(available)}):")
        for rail in available:
            print(f"  {rail}")
        return

    trace_results = processor.process_metrics(model)

    metrics.TestCaseResult.write_fuchsiaperf_json(
        results=trace_results,
        test_suite=args.test_suite,
        output_path=pathlib.Path(args.output_path),
    )


if __name__ == "__main__":
    main()

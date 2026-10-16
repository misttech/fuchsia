#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Formats JSON files using standard indentation and sorted keys."""

import argparse
import json
import os
import shutil
import sys
import tempfile
from pathlib import Path


def _write_results(
    results: list[dict[str, object]], output: Path | None
) -> None:
    # The results include the full formatted contents of each unformatted file,
    # which can exceed shac's limit on the size of a subprocess's stdout.
    if output:
        output.write_text(json.dumps(results), encoding="utf-8")
    else:
        print(json.dumps(results))


def main():
    parser = argparse.ArgumentParser(description="Pretty-print JSON files.")
    parser.add_argument(
        "file",
        type=Path,
        nargs="+",
        help="JSON file to format in-place.",
    )
    parser.add_argument(
        "--sort-keys",
        default=True,
        action=argparse.BooleanOptionalAction,
        dest="sort_keys",
        help="Sort object keys.",
    )
    parser.add_argument(
        "--quiet",
        default=False,
        action=argparse.BooleanOptionalAction,
        dest="quiet",
        help="Suppress output.",
    )
    parser.add_argument(
        "--check",
        default=False,
        action="store_true",
        help="Check files without modifying disk and print JSON results to stdout.",
    )
    parser.add_argument(
        "--output",
        type=Path,
        help="With --check, write the JSON results to this file instead of stdout.",
    )

    args = parser.parse_args()
    if args.check:
        results = []
        for json_path in args.file:
            try:
                original = json_path.read_text(encoding="utf-8")
                data = json.loads(original)
            except json.JSONDecodeError:
                results.append({"filepath": str(json_path)})
                continue
            formatted = json.dumps(
                data,
                indent=4,
                sort_keys=args.sort_keys,
                separators=(",", ": "),
            )
            new_content = formatted + "\n"
            if original != new_content:
                results.append(
                    {
                        "filepath": str(json_path),
                        "replacements": [new_content],
                    }
                )
        _write_results(results, args.output)
        return

    for json_path in args.file:
        try:
            with open(json_path, "r", encoding="utf-8") as f:
                original = f.read()
            data = json.loads(original)
            formatted = json.dumps(
                data,
                indent=4,
                sort_keys=args.sort_keys,
                separators=(",", ": "),
            )
            new_content = formatted + "\n"
            if original != new_content:
                temp_file = None
                try:
                    with tempfile.NamedTemporaryFile(
                        "w",
                        encoding="utf-8",
                        dir=json_path.parent,
                        delete=False,
                    ) as tf:
                        temp_file = tf.name
                        tf.write(new_content)
                    shutil.copymode(json_path, temp_file)
                    os.replace(temp_file, json_path)
                finally:
                    if temp_file and os.path.exists(temp_file):
                        os.unlink(temp_file)
        except json.JSONDecodeError as e:
            if not args.quiet:
                print(f"JSON decode error in {json_path}: {e}", file=sys.stderr)
            sys.exit(1)


if __name__ == "__main__":
    main()

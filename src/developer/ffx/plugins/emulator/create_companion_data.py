#!/usr/bin/env fuchsia-vendored-python
# Copyright 2021 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import json
import os
import sys


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--binary",
        required=True,
        help=(
            "The path to the binary in the base directory to list files for. This file"
            "is not included in the output"
        ),
    )
    parser.add_argument("--src_root", required=True, help="source path root.")
    parser.add_argument(
        "--dest_root", required=True, help="destination path root."
    )
    parser.add_argument(
        "--output", required=True, help="The path to the output file."
    )
    parser.add_argument(
        "--meta_out", required=True, help="path to metadata for tool."
    )
    parser.add_argument(
        "--name", required=True, help="name of host tool in metadata."
    )
    parser.add_argument(
        "--include",
        required=False,
        nargs="*",
        help="file to include in the output. Can be specified multiple times.",
    )

    args = parser.parse_args()

    directory = args.src_root
    binary_path = os.path.join(
        args.dest_root, os.path.relpath(args.binary, directory)
    )
    # the main binary should be first in the list.
    dest_files = [binary_path]
    with open(args.output, "w") as f:
        print(f"{binary_path}={args.binary}", file=f)
        if args.include:
            for filename in args.include:
                filepath = os.path.join(args.dest_root, filename)
                sourcepath = os.path.join(directory, filename)
                if binary_path != filepath:
                    dest_files += [filepath]
                    print(f"{filepath}={sourcepath}", file=f)
        else:
            for path, _dirs, files in os.walk(os.path.abspath(directory)):
                for filename in files:
                    source_filepath = os.path.join(path, filename)
                    filepath = os.path.join(
                        args.dest_root,
                        os.path.relpath(source_filepath, directory),
                    )
                    sourcepath = os.path.relpath(source_filepath)
                    if binary_path != filepath:
                        dest_files += [filepath]
                        print(f"{filepath}={sourcepath}", file=f)

    # Sort all files except the first one, which must be the binary.
    dest_files = [dest_files[0]] + sorted(dest_files[1:])

    metadata = {
        "files": dest_files,
        "name": args.name,
        "root": "tools",
        "type": "companion_host_tool",
    }

    with open(args.meta_out, "w") as f:
        print(json.dumps(metadata, indent=2), file=f)

    return 0


if __name__ == "__main__":
    sys.exit(main())

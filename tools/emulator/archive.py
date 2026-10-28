#!/usr/bin/env fuchsia-vendored-python
# Copyright 2019 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import os
import subprocess
import sys
import tempfile


def main() -> int:
    parser = argparse.ArgumentParser("Archives a directory")
    parser.add_argument(
        "--tarmaker", help="Path to the tarmaker binary", required=True
    )
    parser.add_argument(
        "--src", help="Path to the directory to archive", required=True
    )
    parser.add_argument("--dst", help="Path to the archive", required=True)
    parser.add_argument(
        "--depfile", help="Path to dependency file", required=True
    )
    args = parser.parse_args()

    deps = []

    with tempfile.NamedTemporaryFile("w") as manifest_file:
        for dirpath, dirnames, filenames in os.walk(args.src):
            for filename in filenames:
                path = os.path.join(dirpath, filename)
                deps.append(os.path.relpath(path))
                manifest_file.write(
                    f"{os.path.relpath(path, args.src)}={path}\n"
                )
        manifest_file.flush()
        subprocess.run(
            [
                args.tarmaker,
                "--manifest",
                manifest_file.name,
                "--output",
                args.dst,
            ],
            check=True,
        )

    with open(args.depfile, "w") as depfile:
        depfile.write("%s: %s\n" % (args.dst, " ".join(sorted(deps))))

    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import sys


def main() -> None:
    parser = argparse.ArgumentParser(
        prog="fx worktree",
        description="Manage Fuchsia worktrees",
    )
    parser.add_subparsers(dest="subcommand", required=True)

    args = parser.parse_args()
    print(f"Unknown subcommand: {args.subcommand}", file=sys.stderr)
    sys.exit(1)


if __name__ == "__main__":
    main()

#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import os
import re
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(
        description="Fetches the git diff for a changelist."
    )
    parser.add_argument(
        "cl", help="The CL URL or ID (e.g., 1488427, fxr/1488427, or full URL)"
    )
    args = parser.parse_args()

    cl_input = args.cl.strip().rstrip("/")
    change_id = None

    # Default to fuchsia unless we're in the v/g tree. Allows users to pass bare CL number while
    # shell is in v/g and get the right behavior.
    if "/vendor/google" in os.getcwd():
        host = "turquoise"
    else:
        host = "fuchsia"

    if "turquoise-internal-review" in cl_input or cl_input.startswith("tqr/"):
        host = "turquoise"
    elif (
        "fuchsia-review" in cl_input
        or "fxrev.dev" in cl_input
        or cl_input.startswith("fxr/")
    ):
        host = "fuchsia"

    # Match /+/id or /+/id/ps
    match = re.search(r"\+/(\d+)(?:/\d+)?$", cl_input)
    if match:
        change_id = match.group(1)

    # Match fxr/id, tqr/id, fxrev.dev/id (with or without patchset)
    if not change_id:
        match = re.search(r"(?:fxr|tqr|fxrev\.dev)/(\d+)(?:/\d+)?$", cl_input)
        if match:
            change_id = match.group(1)

    # Match changes/id (Gerrit API style)
    if not change_id:
        match = re.search(r"changes/(\d+)", cl_input)
        if match:
            change_id = match.group(1)

    # Match pure ID
    if not change_id:
        match = re.search(r"^(\d+)$", cl_input)
        if match:
            change_id = match.group(1)

    if not change_id:
        print(
            f"Error: Could not parse change ID from '{cl_input}'",
            file=sys.stderr,
        )
        sys.exit(1)

    if host == "fuchsia":
        url = f"https://fuchsia-review.googlesource.com/changes/{change_id}/revisions/current/patch?raw"
        cmd = ["curl", "-f", "-L", "-s", url]
    else:
        url = f"https://turquoise-internal-review.googlesource.com/changes/{change_id}/revisions/current/patch?raw"
        cmd = ["gob-curl", url]

    try:
        result = subprocess.run(
            cmd,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        print(result.stdout, end="")
    except subprocess.CalledProcessError as e:
        if e.returncode == 22:  # curl -f returns 22 for HTTP errors like 404
            print(
                f"Error: CL {change_id} not found on {host} (HTTP error).\n"
                f"Please check that the CL URL or ID is correct and you have access to it.",
                file=sys.stderr,
            )
        else:
            print(
                f"Error fetching diff: {e.stderr.strip() if e.stderr else e}",
                file=sys.stderr,
            )
        sys.exit(1)
    except FileNotFoundError:
        if "gob-curl" in cmd:
            print(
                "Error: gob-curl is required for turquoise internal review but was not found.",
                file=sys.stderr,
            )
        else:
            print("Error: curl not found.", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()

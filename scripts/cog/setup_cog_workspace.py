#!/usr/bin/env python3
# allow-non-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""This script is used to set up a cog-based workspace for Fuchsia development.
It is currently highly experimental and not guaranteed to work.
"""

import os
import re
import sys


def log_warn(message: str) -> None:
    """Prints a warning message."""
    print(f"WARNING: {message}")


def _workspace_base_path() -> str | None:
    """Returns the base path for the workspace."""
    user: str | None = os.environ.get("USER")
    if not user:
        return None
    return f"/google/cog/cloud/{user}"


def find_cog_workspace_directory() -> str | None:
    """Finds the workspace directory from the current working directory.

    Returns:
        The workspace directory if found, None otherwise.
    """
    user: str | None = os.environ.get("USER")
    if not user:
        return None

    path_prefix = f"/google/cog/cloud/{re.escape(user)}"
    path_pattern = re.compile(f"^{path_prefix}/[^/]+$")

    current_dir = os.getcwd()
    while current_dir != "/":
        print("checking dir: ", current_dir)
        print("patttern match: ", path_pattern)
        match = path_pattern.match(current_dir)
        if match:
            return match.group(0)
        else:
            current_dir = os.path.dirname(current_dir)
    return None


def main() -> None:
    """Main function to set up the cog workspace."""
    # This script is experimental and not ready for general use.
    # To enable, set the FUCHSIA_ALLOW_SETUP_COG_WORKSPACE environment variable.
    if not os.environ.get("FUCHSIA_ALLOW_SETUP_COG_WORKSPACE"):
        log_warn(
            "This script is highly experimental and not yet ready for use."
        )
        print(
            "To acknowledge this and proceed, please set the environment variable:"
        )
        print("  export FUCHSIA_ALLOW_SETUP_COG_WORKSPACE=1")
        sys.exit(1)

    workspace_dir = find_cog_workspace_directory()
    if not workspace_dir:
        log_warn(
            "Could not find workspace directory. Please run this script from a directory matching the pattern /google/cog/cloud/<user>/<workspace_name>."
        )
        sys.exit(1)

    print(f"Workspace dir: {workspace_dir}")


if __name__ == "__main__":
    main()

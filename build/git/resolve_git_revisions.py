#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import sys
from pathlib import Path

# When invoked by GN exec_script(), PYTHONPATH is not set by GN.
# Ensure the Fuchsia root is in sys.path to enable in-tree absolute imports.
_FUCHSIA_DIR = Path(__file__).resolve().parents[2]
if str(_FUCHSIA_DIR) not in sys.path:
    sys.path.insert(0, str(_FUCHSIA_DIR))

from build.git.git_utils import get_git_revision


def main() -> int:
    source_root = Path(__file__).resolve().parents[2]
    projects = {
        "fuchsia": source_root,
        "mesa": source_root / "third_party/mesa-migrating/src",
    }

    results = {}
    # Fuchsia
    results["fuchsia_revision"] = get_git_revision(projects["fuchsia"])

    # Mesa
    mesa_rev = get_git_revision(projects["mesa"])
    results["mesa_revision"] = mesa_rev[:10]

    print(json.dumps(results))
    return 0


if __name__ == "__main__":
    sys.exit(main())

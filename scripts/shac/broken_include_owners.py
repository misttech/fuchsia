# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Checks OWNERS file for broken includes.
"""

import json
import os
import re
import sys


def main():
    results = []
    for owners_file in sys.argv[1:]:
        with open(owners_file, "r") as file:
            lines = file.readlines()

        broken_includes = []

        for line_number, line in enumerate(lines, start=1):
            include_path = None
            line = line.split("#")[0]  # Ignore comments.
            # Search for "file:" or "include" keyword for importing OWNERS file.
            match = re.search(r"(include\s+|file:\s*)(\S+)", line)
            if match:
                include_path = match.group(2)

                if include_path:
                    if include_path.startswith("/"):
                        include_path = include_path.lstrip("/")
                        abs_path = os.path.abspath(include_path)
                    else:
                        dir_path = os.path.dirname(owners_file)
                        abs_path = os.path.abspath(
                            os.path.join(dir_path, include_path)
                        )

                    if not os.path.exists(abs_path):
                        broken_includes.append(line_number)

        if broken_includes:
            results.append({"filepath": owners_file, "lines": broken_includes})

    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()

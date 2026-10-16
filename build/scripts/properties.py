# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Utilities for parsing simple KEY=VALUE property configuration files."""

import argparse
import pathlib
import shlex


def str_to_bool(value: str | bool, default: bool | None = None) -> bool:
    """Parses a boolean string ('true'/'1'/'yes' or 'false'/'0'/'no')."""
    if isinstance(value, bool):
        return value
    if value.lower() in ("true", "1", "yes"):
        return True
    elif value.lower() in ("false", "0", "no"):
        return False
    if default is not None:
        return default
    raise argparse.ArgumentTypeError(f"Boolean value expected, got {value}")


def parse_properties(content: str) -> dict[str, str]:
    """Parses a simple KEY=VALUE properties string, supporting export syntax."""
    props: dict[str, str] = {}
    for line in content.splitlines():
        line = line.strip()
        if not line or line.startswith("//") or line.startswith("#"):
            continue
        k, sep, v = line.partition("=")
        if sep:
            key = k.strip().removeprefix("export ").strip()
            if key:
                try:
                    tokens = shlex.split(v.strip(), comments=True)
                except ValueError:
                    continue
                if tokens:
                    props[key] = tokens[0]
    return props


def load_properties(path: pathlib.Path) -> dict[str, str]:
    """Reads a simple KEY=VALUE properties file safely, supporting export syntax."""
    if not path.exists():
        return {}
    try:
        content = path.read_text()
    except OSError:
        return {}
    return parse_properties(content)

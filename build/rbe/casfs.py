#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""RBE casfs artifact virtualization configuration model, loader, and CLI."""

import argparse
import dataclasses
import os
import pathlib
import shlex
import sys
from collections.abc import Mapping, Sequence

_FUCHSIA_DIR = pathlib.Path(__file__).resolve().parent.parent.parent

# Path relative to the Fuchsia source root for sticky casfs RBE configuration.
_RBE_CASFS_CONFIG = pathlib.Path(".fx/config/rbe_casfs")

ENV_CASFS_VIRTUALIZE = "FX_BUILD_INTERNAL_RBE_CASFS_VIRTUALIZE"


@dataclasses.dataclass
class CasfsConfig:
    """Configuration for RBE casfs artifact virtualization."""

    virtualize: bool = False

    def to_env_dict(self) -> dict[str, str]:
        """Serializes configuration to environment variables for subprocesses."""
        return {
            ENV_CASFS_VIRTUALIZE: "1" if self.virtualize else "0",
        }


def _parse_bool(value: str | bool) -> bool | None:
    """Parses a boolean configuration value, returning None if unrecognized."""
    if isinstance(value, bool):
        return value
    val_lower = value.strip().lower()
    if val_lower in ("true", "1", "yes"):
        return True
    if val_lower in ("false", "0", "no"):
        return False
    return None


def _parse_properties(content: str) -> dict[str, str]:
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


def _load_properties(path: pathlib.Path) -> dict[str, str]:
    """Reads a simple KEY=VALUE properties file safely, supporting export syntax."""
    if not path.exists():
        return {}
    try:
        content = path.read_text()
    except OSError:
        return {}
    return _parse_properties(content)


def _load_preference(path: pathlib.Path) -> bool | None:
    """Loads FX_BUILD_INTERNAL_RBE_CASFS_VIRTUALIZE preference from a properties file."""
    props = _load_properties(path)
    if ENV_CASFS_VIRTUALIZE in props:
        raw = props[ENV_CASFS_VIRTUALIZE]
        parsed = _parse_bool(raw)
        if parsed is None:
            print(
                f"Warning: Invalid {ENV_CASFS_VIRTUALIZE} value {raw!r} in {path}; "
                "expected 0, 1, true, false, yes, or no.",
                file=sys.stderr,
            )
        return parsed
    return None


def load(
    source_dir: pathlib.Path = _FUCHSIA_DIR,
    environ: Mapping[str, str] | None = None,
) -> CasfsConfig:
    """Loads RBE casfs configuration, prioritizing environment over on-disk config."""
    env = environ if environ is not None else os.environ
    virtualize = _load_preference(source_dir / _RBE_CASFS_CONFIG) or False
    raw_env = env.get(ENV_CASFS_VIRTUALIZE, "")
    if raw_env != "":
        parsed = _parse_bool(raw_env)
        if parsed is not None:
            virtualize = parsed
        else:
            print(
                f"Warning: Invalid {ENV_CASFS_VIRTUALIZE} environment value {raw_env!r}; "
                "expected 0, 1, true, false, yes, or no.",
                file=sys.stderr,
            )
    return CasfsConfig(virtualize=virtualize)


def main(
    argv: Sequence[str],
    environ: Mapping[str, str],
) -> int:
    """CLI entry point printing the resolved virtualize setting ('1' or '0')."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--source-dir",
        type=pathlib.Path,
        default=_FUCHSIA_DIR,
        help="Path to the Fuchsia source root.",
    )
    args = parser.parse_args(argv)
    cfg = load(args.source_dir, environ)
    print("1" if cfg.virtualize else "0")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:], os.environ))

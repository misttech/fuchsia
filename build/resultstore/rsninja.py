#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Drop-in Ninja wrapper that injects ResultStore/BES telemetry."""

import argparse
import os
import sys
from collections.abc import Mapping, Sequence
from pathlib import Path

_SCRIPT_DIR = Path(__file__).resolve().parent
_FUCHSIA_DIR = _SCRIPT_DIR.parent.parent

from build.resultstore import related_invocations as rel_inv


def resolve_results_url(
    fuchsia_dir: Path, environ: Mapping[str, str]
) -> str | None:
    """Resolves the ResultStore web base URL, falling back to active profile if needed.

    Note: RS_results_url directly influences rsproxy and sub-builds to format
    links to uploaded build events.
    """
    if "RS_results_url" in environ:
        return environ["RS_results_url"]
    try:
        from build.resultstore import rs_profile

        profile = rs_profile.load(fuchsia_dir, environ)
        return profile.results_url
    except Exception:
        return None


def prepare_ninja_invocation(
    wrapper: str,
    ninja_bin: str,
    ninja_args: Sequence[str],
    environ: Mapping[str, str],
    fuchsia_dir: Path | None = None,
) -> tuple[list[str], dict[str, str]]:
    """Constructs the command line and environment for the wrapped Ninja execution.

    Args:
        wrapper: Path to the rs-ninja.sh wrapper script.
        ninja_bin: Path to the real prebuilt ninja binary.
        ninja_args: Command-line arguments to forward to Ninja.
        environ: Current environment mapping to inspect for ResultStore context.
        fuchsia_dir: Optional root Fuchsia checkout path used to resolve active profile
            configuration when RS_results_url is absent from environ.

    Returns:
        A tuple of (full_cmd, child_env):
            - full_cmd: Complete argv list [wrapper, ninja_bin, *bes_metadata_args, *ninja_args]
              to pass to execvpe.
            - child_env: Environment mapping augmented with ResultStore parent/sibling
              linkages and NINJA_BUILD_ID for sub-build propagation.
    """
    # RS_results_url directly influences rsproxy and sub-builds to link related invocations.
    effective_env = dict(environ)
    results_url = (
        resolve_results_url(fuchsia_dir, environ) if fuchsia_dir else None
    )
    if results_url:
        effective_env["RS_results_url"] = results_url
    elif "RSPROXY_FIFO" in environ and "RS_results_url" not in effective_env:
        raise RuntimeError(
            "Unable to determine ResultStore URL from environment (RS_results_url) "
            "or active profile while RSPROXY_FIFO is active."
        )

    linkage = rel_inv.resolve(effective_env)
    child_env = dict(effective_env)
    child_env.update(linkage.child_env)
    child_env["NINJA_BUILD_ID"] = linkage.invocation_id

    bes_metadata_args = [
        f"--bes_metadata={k}={v}" for k, v in sorted(linkage.metadata.items())
    ]
    full_cmd = [wrapper, ninja_bin] + bes_metadata_args + list(ninja_args)
    return full_cmd, child_env


def main(argv: Sequence[str]) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        add_help=False,  # Pass -h / --help through to ninja
    )
    parser.add_argument(
        "--wrapper", required=True, help="Path to rs-ninja.sh wrapper."
    )
    parser.add_argument(
        "--ninja-bin", required=True, help="Path to prebuilt ninja binary."
    )
    parser.add_argument(
        "command",
        nargs=argparse.REMAINDER,
        help="Remaining ninja command arguments to execute.",
    )

    args = parser.parse_args(argv)
    ninja_args = list(args.command)
    if ninja_args and ninja_args[0] == "--":
        ninja_args = ninja_args[1:]

    try:
        full_cmd, child_env = prepare_ninja_invocation(
            wrapper=args.wrapper,
            ninja_bin=args.ninja_bin,
            ninja_args=ninja_args,
            environ=os.environ,
            fuchsia_dir=_FUCHSIA_DIR,
        )
    except RuntimeError as e:
        sys.stderr.write(f"Error: {e}\n")
        return 1

    os.execvpe(full_cmd[0], full_cmd, child_env)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Determines the appropriate build authentication method.

It chooses from three outcomes:
  1. "loas"    : corporate gcert/LOAS credentials and accessible SrcFS helper.
  2. "oauth"   : local gcloud Application Default Credentials (ADC).
  3. "machine" : bot/VM using GCE VM Metadata.
"""

import argparse
import enum
import os
import pathlib
import shutil
import subprocess
import sys
from typing import TextIO

import gcloud


class AuthType(str, enum.Enum):
    """The supported authentication types."""

    LOAS = "loas"
    OAUTH = "oauth"
    MACHINE = "machine"

    def __str__(self) -> str:
        return self.value

    @property
    def is_user_based(self) -> bool:
        """Returns True if the authentication type is user-based (LOAS or OAuth)."""
        return self in (AuthType.LOAS, AuthType.OAUTH)


BAZEL_CRED_HELPER = pathlib.Path(
    "/google/src/head/depot/google3/devtools/blaze/bazel/credhelper/credhelper"
)

_SCRIPT_NAME = pathlib.Path(__file__).name


def msg(text: str, file: TextIO | None = None) -> None:
    """Print a self-identifying message to a stream."""
    if file is None:
        file = sys.stderr
    print(f"[{_SCRIPT_NAME}] {text}", file=file)


def is_executable(path: pathlib.Path) -> bool:
    """Checks if a path exists and is executable."""
    return path.exists() and os.access(path, os.X_OK)


def is_infra_env(env: dict[str, str]) -> bool:
    """Checks if the environment is a bot/infrastructure environment."""
    return "BUILDBUCKET_ID" in env or "SWARMING_TASK_ID" in env


def has_gcloud_or_adc(env: dict[str, str]) -> bool:
    """Checks if local gcloud ADC credentials or gcloud binary are present."""
    home = env.get("HOME", "")
    adc_file = pathlib.Path(home) / gcloud.ADC_SUBPATH if home else None
    has_gcloud = shutil.which("gcloud", path=env.get("PATH")) is not None
    return bool((adc_file and adc_file.exists()) or has_gcloud)


def check_loas_type(env: dict[str, str], script_path: pathlib.Path) -> str:
    """Runs check_loas_restrictions.sh and returns its output."""
    try:
        output = subprocess.check_output(
            [str(script_path)],
            text=True,
            stderr=subprocess.DEVNULL,
            env=env,
        )
        lines = output.strip().splitlines()
        return lines[-1] if lines else ""
    except (subprocess.CalledProcessError, FileNotFoundError):
        return ""


def select_auth_method(
    env: dict[str, str],
    check_loas_script: pathlib.Path,
    cred_helper: pathlib.Path = BAZEL_CRED_HELPER,
    verbose: bool = False,
) -> AuthType:
    """Select the correct authentication method based on the environment.

    Args:
        env: Environment variables to inspect.
        check_loas_script: Path to check_loas_restrictions.sh.
        cred_helper: Path to the Bazel credential helper.
        verbose: Print detailed diagnostic reasoning to stderr.

    Returns:
        The selected AuthType.

    Raises:
        ValueError: If no valid authentication method is detected.
    """

    def vmsg(text: str) -> None:
        if verbose:
            msg(text)

    # 1. Infrastructure environment
    if is_infra_env(env):
        vmsg(
            f"Detected infrastructure/bot environment (BUILDBUCKET_ID="
            f"{env.get('BUILDBUCKET_ID', '')}, SWARMING_TASK_ID="
            f"{env.get('SWARMING_TASK_ID', '')}). Selecting machine credentials."
        )
        return AuthType.MACHINE

    has_local_creds = has_gcloud_or_adc(env)

    # 2. Determine LOAS restriction type
    loas_type = check_loas_type(env, check_loas_script)
    vmsg(f"LOAS certificate type check returned: {loas_type}")

    # Case 1: Ideal corporate flow (unrestricted LOAS + accessible credhelper)
    if loas_type == "unrestricted" and is_executable(cred_helper):
        vmsg(
            "Unrestricted LOAS certificate and accessible Bazel credential helper detected. Selecting LOAS."
        )
        return AuthType.LOAS

    # Case 2: Fallback to local gcloud OAuth/ADC if available
    if has_local_creds:
        if loas_type == "unrestricted":
            msg(
                f"Warning: Bazel credential helper on SrcFS is not accessible: {cred_helper}"
            )
            msg("Gracefully falling back to OAuth (local gcloud/ADC) mode.")
        else:
            vmsg(
                "Local gcloud ADC or gcloud installation detected. Selecting OAuth."
            )
        return AuthType.OAUTH

    # Case 3: Completely unauthenticated
    raise ValueError(
        "We could not find a valid corporate LOAS credential, and local "
        "gcloud Application Default Credentials (ADC) are not configured.\n"
        "To configure RBE and authenticate successfully, please run: 'fx rbe auth'"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "-v",
        "--verbose",
        action="store_true",
        help="Print diagnostic reasoning to stderr.",
    )
    args = parser.parse_args()

    # Determine script directories
    script_dir = pathlib.Path(__file__).parent.resolve()
    check_loas_script = script_dir / "check_loas_restrictions.sh"

    try:
        auth_method = select_auth_method(
            env=dict(os.environ),
            check_loas_script=check_loas_script,
            verbose=args.verbose,
        )
        print(auth_method)
        return 0
    except ValueError as e:
        msg(str(e))
        return 1


if __name__ == "__main__":
    sys.exit(main())

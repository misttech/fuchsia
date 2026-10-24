# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Host interface for invoking Google Cloud SDK (gcloud) subcommands."""

import functools
import os
import pathlib
import shutil
import subprocess
import sys

try:
    from build.auth import apt
except ImportError:
    # Whether mypy considers this a redefinition depends on whether
    # `build.auth` is resolvable in the context it's checked in.
    import apt  # type: ignore[no-redef, unused-ignore]

# Standard relative subpath for gcloud's Application Default Credentials (ADC).
ADC_SUBPATH = pathlib.Path(
    ".config/gcloud/application_default_credentials.json"
)


def get_default_adc_path() -> pathlib.Path:
    """Returns the default, standard absolute path of gcloud's ADC JSON file."""
    try:
        home_dir = pathlib.Path.home()
    except (RuntimeError, KeyError):
        # Fallback for homeless or user-less environments
        home_dir = pathlib.Path("/tmp")
    return home_dir / ADC_SUBPATH


def resolve_global_adc_path() -> pathlib.Path:
    """Resolves and returns the active absolute path of global ADC.

    Prioritizes GOOGLE_APPLICATION_CREDENTIALS if defined in the environment,
    falling back to the standard gcloud ADC path.
    """
    env_path = os.environ.get("GOOGLE_APPLICATION_CREDENTIALS")
    if env_path:
        return pathlib.Path(env_path)
    return get_default_adc_path()


@functools.cache
def path() -> pathlib.Path | None:
    """Detects and returns the absolute canonical path of the gcloud binary if installed."""
    path_val = shutil.which("gcloud")
    if path_val:
        return pathlib.Path(path_val)

    # Fallback to standard system package installation location if PATH has not been updated yet.
    return apt.get_binary_path("google-cloud-cli", "gcloud")


def login() -> None:
    """Launches the standard gcloud Application Default Credentials (ADC) login workflow.

    Raises:
        RuntimeError: If gcloud is not installed or the login command fails.
    """
    gcloud_path = path()
    if not gcloud_path:
        raise RuntimeError("Google Cloud SDK ('gcloud') is not installed.")

    try:
        # Standard streams are forwarded to let the browser or code-based login flows
        # run perfectly in the active terminal/TTY.
        subprocess.run(
            [str(gcloud_path), "auth", "application-default", "login"],
            stdin=sys.stdin,
            stdout=sys.stdout,
            stderr=sys.stderr,
            check=True,
        )
    except subprocess.CalledProcessError as e:
        raise RuntimeError(f"Interactive gcloud login failed: {e}")

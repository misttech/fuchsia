#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Manages and isolates Google Cloud SDK credentials for the Fuchsia development environment.

This module can be imported as a library by other Python scripts,
or run directly as a standalone CLI tool with subcommands.
"""
# Add our active parent directory to sys.path to enable flat, direct imports of sister modules
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import argparse
import json
from typing import Any, TextIO

import apt
import gcloud

# Type alias representing a parsed JSON dictionary.
JsonDict = dict[str, Any]

# Filename for the workspace-isolated credentials copy.
ISOLATED_ADC_FILENAME = gcloud.ADC_SUBPATH.name

# Package name for Google Cloud SDK on APT-based package managers.
GOOGLE_CLOUD_CLI_PACKAGE = "google-cloud-cli"

_SCRIPT = pathlib.Path(__file__)


class GcloudCredsError(Exception):
    """Base exception for all Google Cloud credentials management and isolation failures."""


def msg(text: str, file: TextIO | None = None) -> None:
    """Print a message prefixed with the script's basename."""
    if file is None:
        file = sys.stdout
    print(f"[{_SCRIPT.name}] {text}", file=file)


def mutate_adc_quota_project(adc_data: JsonDict, quota_project: str) -> bool:
    """Injects or modifies the quota_project_id in ADC JSON data if applicable.

    Returns True if the credentials type is 'authorized_user' and was updated,
    otherwise False.
    """
    if adc_data.get("type") == "authorized_user":
        adc_data["quota_project_id"] = quota_project
        return True
    return False


def isolate_credentials(
    out_dir: pathlib.Path,
    quota_project: str,
    source_credentials_path: pathlib.Path | None = None,
) -> pathlib.Path | None:
    """Creates a workspace-isolated copy of standard Application Default Credentials.

    Reads the global or custom ADC, copies it to the writable shared output
    directory, injects the specified quota_project ID (only if the credentials type
    is 'authorized_user'), and returns the path to the isolated credentials file.

    Benefit of Isolation:
        Isolating credentials under the shared output directory and overriding the
        quota_project prevents RBE and ResultStore development traffic from being billed to
        or interfering with the developer's personal or other non-Fuchsia Google Cloud
        projects. It protects developer quota, prevents billing leakage, and restricts
        all development GCP API interactions securely to the specified Fuchsia project.

    Args:
        out_dir: Path to the writable shared output directory.
        quota_project: The Google Cloud quota project ID to write.
        source_credentials_path: Optional path to standard credentials file.
          If None, detects from GOOGLE_APPLICATION_CREDENTIALS or standard ADC.

    Returns:
        Path to the isolated credentials file, or None if no isolation was performed
        (e.g., for non-user service account credentials).

    Raises:
        GcloudCredsError: If no source credentials file can be located or resolved.
        json.JSONDecodeError: If the source credentials file contains malformed JSON.
        OSError: If reading the source or writing the isolated copy fails.
    """
    adc_src = source_credentials_path or gcloud.resolve_global_adc_path()
    if not adc_src.is_file():
        raise GcloudCredsError(
            f"No Application Default Credentials file found at '{adc_src}'"
        )

    adc_data: JsonDict = json.loads(adc_src.read_text())
    if not isinstance(adc_data, dict):
        raise json.JSONDecodeError(
            "Credentials JSON is not a dictionary structure", "", 0
        )

    # Only isolate and modify standard user default credentials (authorized_user).
    if mutate_adc_quota_project(adc_data, quota_project):
        out_dir.mkdir(parents=True, exist_ok=True)
        local_adc_path = out_dir / ISOLATED_ADC_FILENAME
        local_adc_path.write_text(json.dumps(adc_data, indent=2))
        local_adc_path.chmod(0o600)
        return local_adc_path

    return None


def isolate_credentials_safe(
    out_dir: pathlib.Path,
    quota_project: str,
    source_credentials_path: pathlib.Path | None = None,
) -> pathlib.Path | None:
    """Exception-safe version of isolate_credentials.

    Catches all expected filesystem, parsing, and environment exceptions, and
    silently returns None on any failure, allowing callers to safely fallback.
    """
    try:
        return isolate_credentials(
            out_dir=out_dir,
            quota_project=quota_project,
            source_credentials_path=source_credentials_path,
        )
    except GcloudCredsError as e:
        # Avoid printing warnings for missing files, which is normal for unauthenticated users.
        if "No Application Default Credentials file found" not in str(e):
            msg(
                f"Warning: Failed to isolate RBE credentials: {e}",
                file=sys.stderr,
            )
    except (OSError, json.JSONDecodeError) as e:
        msg(f"Warning: Failed to isolate RBE credentials: {e}", file=sys.stderr)

    return None


def ensure_gcloud_installed() -> None:
    """Verifies gcloud is in the PATH, prompting/installing if missing and available.

    Raises:
        GcloudCredsError: If gcloud is missing and cannot be automatically installed.
    """
    if gcloud.path():
        return

    if apt.exists(GOOGLE_CLOUD_CLI_PACKAGE):
        try:
            apt.install(GOOGLE_CLOUD_CLI_PACKAGE, interactive=True)
            # Re-verify path after successful install
            if not gcloud.path():
                raise GcloudCredsError(
                    "Installation succeeded, but 'gcloud' is still missing from PATH. "
                    "Please restart your terminal."
                )
            return
        except Exception as e:
            raise GcloudCredsError(str(e))

    raise GcloudCredsError(
        "Google Cloud SDK ('gcloud') is missing from your PATH.\n"
        "Please install the Google Cloud SDK before authenticating.\n"
        "For official, secure installation instructions, see:\n"
        "  https://cloud.google.com/sdk/docs/install"
    )


def run_gcloud_login() -> None:
    """Launches interactive Google Cloud Application Default Credentials login."""
    ensure_gcloud_installed()
    try:
        gcloud.login()
    except RuntimeError as e:
        raise GcloudCredsError(str(e))


def _cmd_isolate(args: argparse.Namespace) -> int:
    """Executes the 'isolate' subcommand."""
    adc_src = gcloud.resolve_global_adc_path()
    if not adc_src.is_file():
        # Silent clean exit when no credentials exist yet (fresh machine).
        return 0
    try:
        local_adc = isolate_credentials(
            out_dir=args.out_dir.resolve(),
            quota_project=args.quota_project,
        )
        if local_adc:
            print(local_adc)
        return 0
    except Exception as e:
        msg(f"Error: {e}", file=sys.stderr)
        return 1


def _cmd_login(args: argparse.Namespace) -> int:
    """Executes the 'login' subcommand."""
    try:
        run_gcloud_login()
        return 0
    except Exception as e:
        msg(f"Error: {e}", file=sys.stderr)
        return 1


def _cmd_ensure(args: argparse.Namespace) -> int:
    """Executes the 'ensure' subcommand."""
    adc_src = gcloud.resolve_global_adc_path()
    if not adc_src.is_file():
        if args.interactive:
            try:
                run_gcloud_login()
            except Exception as e:
                msg(f"Error: {e}", file=sys.stderr)
                return 1
        else:
            msg(
                "Error: No credentials found and --interactive is not set.",
                file=sys.stderr,
            )
            return 1

    # Credentials are now guaranteed to exist.
    try:
        local_adc = isolate_credentials(
            out_dir=args.out_dir.resolve(),
            quota_project=args.quota_project,
        )
        if local_adc:
            print(local_adc)
        return 0
    except Exception as e:
        msg(f"Error: {e}", file=sys.stderr)
        return 1


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description="Manages and isolates Google Cloud SDK credentials for the Fuchsia development environment."
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    # 1. Isolate Subcommand
    isolate_parser = subparsers.add_parser(
        "isolate",
        help="Creates a copy of global user default credentials with overridden quota project.",
    )
    isolate_parser.add_argument(
        "--out-dir",
        type=pathlib.Path,
        required=True,
        help="Path to a writable shared output directory, typically '<fuchsia_dir>/out'.",
    )
    isolate_parser.add_argument(
        "--quota-project",
        required=True,
        help="The Google Cloud quota project ID to write.",
    )
    isolate_parser.set_defaults(func=_cmd_isolate)

    # 2. Login Subcommand
    login_parser = subparsers.add_parser(
        "login",
        help="Runs 'gcloud auth application-default login' interactively.",
    )
    login_parser.set_defaults(func=_cmd_login)

    # 3. Ensure Subcommand
    ensure_parser = subparsers.add_parser(
        "ensure",
        help="Checks if ADC exists, prompts interactive login if missing, then isolates them.",
    )
    ensure_parser.add_argument(
        "--out-dir",
        type=pathlib.Path,
        required=True,
        help="Path to a writable shared output directory, typically '<fuchsia_dir>/out'.",
    )
    ensure_parser.add_argument(
        "--quota-project",
        required=True,
        help="The Google Cloud quota project ID to write.",
    )
    ensure_parser.add_argument(
        "--interactive",
        action="store_true",
        help="Launch interactive gcloud login if credentials are missing.",
    )
    ensure_parser.set_defaults(func=_cmd_ensure)

    # If no arguments are provided, default to displaying the help message
    if not argv:
        parser.print_help()
        return 0

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except KeyboardInterrupt:
        # Standard Unix SIGINT exit code (128 + 2) = 130
        sys.exit(130)

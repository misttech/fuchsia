#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Generates an ephemeral bazelrc and environment wrapper for Bazel invocations."""

import argparse
import dataclasses
import os
import sys
from collections.abc import Iterable, Mapping
from pathlib import Path

_DEFAULT_FUCHSIA_DIR = Path(__file__).resolve().parent.parent.parent.parent

from build.bazel.scripts import bazel_invocation as bazel_inv
from build.rbe import casfs
from build.resultstore import related_invocations as rel_inv
from build.resultstore import rs_profile

build_config_option = bazel_inv.build_config_option


def startup_config_option(options: str) -> str:
    """Returns a bazelrc command for a startup option."""
    return f"startup {options}"


@dataclasses.dataclass(frozen=True)
class InvocationConfig:
    """Encapsulates the ephemeral configuration generated for a Bazel invocation.

    Attributes:
        bazelrc_lines: List of command-line and config options to write to
          invocation.bazelrc.
        child_env: Environment variables to propagate to child sub-builds and
          actions.
        invocation_id: Canonical invocation UUID established for this Bazel
          build.
    """

    bazelrc_lines: list[str]
    child_env: dict[str, str]
    invocation_id: str


def _metadata_option(key: str, value: str) -> str:
    """Returns the equivalent bazel option for passing metadata."""
    return f"--build_metadata={key}={value}"


def _service_proxies_bazelrc(env: Mapping[str, str]) -> Iterable[str]:
    """Generates bazelrc lines for remote execution and BES proxy sockets."""
    yield from bazel_inv.generate_service_proxies_bazelrc(env)
    if env.get(casfs.ENV_CASFS_VIRTUALIZE) == "1":
        yield startup_config_option(
            f"--unix_digest_hash_attribute_name={casfs.XATTR_RBE_ARI_BIN_HASH}"
        )
    yield from _jiri_prebuilt_bazelrc(env)


def _jiri_prebuilt_bazelrc(env: Mapping[str, str]) -> Iterable[str]:
    """Configure sandbox mounts for on-demand Jiri/CIPD prebuilts."""
    env_fuchsia_dir = env.get("FUCHSIA_DIR")
    fuchsia_dir = (
        Path(env_fuchsia_dir).resolve()
        if env_fuchsia_dir
        else _DEFAULT_FUCHSIA_DIR
    )
    for subdir in (".jiri_root", "prebuilt"):
        dir_path = fuchsia_dir / subdir
        if dir_path.is_dir():
            yield f"build --sandbox_add_mount_pair={dir_path}"
            yield f"build --sandbox_writable_path={dir_path}"

    # TODO(https://fxbug.dev/570715338): Mount the Jiri proxy socket directory
    # and forward its environment variable once jiri proxy is implemented.


def _auth_bazelrc(env: Mapping[str, str]) -> Iterable[str]:
    """Generates bazelrc lines for authentication (e.g. gcertauth)."""
    rbe_auth = env.get("FX_BUILD_RBE_AUTH_TYPE", "")
    rs_auth = env.get("FX_BUILD_RS_AUTH_TYPE", "")
    yield from bazel_inv.generate_auth_bazelrc(rbe_auth, rs_auth)


def _log_options_bazelrc(log_dir: Path) -> Iterable[str]:
    """Generates options pointing Bazel invocation log outputs to log_dir."""
    workspace_log = log_dir / "workspace_events.log"
    exec_log = log_dir / "exec_log.pb.zstd"
    profile_log = log_dir / "command.profile.gz"
    yield f"common --experimental_workspace_rules_log_file={workspace_log}"
    yield f"common:exec_log --execution_log_compact_file={exec_log}"
    yield f"common:_profiling --profile={profile_log}"


def generate_invocation(
    env: Mapping[str, str],
    invocation_id: str | None = None,
    log_dir: Path | None = None,
) -> InvocationConfig:
    """Resolves invocation hierarchy, bazelrc configuration lines, and child environment.

    Args:
        env: Environment variable mapping (typically os.environ).
        invocation_id: Optional explicit invocation UUID. If omitted, resolved
          automatically by related_invocations (adopting FX_BUILD_UUID for
          top-level builds, or generating a unique child UUID for sub-builds).
        log_dir: Optional path to the invocation log directory. When provided,
          adds options pointing Bazel's workspace rules log, execution log, and
          command profile to this directory.

    Returns:
        An InvocationConfig dataclass containing:
          - bazelrc_lines: List of options to write to invocation.bazelrc.
          - child_env: Environment mapping augmented with ResultStore parent and
            sibling linkage variables for sub-build propagation.
          - invocation_id: The resolved canonical invocation UUID.
    """
    effective_env = dict(env)
    uuid = env.get(rel_inv.ENV_FX_BUILD_UUID)
    bbid = env.get(rel_inv.ENV_BUILDBUCKET_ID)
    if (uuid or bbid) and rel_inv.ENV_RESULTS_URL not in effective_env:
        try:
            profile = rs_profile.load(_DEFAULT_FUCHSIA_DIR, env)
            effective_env[rel_inv.ENV_RESULTS_URL] = profile.results_url
        except Exception:
            pass

    linkage = rel_inv.resolve(effective_env, invocation_id=invocation_id)

    header = [
        "# This bazelrc file contains ephemeral options that are not intended",
        "# to persist across build invocations.",
        "# AUTO-GENERATED - DO NOT EDIT!",
    ]

    bazelrc_lines: list[str] = list(header)
    bazelrc_lines.append(f"build --invocation_id={linkage.invocation_id}")

    if uuid or bbid:
        for k, v in sorted(linkage.metadata.items()):
            opt = _metadata_option(k, v)
            bazelrc_lines.append(build_config_option("_bes_common", opt))

    bazelrc_lines.extend(_auth_bazelrc(env))
    bazelrc_lines.extend(_service_proxies_bazelrc(env))

    if log_dir:
        bazelrc_lines.extend(_log_options_bazelrc(log_dir))

    child_env = dict(env)
    child_env.update(linkage.child_env)

    return InvocationConfig(
        bazelrc_lines=bazelrc_lines,
        child_env=child_env,
        invocation_id=linkage.invocation_id,
    )


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--output-bazelrc",
        type=Path,
        help="Optional file path to write generated bazelrc options.",
    )
    parser.add_argument(
        "--invocation-id",
        default="",
        help="Optional explicit invocation ID to associate with this build.",
    )
    parser.add_argument(
        "--log-dir",
        type=Path,
        help="Optional path to the invocation log directory to configure log file options.",
    )
    parser.add_argument(
        "command",
        nargs=argparse.REMAINDER,
        help="Optional command and arguments to execute within the configured child environment.",
    )
    args = parser.parse_args(argv)

    # Only treat log_dir as valid if it is a non-empty, non-relative-dot path.
    # When callers pass `--log-dir=""`, Path("") resolves to the current
    # working directory, which may be mounted read-only in infra environments
    # (such as Swarming bots running `fint set` / `gn gen`).
    log_dir = (
        args.log_dir
        if (args.log_dir and str(args.log_dir) not in ("", "."))
        else None
    )

    config = generate_invocation(
        os.environ,
        invocation_id=args.invocation_id or None,
        log_dir=log_dir,
    )

    cmd = list(args.command)
    if cmd and cmd[0] == "--":
        cmd = cmd[1:]

    content = "\n".join(config.bazelrc_lines) + "\n"

    if log_dir:
        try:
            (log_dir / "invocation.bazelrc").write_text(content)
        except OSError as e:
            sys.stderr.write(
                f"WARNING: Failed to save debug copy of invocation.bazelrc to {log_dir}: {e}\n"
            )

    if args.output_bazelrc:
        args.output_bazelrc.write_text(content)
    elif not cmd:
        sys.stdout.write(content)
    else:
        parser.error(
            "--output-bazelrc is required when executing a wrapped command."
        )

    if cmd:
        os.execvpe(cmd[0], cmd, config.child_env)

    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

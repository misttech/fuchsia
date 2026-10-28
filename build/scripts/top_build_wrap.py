#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Orchestrates feature-enabling wrappers around Fuchsia build commands.

Can be executed directly as a standalone CLI tool or imported as a library.
"""

from __future__ import annotations

import argparse
import dataclasses
import functools
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import time
from typing import Mapping, Sequence, TextIO

from build.scripts import python_subprocess

_SCRIPT: pathlib.Path = pathlib.Path(__file__)
_SOURCE_DIR: pathlib.Path = _SCRIPT.resolve().parent.parent.parent

_PROFILE_WRAPPER: pathlib.Path = pathlib.Path("build/profile/profile_wrap.sh")
_HARDWARE_PROFILER: pathlib.Path = pathlib.Path(
    "build/profile/hardware_profiler.py"
)
_REPROXY_WRAPPER: pathlib.Path = pathlib.Path(
    "build/rbe/fuchsia-reproxy-wrap.sh"
)
_RSPROXY_WRAPPER: pathlib.Path = pathlib.Path(
    "build/resultstore/fuchsia-rsproxy-wrap.sh"
)
_TUI_WRAPPER: pathlib.Path = pathlib.Path("build/tui/build-tui-wrap.sh")

_BUILD_LOGS_DIR: str = "_build_logs"
_TUI_LOGS_DIR: str = "tui_logs"

_BUILD_PROFILE_DIR: str = "build_profile"
_SYSTEM_PROFILE_JSON: str = "system_profile.json"
_HARDWARE_PROFILE_JSON: str = "hardware_profile.json"
_HARDWARE_PROFILE_CACHE_JSON: str = "hardware_profile.cache.json"

_REPROXY_LOGS_DIR: str = "reproxy_logs"
_REPROXY_TMPDIRS_DIR: str = ".reproxy_tmpdirs"
_RBE_METRICS_TXT: str = "rbe_metrics.txt"
_REPROXY_RRPL: str = "reproxy.rrpl"

_RSPROXY_LOGS_DIR: str = "rsproxy_logs"


def msg(text: str, file: TextIO = sys.stderr) -> None:
    """Print a message prefixed with the script's basename."""
    print(f"[{_SCRIPT.name}]: {text}", file=file)


def ts_msg(text: str, file: TextIO = sys.stderr) -> None:
    """Print a message with a timestamp prefixed with the script's basename."""
    print(f"[@{time.time():.6f}] [{_SCRIPT.name}]: {text}", file=file)


@dataclasses.dataclass
class Config:
    """Configuration for top-level build wrapper orchestration."""

    build_dir: pathlib.Path
    log_dir: pathlib.Path | None = None
    rbe_auth_type: str | None = None
    resultstore_auth_type: str | None = None
    use_machine_credentials: bool = False
    collect_system_profile: bool = False
    needs_reproxy_rbe: bool = False
    reproxy_cfgs: list[pathlib.Path] = dataclasses.field(default_factory=list)
    enable_resultstore: bool = False
    pre_build_uploads: list[pathlib.Path] = dataclasses.field(
        default_factory=list
    )
    post_build_uploads: list[pathlib.Path] = dataclasses.field(
        default_factory=list
    )
    tui: bool = False
    verbose: bool = False
    dry_run: bool = False


@dataclasses.dataclass(frozen=True)
class FeatureLayer:
    """An orchestrated wrapper layer, its storage, and its generated artifacts."""

    name: str
    prefix: list[str]
    log_dir: pathlib.Path
    artifacts: Mapping[str, pathlib.Path] = dataclasses.field(
        default_factory=dict
    )
    post_build_uploads: list[pathlib.Path] = dataclasses.field(
        default_factory=list
    )


class WrapContext:
    """Execution context for build wrapper orchestration."""

    def __init__(
        self,
        config: Config,
        source_dir: pathlib.Path = _SOURCE_DIR,
    ) -> None:
        self.config = config
        self.source_dir = source_dir
        self.log_dir = resolve_log_dir(config, source_dir)

    def setup(self) -> None:
        """Performs filesystem and preflight setup for active wrappers."""
        if self.config.dry_run:
            return

        if self.config.collect_system_profile:
            profile_log_dir = self.log_dir / _BUILD_PROFILE_DIR
            profile_log_dir.mkdir(parents=True, exist_ok=True)
            cache_file = self.config.build_dir / _HARDWARE_PROFILE_CACHE_JSON
            if not cache_file.exists():
                if self.config.verbose:
                    msg(
                        "Host hardware profile cache missing. Scanning host topology..."
                    )
                try:
                    python_subprocess.run(
                        [
                            str(self.source_dir / _HARDWARE_PROFILER),
                            "--output",
                            str(cache_file),
                        ],
                        source_dir=self.source_dir,
                        check=True,
                    )
                except (subprocess.CalledProcessError, OSError) as e:
                    if self.config.verbose:
                        msg(f"Warning: hardware profiler failed: {e}")

            hardware_profile_log = profile_log_dir / _HARDWARE_PROFILE_JSON
            if cache_file.exists():
                shutil.copyfile(cache_file, hardware_profile_log)

        if self.config.needs_reproxy_rbe:
            reproxy_log_dir = self.log_dir / _REPROXY_LOGS_DIR
            reproxy_log_dir.mkdir(parents=True, exist_ok=True)
            reproxy_tmpdir = (
                self.config.build_dir / _REPROXY_TMPDIRS_DIR / self.log_dir.name
            )
            reproxy_tmpdir.mkdir(parents=True, exist_ok=True)

    @functools.cached_property
    def tui(self) -> FeatureLayer | None:
        """Composes the TUI wrapper feature layer."""
        if not self.config.tui:
            return None

        tui_log_dir = self.log_dir / _TUI_LOGS_DIR
        cmd = [
            str(self.source_dir / _TUI_WRAPPER),
            "--log-dir",
            str(tui_log_dir),
        ]
        if self.config.verbose:
            cmd.append("--verbose")
        cmd.append("--")
        return FeatureLayer(
            name="tui",
            prefix=cmd,
            log_dir=tui_log_dir,
            artifacts={"log_dir": tui_log_dir},
        )

    @functools.cached_property
    def profile(self) -> FeatureLayer | None:
        """Composes the system profiler wrapper feature layer."""
        if not self.config.collect_system_profile:
            return None

        profile_log_dir = self.log_dir / _BUILD_PROFILE_DIR
        system_profile_log = profile_log_dir / _SYSTEM_PROFILE_JSON
        hardware_profile_log = profile_log_dir / _HARDWARE_PROFILE_JSON

        cmd = [
            str(self.source_dir / _PROFILE_WRAPPER),
            "--system-log",
            str(system_profile_log),
        ]
        if self.config.verbose:
            cmd.append("--verbose")
        cmd.append("--")

        uploads = [system_profile_log, hardware_profile_log]
        return FeatureLayer(
            name="profile",
            prefix=cmd,
            log_dir=profile_log_dir,
            artifacts={
                "system_profile": system_profile_log,
                "hardware_profile": hardware_profile_log,
            },
            post_build_uploads=uploads,
        )

    @functools.cached_property
    def rbe(self) -> FeatureLayer | None:
        """Composes the RBE reproxy wrapper feature layer."""
        if not self.config.needs_reproxy_rbe:
            return None

        reproxy_log_dir = self.log_dir / _REPROXY_LOGS_DIR
        reproxy_tmpdir = (
            self.config.build_dir / _REPROXY_TMPDIRS_DIR / self.log_dir.name
        )

        cmd = [
            str(self.source_dir / _REPROXY_WRAPPER),
            "--logdir",
            str(reproxy_log_dir),
            "--tmpdir",
            str(reproxy_tmpdir),
        ]
        if self.config.rbe_auth_type:
            cmd.extend(["--auth-type", self.config.rbe_auth_type])
        if self.config.use_machine_credentials:
            cmd.append("--use-machine-credentials")
        if self.config.verbose:
            cmd.append("--verbose")
        for cfg in self.config.reproxy_cfgs:
            cmd.extend(["--cfg", str(cfg)])
        if not self.config.enable_resultstore:
            cmd.append("--async_reproxy_termination")
        cmd.append("--")

        metrics_file = reproxy_log_dir / _RBE_METRICS_TXT
        rrpl_file = reproxy_log_dir / _REPROXY_RRPL
        uploads = [metrics_file, rrpl_file]
        return FeatureLayer(
            name="rbe",
            prefix=cmd,
            log_dir=reproxy_log_dir,
            artifacts={
                "metrics": metrics_file,
                "rrpl": rrpl_file,
            },
            post_build_uploads=uploads,
        )

    @functools.cached_property
    def resultstore(self) -> FeatureLayer | None:
        """Composes the ResultStore rsproxy wrapper feature layer."""
        return self._compose_resultstore()

    def _compose_resultstore(
        self,
        extra_post_build_uploads: Sequence[pathlib.Path] = (),
    ) -> FeatureLayer | None:
        if not (self.config.enable_resultstore or self.config.tui):
            return None

        rsproxy_log_dir = self.log_dir / _RSPROXY_LOGS_DIR
        cmd = [str(self.source_dir / _RSPROXY_WRAPPER)]
        if self.config.enable_resultstore:
            if self.config.resultstore_auth_type:
                cmd.extend(["--auth-type", self.config.resultstore_auth_type])
        else:
            cmd.extend(["--auth-type", "none"])

        if self.config.use_machine_credentials:
            cmd.append("--use-machine-credentials")
        if self.config.verbose:
            cmd.append("--verbose")
        cmd.extend(["--log-dir", str(rsproxy_log_dir)])

        all_post_build_uploads = list(self.config.post_build_uploads)
        if self.profile:
            all_post_build_uploads.extend(self.profile.post_build_uploads)
        if self.rbe:
            all_post_build_uploads.extend(self.rbe.post_build_uploads)
        all_post_build_uploads.extend(extra_post_build_uploads)

        if self.config.enable_resultstore:
            for f in self.config.pre_build_uploads:
                cmd.extend(["--pre_build_uploads", str(f)])
            for f in all_post_build_uploads:
                cmd.extend(["--post_build_uploads", str(f)])
        cmd.append("--")

        return FeatureLayer(
            name="resultstore",
            prefix=cmd,
            log_dir=rsproxy_log_dir,
            artifacts={"log_dir": rsproxy_log_dir},
            post_build_uploads=all_post_build_uploads,
        )

    def command_prefix(
        self,
        extra_post_build_uploads: Sequence[pathlib.Path] = (),
    ) -> list[str]:
        """Composes the combined wrapper command prefix in canonical order."""
        self.setup()
        layers = [
            self.tui,
            self._compose_resultstore(
                extra_post_build_uploads=extra_post_build_uploads
            ),
            self.profile,
            self.rbe,
        ]
        prefix: list[str] = []
        for layer in layers:
            if layer is not None:
                prefix.extend(layer.prefix)
        return prefix


def resolve_log_dir(
    config: Config,
    source_dir: pathlib.Path,
) -> pathlib.Path:
    """Resolves the build log directory, creating one under out/_build_logs if unset."""
    if config.log_dir is not None:
        return config.log_dir

    out_dir_root = source_dir / "out"
    build_logs_root = out_dir_root / _BUILD_LOGS_DIR
    log_dir_base = build_logs_root / config.build_dir.name
    log_dir_base.mkdir(parents=True, exist_ok=True)
    timestamp = time.strftime("%Y%m%d-%H%M%S")
    return pathlib.Path(
        tempfile.mkdtemp(prefix=f"build.{timestamp}.", dir=str(log_dir_base))
    )


def _str_to_bool(val: str) -> bool:
    """Type-converter for boolean CLI flags."""
    lower = val.lower()
    if lower in ("1", "true", "yes"):
        return True
    if lower in ("0", "false", "no", ""):
        return False
    raise argparse.ArgumentTypeError(f"Boolean value expected, got {val!r}")


def _parse_resultstore_mode(val: str) -> bool:
    """Type-converter for the --resultstore CLI option.

    ResultStore uploads can be enabled for all build tools ('all'), specific
    build systems ('ninja', 'bazel'), or booleans ('true', '1', 'yes').
    At the wrapper orchestrator level, any of these active modes enables
    the ResultStore proxy daemon (rsproxy), while 'none', 'false', '0',
    or 'no' disables it.
    """
    lower = val.lower()
    if lower in ("all", "ninja", "bazel", "1", "true", "yes"):
        return True
    if lower in ("none", "0", "false", "no", ""):
        return False
    raise argparse.ArgumentTypeError(
        f"Invalid resultstore mode: {val!r} (expected all, ninja, bazel, none, or boolean)"
    )


def _build_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Top-level build wrapper orchestrator",
        add_help=False,
        exit_on_error=False,
    )
    parser.add_argument("-h", "--help", action="store_true")
    parser.add_argument(
        "-v",
        "--verbose",
        action="store_true",
        help="Run verbosely and print debug messages.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the final command instead of running it.",
    )
    parser.add_argument(
        "--build-dir",
        type=pathlib.Path,
        help="Build output dir (e.g. out/default).",
    )
    parser.add_argument(
        "--log-dir",
        type=pathlib.Path,
        help="Where to produce logs for various build tools.",
    )
    parser.add_argument(
        "--rbe-auth-type",
        help="Authentication type for RBE (e.g. machine, none, auto, loas, oauth).",
    )
    parser.add_argument(
        "--resultstore-auth-type",
        help="Authentication type for ResultStore (e.g. machine, none, auto, loas, oauth).",
    )
    parser.add_argument(
        "--use-machine-credentials",
        action="store_true",
        help="Use GCE machine credentials (bypasses LOAS/OAuth checks).",
    )
    parser.add_argument(
        "--profile",
        type=_str_to_bool,
        nargs="?",
        const=True,
        default=False,
        help="Enable system profiling during build.",
    )
    parser.add_argument(
        "--rbe",
        type=_str_to_bool,
        nargs="?",
        const=True,
        default=False,
        help="Enable Remote Build Execution using reproxy.",
    )
    parser.add_argument(
        "--reproxy-cfg",
        action="append",
        type=pathlib.Path,
        help="Additional reproxy configs to merge and use (can be repeated).",
    )
    parser.add_argument(
        "--resultstore",
        type=_parse_resultstore_mode,
        nargs="?",
        const=True,
        default=False,
        help="Enable uploading build metadata and results to ResultStore.",
    )
    parser.add_argument(
        "--pre-build-uploads",
        action="append",
        type=pathlib.Path,
        help="Configure-time invocation artifacts to upload to ResultStore (can be repeated).",
    )
    parser.add_argument(
        "--post-build-uploads",
        action="append",
        type=pathlib.Path,
        help="End-of-build invocation artifacts to upload to ResultStore (can be repeated).",
    )
    parser.add_argument(
        "--tui",
        type=_str_to_bool,
        nargs="?",
        const=True,
        default=False,
        help="Enable terminal UI for monitoring build.",
    )
    return parser


def parse_args(argv: Sequence[str]) -> tuple[Config | None, list[str]]:
    """Parses CLI arguments into (Config, command), or (None, []) if help was requested."""
    args_list = list(argv)
    if "--" in args_list:
        sep_idx = args_list.index("--")
        opt_args = args_list[:sep_idx]
        wrapped_command = args_list[sep_idx + 1 :]
    else:
        opt_args = args_list
        wrapped_command = []

    parser = _build_arg_parser()
    args, unknown = parser.parse_known_args(opt_args)

    if args.help:
        parser.print_help()
        return None, []

    if unknown and args.verbose:
        msg(f"Warning: Unrecognized arguments before --: {' '.join(unknown)}")

    if not args.build_dir:
        raise ValueError("Missing required --build-dir option.")

    if not wrapped_command:
        raise ValueError("Missing wrapped command after --.")

    config = Config(
        build_dir=args.build_dir,
        log_dir=args.log_dir,
        rbe_auth_type=args.rbe_auth_type,
        resultstore_auth_type=args.resultstore_auth_type,
        use_machine_credentials=args.use_machine_credentials,
        collect_system_profile=args.profile,
        needs_reproxy_rbe=args.rbe,
        reproxy_cfgs=args.reproxy_cfg or [],
        enable_resultstore=args.resultstore,
        pre_build_uploads=args.pre_build_uploads or [],
        post_build_uploads=args.post_build_uploads or [],
        tui=args.tui,
        verbose=args.verbose,
        dry_run=args.dry_run,
    )
    return config, wrapped_command


def main(argv: Sequence[str]) -> int:
    try:
        config, command = parse_args(argv)
    except (ValueError, argparse.ArgumentError) as e:
        msg(f"Error: {e}")
        return 1

    if config is None:
        return 0

    context = WrapContext(config)
    full_cmd = context.command_prefix() + command

    if config.verbose:
        ts_msg("Starting build wrapper orchestration")
        msg(f"full command: {' '.join(full_cmd)}")

    if config.dry_run:
        print(" ".join(full_cmd))
        return 0

    os.execvp(full_cmd[0], full_cmd)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

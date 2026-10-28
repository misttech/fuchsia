#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for top_build_wrap.py."""

from __future__ import annotations

import argparse
import contextlib
import io
import pathlib
import sys
import tempfile
import unittest
from typing import Any, Sequence

from build.scripts import python_subprocess, top_build_wrap


class TopBuildWrapTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.tmp_path = pathlib.Path(self.temp_dir.name)
        self.build_dir = self.tmp_path / "out" / "default"
        self.build_dir.mkdir(parents=True)
        self.log_dir = self.tmp_path / "logs"
        self.log_dir.mkdir(parents=True)

        self.tui_wrapper = str(self.tmp_path / top_build_wrap._TUI_WRAPPER)
        self.profile_wrapper = str(
            self.tmp_path / top_build_wrap._PROFILE_WRAPPER
        )
        self.reproxy_wrapper = str(
            self.tmp_path / top_build_wrap._REPROXY_WRAPPER
        )
        self.rsproxy_wrapper = str(
            self.tmp_path / top_build_wrap._RSPROXY_WRAPPER
        )

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def make_context(self, **kwargs: Any) -> top_build_wrap.WrapContext:
        """Helper to create a WrapContext with test defaults."""
        defaults: dict[str, Any] = {
            "build_dir": self.build_dir,
            "log_dir": self.log_dir,
        }
        defaults.update(kwargs)
        return top_build_wrap.WrapContext(
            top_build_wrap.Config(**defaults), source_dir=self.tmp_path
        )

    def run_dry_run(self, executable: Sequence[str]) -> str:
        """Runs a dry-run invocation and returns stdout."""
        cmd = list(executable) + [
            "--dry-run",
            "--build-dir",
            str(self.build_dir),
            "--log-dir",
            str(self.log_dir),
            "--",
            "ninja",
            "target",
        ]
        return python_subprocess.check_output(cmd, source_dir=self.tmp_path)

    def test_all_layers_disabled_by_default(self) -> None:
        context = self.make_context()
        self.assertEqual(context.command_prefix(), [])
        self.assertIsNone(context.tui)
        self.assertIsNone(context.profile)
        self.assertIsNone(context.rbe)
        self.assertIsNone(context.resultstore)
        self.assertEqual(context.log_dir, self.log_dir)

    def test_tui_enabled(self) -> None:
        context = self.make_context(tui=True)
        assert context.tui is not None
        assert context.resultstore is not None
        self.assertEqual(
            context.command_prefix(),
            [
                self.tui_wrapper,
                "--log-dir",
                str(context.tui.log_dir),
                "--",
                self.rsproxy_wrapper,
                "--auth-type",
                "none",
                "--log-dir",
                str(context.resultstore.log_dir),
                "--",
            ],
        )
        self.assertEqual(context.tui.log_dir.name, top_build_wrap._TUI_LOGS_DIR)
        self.assertEqual(
            context.tui.artifacts, {"log_dir": context.tui.log_dir}
        )

    def test_tui_verbose(self) -> None:
        context = self.make_context(tui=True, verbose=True)
        assert context.tui is not None
        self.assertEqual(
            context.tui.prefix,
            [
                self.tui_wrapper,
                "--log-dir",
                str(context.tui.log_dir),
                "--verbose",
                "--",
            ],
        )

    def test_profile_enabled(self) -> None:
        cache_file = self.build_dir / "hardware_profile.cache.json"
        cache_file.write_text('{"cpu": "x64"}\n', encoding="utf-8")

        context = self.make_context(
            collect_system_profile=True, enable_resultstore=True
        )
        assert context.profile is not None
        prefix = context.command_prefix()

        expected_profile_log = context.profile.artifacts["system_profile"]
        expected_hw_log = context.profile.artifacts["hardware_profile"]
        self.assertTrue(expected_hw_log.exists())
        self.assertEqual(
            expected_hw_log.read_text(encoding="utf-8"), '{"cpu": "x64"}\n'
        )

        self.assertIn(self.profile_wrapper, prefix)
        self.assertIn(str(expected_profile_log), prefix)
        self.assertIn(str(expected_hw_log), prefix)

    def test_profile_artifacts(self) -> None:
        cache_file = self.build_dir / "hardware_profile.cache.json"
        cache_file.write_text("{}", encoding="utf-8")

        context = self.make_context(collect_system_profile=True)
        layer = context.profile
        self.assertIsNotNone(layer.log_dir)
        self.assertEqual(layer.log_dir.name, top_build_wrap._BUILD_PROFILE_DIR)
        expected_sys_log = layer.artifacts["system_profile"]
        expected_hw_log = layer.artifacts["hardware_profile"]
        self.assertEqual(expected_sys_log.name, "system_profile.json")
        self.assertEqual(expected_hw_log.name, "hardware_profile.json")
        self.assertEqual(
            layer.prefix,
            [
                self.profile_wrapper,
                "--system-log",
                str(expected_sys_log),
                "--",
            ],
        )
        self.assertEqual(
            layer.post_build_uploads, [expected_sys_log, expected_hw_log]
        )

    def test_rbe_enabled_without_resultstore(self) -> None:
        context = self.make_context(
            needs_reproxy_rbe=True,
            enable_resultstore=False,
            rbe_auth_type="oauth",
            reproxy_cfgs=[pathlib.Path("/path/to/reproxy.cfg")],
        )
        prefix = context.command_prefix()

        self.assertIn(self.reproxy_wrapper, prefix)
        self.assertIn("--logdir", prefix)
        self.assertIn(str(context.rbe.log_dir), prefix)
        self.assertIn("--tmpdir", prefix)
        self.assertIn(
            top_build_wrap._REPROXY_TMPDIRS_DIR,
            prefix[prefix.index("--tmpdir") + 1],
        )
        self.assertIn("--auth-type", prefix)
        self.assertIn("oauth", prefix)
        self.assertIn("--cfg", prefix)
        self.assertIn("/path/to/reproxy.cfg", prefix)
        self.assertIn("--async_reproxy_termination", prefix)
        self.assertNotIn(self.rsproxy_wrapper, prefix)

    def test_rbe_enabled_with_resultstore(self) -> None:
        context = self.make_context(
            needs_reproxy_rbe=True,
            enable_resultstore=True,
        )
        assert context.rbe is not None
        prefix = context.command_prefix()

        self.assertIn(self.reproxy_wrapper, prefix)
        self.assertIn(self.rsproxy_wrapper, prefix)
        self.assertNotIn("--async_reproxy_termination", prefix)

        self.assertIn(str(context.rbe.artifacts["metrics"]), prefix)
        self.assertIn(str(context.rbe.artifacts["rrpl"]), prefix)

    def test_rbe_artifacts(self) -> None:
        context = self.make_context(
            needs_reproxy_rbe=True,
            rbe_auth_type="machine",
            use_machine_credentials=True,
            enable_resultstore=False,
        )
        layer = context.rbe
        self.assertIsNotNone(layer)
        assert layer is not None
        self.assertEqual(layer.log_dir.name, top_build_wrap._REPROXY_LOGS_DIR)
        metrics_file = layer.artifacts["metrics"]
        rrpl_file = layer.artifacts["rrpl"]
        self.assertEqual(metrics_file.name, "rbe_metrics.txt")
        self.assertEqual(rrpl_file.name, "reproxy.rrpl")
        reproxy_tmpdir = (
            self.build_dir
            / top_build_wrap._REPROXY_TMPDIRS_DIR
            / self.log_dir.name
        )
        self.assertEqual(
            layer.prefix,
            [
                self.reproxy_wrapper,
                "--logdir",
                str(layer.log_dir),
                "--tmpdir",
                str(reproxy_tmpdir),
                "--auth-type",
                "machine",
                "--use-machine-credentials",
                "--async_reproxy_termination",
                "--",
            ],
        )
        self.assertEqual(
            layer.post_build_uploads,
            [metrics_file, rrpl_file],
        )

    def test_resultstore_uploads(self) -> None:
        context = self.make_context(
            enable_resultstore=True,
            resultstore_auth_type="auto",
            pre_build_uploads=[pathlib.Path("/path/pre.txt")],
        )
        assert context.resultstore is not None
        self.assertEqual(
            context.resultstore.log_dir.name,
            top_build_wrap._RSPROXY_LOGS_DIR,
        )
        layer = context._compose_resultstore(
            extra_post_build_uploads=[pathlib.Path("/path/post.txt")],
        )
        self.assertIsNotNone(layer)
        assert layer is not None
        self.assertEqual(layer.log_dir.name, top_build_wrap._RSPROXY_LOGS_DIR)
        self.assertEqual(layer.artifacts, {"log_dir": layer.log_dir})
        self.assertEqual(
            layer.prefix,
            [
                self.rsproxy_wrapper,
                "--auth-type",
                "auto",
                "--log-dir",
                str(layer.log_dir),
                "--pre_build_uploads",
                "/path/pre.txt",
                "--post_build_uploads",
                "/path/post.txt",
                "--",
            ],
        )

    def test_wrapper_stack_order(self) -> None:
        cache_file = self.build_dir / "hardware_profile.cache.json"
        cache_file.write_text("{}", encoding="utf-8")

        context = self.make_context(
            tui=True,
            enable_resultstore=True,
            collect_system_profile=True,
            needs_reproxy_rbe=True,
        )
        prefix = context.command_prefix()

        tui_idx = prefix.index(self.tui_wrapper)
        rsproxy_idx = prefix.index(self.rsproxy_wrapper)
        profile_idx = prefix.index(self.profile_wrapper)
        reproxy_idx = prefix.index(self.reproxy_wrapper)

        self.assertLess(tui_idx, rsproxy_idx)
        self.assertLess(rsproxy_idx, profile_idx)
        self.assertLess(profile_idx, reproxy_idx)

    def test_resolve_log_dir_explicit(self) -> None:
        config = top_build_wrap.Config(
            build_dir=self.build_dir,
            log_dir=self.log_dir,
        )
        resolved = top_build_wrap.resolve_log_dir(config, self.tmp_path)
        self.assertEqual(resolved, self.log_dir)

    def test_resolve_log_dir_generates_default(self) -> None:
        config = top_build_wrap.Config(build_dir=self.build_dir)
        resolved = top_build_wrap.resolve_log_dir(config, self.tmp_path)
        self.assertTrue(resolved.exists())
        self.assertEqual(resolved.parent.name, self.build_dir.name)
        self.assertEqual(resolved.parent.parent.name, "_build_logs")

    def test_parse_args(self) -> None:
        argv = [
            "--build-dir",
            str(self.build_dir),
            "--log-dir",
            str(self.log_dir),
            "--rbe=1",
            "--resultstore=all",
            "--tui=0",
            "--verbose",
            "--dry-run",
            "--",
            "ninja",
            "-j",
            "32",
        ]
        config, command = top_build_wrap.parse_args(argv)
        self.assertIsNotNone(config)
        assert config is not None
        self.assertEqual(config.build_dir, self.build_dir)
        self.assertEqual(config.log_dir, self.log_dir)
        self.assertTrue(config.needs_reproxy_rbe)
        self.assertTrue(config.enable_resultstore)
        self.assertFalse(config.tui)
        self.assertTrue(config.verbose)
        self.assertTrue(config.dry_run)
        self.assertEqual(command, ["ninja", "-j", "32"])

    def test_property_access_is_side_effect_free(self) -> None:
        context = self.make_context(
            collect_system_profile=True,
            needs_reproxy_rbe=True,
        )
        # Inspecting properties before setup() or command_prefix() should not create directories
        self.assertFalse(
            (self.log_dir / top_build_wrap._BUILD_PROFILE_DIR).exists()
        )
        self.assertFalse(
            (self.log_dir / top_build_wrap._REPROXY_LOGS_DIR).exists()
        )
        _ = context.profile.artifacts
        _ = context.rbe.log_dir
        self.assertFalse(
            (self.log_dir / top_build_wrap._BUILD_PROFILE_DIR).exists()
        )
        self.assertFalse(
            (self.log_dir / top_build_wrap._REPROXY_LOGS_DIR).exists()
        )

    def test_parse_args_auth_types(self) -> None:
        argv = [
            "--build-dir",
            str(self.build_dir),
            "--rbe-auth-type",
            "machine",
            "--resultstore-auth-type",
            "oauth",
            "--",
            "ninja",
        ]
        config, command = top_build_wrap.parse_args(argv)
        assert config is not None
        self.assertEqual(config.rbe_auth_type, "machine")
        self.assertEqual(config.resultstore_auth_type, "oauth")

    def test_parse_args_invalid_argument_raises_argument_error(self) -> None:
        with self.assertRaises(argparse.ArgumentError):
            top_build_wrap.parse_args(["--profile=invalid", "--", "ninja"])

    def test_parse_args_missing_build_dir(self) -> None:
        with self.assertRaises(ValueError):
            top_build_wrap.parse_args(["--", "ninja"])

    def test_parse_args_missing_command(self) -> None:
        with self.assertRaises(ValueError):
            top_build_wrap.parse_args(["--build-dir", str(self.build_dir)])

    def test_parse_args_help_returns_none(self) -> None:
        with contextlib.redirect_stdout(io.StringIO()):
            config, command = top_build_wrap.parse_args(["--help"])
        self.assertIsNone(config)
        self.assertEqual(command, [])

    def test_str_to_bool(self) -> None:
        self.assertTrue(top_build_wrap._str_to_bool("1"))
        self.assertTrue(top_build_wrap._str_to_bool("true"))
        self.assertTrue(top_build_wrap._str_to_bool("yes"))
        self.assertFalse(top_build_wrap._str_to_bool("0"))
        self.assertFalse(top_build_wrap._str_to_bool("false"))
        self.assertFalse(top_build_wrap._str_to_bool("no"))
        self.assertFalse(top_build_wrap._str_to_bool(""))
        with self.assertRaises(argparse.ArgumentTypeError):
            top_build_wrap._str_to_bool("ninja")
        with self.assertRaises(argparse.ArgumentTypeError):
            top_build_wrap._str_to_bool("invalid")

    def test_parse_resultstore_mode(self) -> None:
        for val in ("all", "ninja", "bazel", "1", "true", "yes"):
            self.assertTrue(top_build_wrap._parse_resultstore_mode(val))
        for val in ("none", "0", "false", "no", ""):
            self.assertFalse(top_build_wrap._parse_resultstore_mode(val))
        with self.assertRaises(argparse.ArgumentTypeError):
            top_build_wrap._parse_resultstore_mode("invalid")

    def test_standalone_cli_dry_run(self) -> None:
        script_path = pathlib.Path(top_build_wrap.__file__).resolve()
        stdout = self.run_dry_run([sys.executable, "-S", str(script_path)])
        self.assertEqual(stdout, "ninja target")


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import pathlib
import unittest
from contextlib import contextmanager
from unittest import mock

import main_build


class MainBuildTestBase(unittest.TestCase):
    """Base class for main_build tests with shared helpers."""

    def setUp(self):
        # Default mock for read_json to avoid file system errors for rbe_settings.json etc.
        self.read_json_patcher = mock.patch(
            "main_build.read_json", return_value={}
        )
        self.mock_read_json = self.read_json_patcher.start()

    def tearDown(self):
        self.read_json_patcher.stop()

    @contextmanager
    def mock_invocation_context(
        self, build_uuid="uuid-123", timestamp="ts-456"
    ):
        """Helper to mock BuildInvocation boilerplate."""
        with mock.patch.object(
            main_build.BuildInvocation,
            "build_uuid",
            new_callable=mock.PropertyMock,
            return_value=build_uuid,
        ):
            with mock.patch.object(
                main_build.BuildInvocation,
                "timestamp",
                new_callable=mock.PropertyMock,
                return_value=timestamp,
            ):
                with mock.patch("main_build.mkdir") as mock_mkdir:
                    with mock.patch("main_build.write_text") as mock_write:
                        yield mock_mkdir, mock_write

    def create_context(self, **config_kwargs):
        """Helper to create a FuchsiaBuildContext with specific config."""
        config_vals = {
            "rbe": False,
            "resultstore": False,
            "profile": False,
            "verbose": False,
            "dry_run": False,
        }
        config_vals.update(config_kwargs)
        config = main_build.FuchsiaBuildConfig(**config_vals)
        return main_build.FuchsiaBuildContext(
            source_dir=pathlib.Path("/tmp/fuchsia"),
            out_dir=pathlib.Path("/tmp/out"),
            build_dir=pathlib.Path("/tmp/out/default"),
            env={},
            config=config,
        )


class FuchsiaBuildContextTest(MainBuildTestBase):
    def test_properties(self):
        source_dir = pathlib.Path("/tmp/fuchsia")
        out_dir = pathlib.Path("/tmp/out")
        build_dir = out_dir / "default"
        context = self.create_context()
        context.source_dir = source_dir
        context.out_dir = out_dir
        context.build_dir = build_dir

        self.assertEqual(
            context.rbe_settings_file, build_dir / "rbe_settings.json"
        )
        self.assertEqual(context.rbe_config_json, build_dir / "rbe_config.json")
        self.assertEqual(
            context.check_loas_script,
            source_dir / "build/rbe/check_loas_restrictions.sh",
        )
        self.assertEqual(
            context.top_build_wrapper,
            source_dir / "build/scripts/top_build_wrap.sh",
        )
        self.assertEqual(context.args_gn, build_dir / "args.gn")
        self.assertEqual(
            context.rsninja_sh, source_dir / "build/resultstore/rsninja.sh"
        )
        self.assertEqual(
            context.ninja_edge_weights_csv, build_dir / "ninja_edge_weights.csv"
        )

    def test_loas_type_skip_when_no_auth(self):
        context = self.create_context(resultstore=False)
        with mock.patch.object(
            main_build.FuchsiaBuildContext,
            "needs_auth",
            new_callable=mock.PropertyMock,
            return_value=False,
        ):
            self.assertEqual(context.loas_type, "skip")

    def test_loas_type_detected_when_needs_auth(self):
        context = self.create_context()
        context.env = {"FOO": "BAR"}
        with mock.patch.object(
            main_build.FuchsiaBuildContext,
            "needs_auth",
            new_callable=mock.PropertyMock,
            return_value=True,
        ):
            with mock.patch("main_build.is_executable", return_value=True):
                with mock.patch(
                    "subprocess.check_output",
                    return_value="some output\nrestricted\n",
                ) as mock_sub:
                    self.assertEqual(context.loas_type, "restricted")
                    mock_sub.assert_called_once_with(
                        [str(context.check_loas_script)],
                        text=True,
                        stderr=mock.ANY,
                        env=context.env,
                    )

    def test_rbe_settings_missing_throws(self):
        context = self.create_context(rbe=None)
        self.mock_read_json.side_effect = main_build.BuildConfigurationError(
            "missing file"
        )
        with self.assertRaises(main_build.BuildConfigurationError) as cm:
            _ = context.rbe_enabled
        self.assertEqual(str(cm.exception), "missing file")


class BuildInvocationTest(MainBuildTestBase):
    def test_init_caching(self):
        context = self.create_context()
        with self.mock_invocation_context("uuid-123", "ts-456") as (
            mock_mkdir,
            mock_write,
        ):
            invocation = main_build.BuildInvocation(context)
            self.assertEqual(invocation.build_uuid, "uuid-123")
            self.assertEqual(invocation.timestamp, "ts-456")
            log_dir = pathlib.Path(
                "/tmp/out/_build_logs/default/build.ts-456.uuid-123"
            )
            self.assertEqual(str(invocation.log_dir), str(log_dir))

            expected_mkdir_calls = [
                mock.call(pathlib.Path("/tmp/out/_build_logs/default")),
                mock.call(log_dir),
            ]
            mock_mkdir.assert_has_calls(expected_mkdir_calls)
            mock_write.assert_called_once_with(
                log_dir / "invocation_id", "uuid-123\n"
            )

    def test_get_build_env(self):
        context = self.create_context()
        context.env = {"TERM": "xterm", "USER": "fuchsia-user", "EXTRA": "val"}
        with self.mock_invocation_context("uuid-123", "ts-456"):
            invocation = main_build.BuildInvocation(context)
            env = invocation.get_build_env()
            self.assertEqual(env["FX_BUILD_UUID"], "uuid-123")
            self.assertEqual(env["TERM"], "xterm")
            self.assertEqual(env["USER"], "fuchsia-user")
            self.assertNotIn("EXTRA", env)

    def test_get_build_env_missing_user_error(self):
        context = self.create_context()
        context.env = {}  # No USER
        with self.mock_invocation_context():
            invocation = main_build.BuildInvocation(context)
            with mock.patch.object(
                main_build.FuchsiaBuildContext,
                "needs_auth",
                new_callable=mock.PropertyMock,
                return_value=True,
            ):
                with mock.patch("os.getlogin", side_effect=OSError()):
                    with self.assertRaises(
                        main_build.BuildConfigurationError
                    ) as cm:
                        invocation.get_build_env()
                    self.assertIn(
                        "USER environment variable is not set",
                        str(cm.exception),
                    )


class BuildCommandExecutionTest(unittest.TestCase):
    @mock.patch("main_build.BuildLock")
    @mock.patch("subprocess.call", return_value=0)
    def test_run(self, mock_call, mock_lock):
        # We still need context and invocation for the execution object
        # Create them manually to avoid TestBase dependency
        config = main_build.FuchsiaBuildConfig(
            rbe=False,
            resultstore=False,
            profile=False,
            verbose=False,
            dry_run=False,
        )
        context = main_build.FuchsiaBuildContext(
            source_dir=pathlib.Path("/tmp/fuchsia"),
            out_dir=pathlib.Path("/tmp/out"),
            build_dir=pathlib.Path("/tmp/out/default"),
            env={},
            config=config,
        )
        with mock.patch.object(
            main_build.BuildInvocation,
            "build_uuid",
            new_callable=mock.PropertyMock,
            return_value="uuid-123",
        ):
            with mock.patch.object(
                main_build.BuildInvocation,
                "timestamp",
                new_callable=mock.PropertyMock,
                return_value="ts",
            ):
                with mock.patch("main_build.mkdir"):
                    with mock.patch("main_build.write_text"):
                        invocation = main_build.BuildInvocation(context)

        exec_info = main_build.BuildCommandExecution(
            full_command=["cmd", "arg"],
            env={"VAR": "VAL"},
            invocation=invocation,
            cleanup_files=[pathlib.Path("/tmp/cleanup")],
        )

        with mock.patch("main_build.exists", return_value=True):
            with mock.patch("pathlib.Path.unlink") as mock_unlink:
                result = exec_info.run()
                self.assertEqual(result.return_code, 0)
                mock_call.assert_called_once_with(
                    ["cmd", "arg"], env={"VAR": "VAL"}
                )
                mock_unlink.assert_called_once_with(missing_ok=True)


class BuildLockTest(unittest.TestCase):
    @mock.patch("main_build.check_shell_command", return_value=True)
    @mock.patch("subprocess.call")
    @mock.patch("builtins.print")
    def test_acquire_lock_success(self, mock_print, mock_call, mock_check):
        mock_call.return_value = 0
        build_dir = pathlib.Path("/tmp/build")
        with main_build.BuildLock(build_dir):
            pass
        mock_call.assert_called_with(
            [
                "shlock",
                "-f",
                str(build_dir.with_suffix(".build_lock")),
                "-p",
                mock.ANY,
            ]
        )
        mock_print.assert_called_with("Lock acquired, proceeding with build.")

    @mock.patch("main_build.check_shell_command", return_value=True)
    @mock.patch("subprocess.call")
    @mock.patch("time.sleep")
    @mock.patch("builtins.print")
    def test_acquire_lock_retries(
        self, mock_print, mock_sleep, mock_call, mock_check
    ):
        mock_call.side_effect = [1, 0]
        build_dir = pathlib.Path("/tmp/build")
        with main_build.BuildLock(build_dir):
            pass
        self.assertEqual(mock_call.call_count, 2)
        mock_sleep.assert_called_once()
        mock_print.assert_called_with("Lock acquired, proceeding with build.")


class FindFuchsiaDirTest(unittest.TestCase):
    def test_find_success(self):
        # Mock exists() at the module level
        with mock.patch("main_build.exists") as mock_exists:
            # .jiri_manifest checks:
            # 1. /tmp/a/b/c/.jiri_manifest -> False
            # 2. /tmp/a/b/.jiri_manifest -> False
            # 3. /tmp/a/.jiri_manifest -> True
            mock_exists.side_effect = [False, False, True]

            start = pathlib.Path("/tmp/a/b/c")
            res = main_build.find_fuchsia_dir(start)

            self.assertEqual(res, pathlib.Path("/tmp/a"))
            self.assertEqual(mock_exists.call_count, 3)

    def test_find_failure(self):
        with mock.patch.object(pathlib.Path, "exists", return_value=False):
            with self.assertRaises(ValueError):
                main_build.find_fuchsia_dir(pathlib.Path("/tmp/only/two"))


class StrToBoolTest(unittest.TestCase):
    def test_str_to_bool(self):
        self.assertTrue(main_build.str_to_bool("true"))
        self.assertTrue(main_build.str_to_bool("1"))
        self.assertTrue(main_build.str_to_bool("yes"))
        self.assertFalse(main_build.str_to_bool("false"))
        self.assertFalse(main_build.str_to_bool("0"))
        self.assertFalse(main_build.str_to_bool("no"))
        with self.assertRaises(Exception):
            main_build.str_to_bool("maybe")


class ChooseConcurrencyTest(unittest.TestCase):
    def test_local(self):
        with mock.patch("main_build.get_cpu_count", return_value=8):
            self.assertEqual(
                main_build.choose_concurrency(rbe_enabled=False), 8
            )

    def test_rbe(self):
        with mock.patch("main_build.get_cpu_count", return_value=8):
            self.assertEqual(
                main_build.choose_concurrency(rbe_enabled=True), 80
            )


class TopBuildCommandPrefixTest(MainBuildTestBase):
    def test_basic(self):
        context = self.create_context(rbe=False, resultstore=False)
        with self.mock_invocation_context():
            invocation = main_build.BuildInvocation(context)
            prefix = main_build.top_build_command_prefix(invocation)
            self.assertIn(
                "/tmp/fuchsia/build/scripts/top_build_wrap.sh", prefix[0]
            )
            self.assertIn("--build-dir", prefix)
            self.assertNotIn("--rbe", prefix)

    def test_rbe_resultstore(self):
        context = self.create_context(rbe=True, resultstore=True)
        with mock.patch.multiple(
            main_build.FuchsiaBuildContext,
            rbe_enabled=mock.PropertyMock(return_value=True),
            get_rbe_reproxy_configs=lambda s: [pathlib.Path("cfg")],
        ):
            with self.mock_invocation_context():
                invocation = main_build.BuildInvocation(context)
                prefix = main_build.top_build_command_prefix(invocation)
                self.assertIn("--rbe", prefix)
                self.assertIn("--reproxy-cfg", prefix)
                self.assertIn("--resultstore", prefix)


class InjectNinjaArgsTest(MainBuildTestBase):
    def test_injection(self):
        context = self.create_context()
        with self.mock_invocation_context() as (mock_mkdir, _):
            invocation = main_build.BuildInvocation(context)
            cmd = ["ninja", "target"]
            injected = main_build.inject_ninja_args(invocation, cmd)
            self.assertEqual(injected[0], "ninja")
            self.assertIn("--dirty_sources_list", injected)
            self.assertIn("--action_metrics_output", injected)
            self.assertEqual(injected[-1], "target")
            mock_mkdir.assert_any_call(invocation.log_dir / "ninja_logs")


class NewBuildCommandExecutionTest(MainBuildTestBase):
    def test_new_build_command_execution_ninja(self):
        context = self.create_context()
        with self.mock_invocation_context("uuid-123", "ts-456"):
            invocation = main_build.BuildInvocation(context)
            with mock.patch.multiple(
                main_build.FuchsiaBuildContext,
                rbe_enabled=mock.PropertyMock(return_value=False),
                needs_auth=mock.PropertyMock(return_value=False),
            ):
                with mock.patch("main_build.mkdir"):
                    exec_info = main_build.new_build_command_execution(
                        invocation, "ninja", ["ninja", "target"]
                    )
                    self.assertEqual(
                        exec_info.full_command[0],
                        str(context.top_build_wrapper),
                    )
                    self.assertIn("--", exec_info.full_command)
                    self.assertEqual(exec_info.env["FX_BUILD_UUID"], "uuid-123")


class PrepareFunctionsTest(MainBuildTestBase):
    def test_bazel(self):
        context = self.create_context()
        with self.mock_invocation_context():
            exec_info = main_build.new_bazel_build_command_execution(
                context, ["build", "target"]
            )
            self.assertIsInstance(exec_info, main_build.BuildCommandExecution)
            self.assertIn("bazel", exec_info.full_command)

    def test_fint(self):
        context = self.create_context()
        with self.mock_invocation_context():
            with mock.patch("tempfile.NamedTemporaryFile") as mock_tmp:
                mock_tmp.return_value.__enter__.return_value.name = (
                    "/tmp/fint.proto"
                )
                exec_info = main_build.new_fint_build_command_execution(
                    context, ["fint", "build"]
                )
                self.assertIsInstance(
                    exec_info, main_build.BuildCommandExecution
                )
                self.assertIn(
                    "/tmp/fint.proto", [str(p) for p in exec_info.cleanup_files]
                )

    def test_ninja_missing_j_arg(self):
        context = self.create_context()
        with self.assertRaises(main_build.BuildConfigurationError) as cm:
            main_build.new_ninja_build_command_execution(context, ["-j"])
        self.assertEqual(str(cm.exception), "-j requires an argument")


class CheckShellCommandTest(unittest.TestCase):
    @mock.patch("shutil.which", return_value="/usr/bin/ls")
    def test_success(self, mock_which):
        self.assertTrue(main_build.check_shell_command("ls"))
        mock_which.assert_called_once_with("ls")

    @mock.patch("shutil.which", return_value=None)
    def test_failure(self, mock_which):
        self.assertFalse(main_build.check_shell_command("nonexistent"))


class MainFunctionTest(MainBuildTestBase):
    def test_arg_parser_defaults(self):
        args = main_build._MAIN_ARG_PARSER.parse_args(
            ["--build-dir", "out/default", "ninja"]
        )
        self.assertIsNone(args.rbe)
        self.assertIsNone(args.resultstore)
        self.assertFalse(args.verbose)

    def test_main_catches_config_error(self):
        with mock.patch.object(
            main_build._MAIN_ARG_PARSER, "parse_known_args"
        ) as mock_parse:
            mock_args = mock.Mock()
            mock_parse.return_value = (mock_args, [])
            mock_args.func.side_effect = main_build.BuildConfigurationError(
                "test error"
            )
            with mock.patch("main_build.FuchsiaBuildContext.from_args"):
                with mock.patch("builtins.print") as mock_print:
                    rc = main_build.main(
                        ["--build-dir", "out/default", "ninja"]
                    )
                    self.assertEqual(rc, 1)
                    mock_print.assert_called_with("Error: test error")


if __name__ == "__main__":
    unittest.main()

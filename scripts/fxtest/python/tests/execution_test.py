# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import json
import os
import tempfile
import typing
import unittest
import unittest.mock as mock

import async_utils.command as command
from parameterized import parameterized

import args
import environment
import event
import execution
import test_list_file
import tests_json_file


def _make_exec_env(
    fuchsia_dir: str, out_dir: str, gemini_api_key: typing.Optional[str] = None
) -> environment.ExecutionEnvironment:
    """Create an execution environment for test."""
    return environment.ExecutionEnvironment(
        fuchsia_dir=fuchsia_dir,
        out_dir=out_dir,
        test_json_file="",
        disabled_ctf_tests_file="",
        log_file=None,
        test_list_file="",
        gemini_api_key=gemini_api_key,
    )


class TestExecution(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        super().setUp()
        patch = mock.patch.dict(os.environ, {}, clear=False)
        patch.start()
        os.environ.pop("FUCHSIA_NODENAME", None)
        self.addCleanup(patch.stop)

    def assertContainsSublist(
        self, target: list[typing.Any], data: list[typing.Any]
    ) -> None:
        """Helper method to assert that one list is contained in the other, in order.

        Args:
            target (list[typing.Any]): The sublist to search for.
            data (list[typing.Any]): The list to search in.
        """
        self.assertNotEqual(len(target), 0, "Target list cannot be empty")
        starts = [i for i, v in enumerate(data) if v == target[0]]
        for start_index in starts:
            if data[start_index : start_index + len(target)] == target:
                return
        self.assertTrue(False, f"List {target} is not a sublist of {data}")

    async def test_run_command(self) -> None:
        """Test that run_command works with and without events"""
        with tempfile.TemporaryDirectory() as tmp:
            open(os.path.join(tmp, "temp-file.txt"), "w").close()

            output = await execution.run_command(
                "ls", "temp-file.txt", env={"CWD": tmp}
            )
            assert output is not None
            self.assertEqual(output.return_code, 0)

            recorder = event.EventRecorder()
            recorder.emit_init()

            output = await execution.run_command(
                "ls", "temp-file.txt", env={"CWD": tmp}, recorder=recorder
            )
            assert output is not None
            self.assertEqual(output.return_code, 0)

            recorder.emit_end()

            events = [e async for e in recorder.iter()]
            # Ensure we got init, end, and at least one sub-event start/stop
            self.assertGreater(len(events), 4)

    async def test_run_command_failure(self) -> None:
        """Test that running an invalid command emits an error event"""
        recorder = event.EventRecorder()
        recorder.emit_init()
        output = await execution.run_command(
            "___invalid_command_name___", recorder=recorder
        )
        recorder.emit_end()
        events = [e async for e in recorder.iter()]

        # Ensure we get no output and that at least one event is an error.
        self.assertIsNone(output)
        self.assertTrue(any([e.error is not None for e in events]))

    @parameterized.expand(
        [
            (
                "with default log severity",
                [],
                [["--max-severity-logs", "INFO"]],
                [],
            ),
            (
                "with default capture syslog",
                [],
                [["--capture-syslog"]],
                [],
            ),
            (
                "without capture syslog",
                ["--no-capture-syslog"],
                [],
                ["--capture-syslog"],
            ),
            (
                "with min log severity override",
                ["--min-severity-logs", "DEBUG"],
                [["--min-severity-logs", "DEBUG"]],
                [],
            ),
            (
                "without log restriction",
                ["--no-restrict-logs"],
                [["--min-severity-logs", "TRACE"]],
                ["--max-severity-logs"],
            ),
            (
                "with default to not run disabled tests",
                [],
                [["--min-severity-logs", "TRACE"]],
                ["--run-disabled"],
            ),
            (
                "with disabled tests running",
                ["--also-run-disabled-tests"],
                [["--run-disabled"], ["--min-severity-logs", "TRACE"]],
                [],
            ),
            (
                "with full moniker in logs",
                ["--show-full-moniker-in-logs"],
                [["--show-full-moniker-in-logs"]],
                [],
            ),
            (
                "without full moniker in logs",
                ["--no-show-full-moniker-in-logs"],
                [],
                ["--show-full-moniker-in-logs"],
            ),
            (
                "without ffx output directory",
                [],
                [],
                ["--output-directory"],
            ),
            (
                "with ffx output directory",
                ["--ffx-output-directory", "foo"],
                [["--output-directory", "foo/0"]],
                [],
            ),
            (
                "with extra args",
                ["--", "--foo"],
                [["--", "--foo"]],
                [],
            ),
        ]
    )
    async def test_test_execution_component(
        self,
        _unused_name: str,
        flag_list: list[str],
        expected_present_flag_lists: list[list[str]],
        expected_not_present_flags: list[str],
    ) -> None:
        """Test the usage of the TestExecution wrapper on a component test"""

        exec_env = _make_exec_env("/fuchsia", "/out/fuchsia")

        test = execution.TestExecution(
            test_list_file.Test(
                tests_json_file.TestEntry(
                    tests_json_file.TestSection("foo", "//foo", "fuchsia")
                ),
                test_list_file.TestListEntry(
                    "foo",
                    [],
                    test_list_file.TestListExecutionEntry(
                        "fuchsia-pkg://fuchsia.com/foo#meta/foo_test.cm",
                        realm="foo_tests",
                        max_severity_logs="INFO",
                        min_severity_logs="TRACE",
                        test_filters=["-foo", "-bar"],
                    ),
                ),
            ),
            exec_env,
            args.parse_args(["--no-use-package-hash"] + flag_list),
        )

        command_line = test.command_line()
        self.assertContainsSublist(
            [
                "fx",
                "--dir",
                "/out/fuchsia",
                "ffx",
                "test",
                "run",
                "--realm",
                "foo_tests",
            ],
            command_line,
        )
        self.assertContainsSublist(
            [
                "--test-filter",
                "-foo",
                "--test-filter",
                "-bar",
            ],
            command_line,
        )
        self.assertContainsSublist(
            ["fuchsia-pkg://fuchsia.com/foo#meta/foo_test.cm"], command_line
        )

        for expected_flag_list in expected_present_flag_lists:
            self.assertContainsSublist(expected_flag_list, command_line)
        for not_present_flag in expected_not_present_flags:
            self.assertFalse(
                not_present_flag in command_line,
                f"Expected {not_present_flag} to not be in {command_line}",
            )

        self.assertFalse(test.is_hermetic())
        self.assertFalse(test.is_host_test())
        self.assertIsNone(test.environment())
        self.assertTrue(test.should_symbolize())

    @parameterized.expand(
        [
            (
                "test execution does not pass a --parallel by default",
                None,
                None,
                [],
            ),
            (
                "test execution respects parallel overrides in test specs",
                1,
                None,
                ["--parallel", "1"],
            ),
            (
                "test execution flag overrides test spec",
                4,
                2,
                ["--parallel", "2"],
            ),
        ]
    )
    async def test_test_execution_parallel_cases(
        self,
        _unused_name: str,
        spec_parallel: int | None,
        flag_parallel: int | None,
        expected_args: list[str],
    ) -> None:
        """Test the usage of the TestExecution wrapper on a component test"""
        exec_env = _make_exec_env("/fuchsia", "/out/fuchsia")

        test = execution.TestExecution(
            test_list_file.Test(
                tests_json_file.TestEntry(
                    tests_json_file.TestSection(
                        "foo", "//foo", "fuchsia", parallel=spec_parallel
                    )
                ),
                test_list_file.TestListEntry(
                    "foo",
                    [],
                    test_list_file.TestListExecutionEntry(
                        "fuchsia-pkg://fuchsia.com/foo#meta/foo_test.cm",
                        realm="foo_tests",
                        max_severity_logs="INFO",
                        min_severity_logs="TRACE",
                    ),
                ),
            ),
            exec_env,
            args.parse_args(
                ["--no-use-package-hash"]
                + (
                    []
                    if not flag_parallel
                    else ["--parallel-cases", str(flag_parallel)]
                )
            ),
        )

        self.assertListEqual(
            test.command_line(),
            [
                "fx",
                "--dir",
                "/out/fuchsia",
                "ffx",
                "test",
                "run",
                "--realm",
                "foo_tests",
                "--max-severity-logs",
                "INFO",
                "--min-severity-logs",
                "TRACE",
            ]
            + expected_args
            + [
                "--capture-syslog",
                "--timeout",
                "300",
                "fuchsia-pkg://fuchsia.com/foo#meta/foo_test.cm",
            ],
        )

        self.assertFalse(test.is_hermetic())
        self.assertFalse(test.is_host_test())
        self.assertIsNone(test.environment())
        self.assertTrue(test.should_symbolize())

    @parameterized.expand(
        [
            (
                "test execution does not pass a --no-exception-channel by default",
                None,
                [],
            ),
            (
                "test execution respects create_no_exception_channel in test specs",
                True,
                ["--no-exception-channel"],
            ),
        ]
    )
    async def test_test_execution_create_no_exception_channel(
        self,
        _unused_name: str,
        spec_create_no_exception_channel: bool | None,
        expected_args: list[str],
    ) -> None:
        """Test the usage of the TestExecution wrapper on a component test"""
        exec_env = _make_exec_env("/fuchsia", "/out/fuchsia")

        test = execution.TestExecution(
            test_list_file.Test(
                tests_json_file.TestEntry(
                    tests_json_file.TestSection(
                        "foo",
                        "//foo",
                        "fuchsia",
                        create_no_exception_channel=spec_create_no_exception_channel,
                    )
                ),
                test_list_file.TestListEntry(
                    "foo",
                    [],
                    test_list_file.TestListExecutionEntry(
                        "fuchsia-pkg://fuchsia.com/foo#meta/foo_test.cm",
                        realm="foo_tests",
                        max_severity_logs="INFO",
                        min_severity_logs="TRACE",
                    ),
                ),
            ),
            exec_env,
            args.parse_args(["--no-use-package-hash"]),
        )

        self.assertListEqual(
            test.command_line(),
            [
                "fx",
                "--dir",
                "/out/fuchsia",
                "ffx",
                "test",
                "run",
                "--realm",
                "foo_tests",
                "--max-severity-logs",
                "INFO",
                "--min-severity-logs",
                "TRACE",
            ]
            + expected_args
            + [
                "--capture-syslog",
                "--timeout",
                "300",
                "fuchsia-pkg://fuchsia.com/foo#meta/foo_test.cm",
            ],
        )

        self.assertFalse(test.is_hermetic())
        self.assertFalse(test.is_host_test())
        self.assertIsNone(test.environment())
        self.assertTrue(test.should_symbolize())

    async def test_test_execution_component_with_experimental_new_path(
        self,
    ) -> None:
        """Test the usage of the TestExecution wrapper on a component test while using test interface"""

        exec_env = _make_exec_env("/fuchsia", "/out/fuchsia")

        test = execution.TestExecution(
            test_list_file.Test(
                tests_json_file.TestEntry(
                    tests_json_file.TestSection(
                        "foo",
                        "//foo",
                        "fuchsia",
                        new_path="bin/test_component_wrapper.sh",
                    )
                ),
                test_list_file.TestListEntry(
                    "foo",
                    [],
                    test_list_file.TestListExecutionEntry(
                        "fuchsia-pkg://fuchsia.com/foo#meta/foo_test.cm",
                        realm="foo_tests",
                        max_severity_logs="INFO",
                        min_severity_logs="TRACE",
                    ),
                ),
            ),
            exec_env,
            args.parse_args(["--use-test-pilot", "--", "--some_extra_arg"]),
        )

        command_line = test.command_line()
        self.assertEqual(
            [
                "/out/fuchsia/bin/test_component_wrapper.sh",
                "--realm=foo_tests",
                "--max-severity-logs=INFO",
                "--min-severity-logs=TRACE",
                "--target-test-args=--some_extra_arg",
                "--timeout=300",
            ],
            command_line,
        )
        env = test.environment()
        assert env is not None
        # TODO: Add environment checking when added.
        expected_env_subset = {
            "CWD": "/out/fuchsia",
            "FUCHSIA_CUSTOM_TEST_ARGS": "--some_extra_arg",
        }
        self.assertTrue(expected_env_subset.items() <= env.items())

        # without --use-test-pilot flag
        test = execution.TestExecution(
            test_list_file.Test(
                tests_json_file.TestEntry(
                    tests_json_file.TestSection(
                        "foo",
                        "//foo",
                        "fuchsia",
                        new_path="bin/test_component_wrapper.sh",
                    )
                ),
                test_list_file.TestListEntry(
                    "foo",
                    [],
                    test_list_file.TestListExecutionEntry(
                        "fuchsia-pkg://fuchsia.com/foo#meta/foo_test.cm",
                        realm="foo_tests",
                        max_severity_logs="INFO",
                        min_severity_logs="TRACE",
                    ),
                ),
            ),
            exec_env,
            args.parse_args(
                ["--no-use-package-hash", "--", "--some_extra_arg"]
            ),
        )

        command_line = test.command_line()
        self.assertContainsSublist(
            [
                "fx",
                "--dir",
                "/out/fuchsia",
                "ffx",
                "test",
                "run",
                "--realm",
                "foo_tests",
            ],
            command_line,
        )
        self.assertContainsSublist(
            ["fuchsia-pkg://fuchsia.com/foo#meta/foo_test.cm"], command_line
        )

        self.assertContainsSublist(["--", "--some_extra_arg"], command_line)

        self.assertIsNone(test.environment())

    # TODO(b/444203259): Failing on debian 12 Linux. Re-enable once a fix is implemented.
    # async def test_test_execution_host(self) -> None:
    #     """Test the usage of the TestExecution wrapper on a host test, and actually run it"""

    #     with tempfile.TemporaryDirectory() as tmp:
    #         # We will run ls, but it needs to be relative to the output directory.
    #         # Find the actual path to the ls binary and symlink it into the
    #         # output directory.
    #         env_path = (
    #             subprocess.check_output(["which", "env"]).decode().strip()
    #         )
    #         self.assertTrue(os.path.isfile, f"{env_path} is not a file")
    #         os.symlink(env_path, os.path.join(tmp, "env"))

    #         exec_env = _make_exec_env("/fuchsia", tmp)

    #         flags = args.parse_args([])

    #         test = execution.TestExecution(
    #             test_list_file.Test(
    #                 tests_json_file.TestEntry(
    #                     tests_json_file.TestSection(
    #                         "foo", "//foo", "linux", path="env"
    #                     )
    #                 ),
    #                 test_list_file.TestListEntry("foo", [], execution=None),
    #             ),
    #             exec_env,
    #             flags,
    #         )

    #         self.assertFalse(test.is_hermetic())
    #         env = test.environment()
    #         assert env is not None
    #         self.assertDictEqual(env, {"CWD": tmp})
    #         self.assertFalse(test.should_symbolize())

    #         recorder = event.EventRecorder()
    #         recorder.emit_init()

    #         output = await test.run(recorder, flags, event.GLOBAL_RUN_ID)
    #         recorder.emit_end()

    #         assert output is not None

    #         lines = output.stdout.splitlines()
    #         outdir: None | str = None
    #         for line in lines:
    #             l = line.split("=")
    #             if len(l) > 1 and l[0] == "FUCHSIA_TEST_OUTDIR":
    #                 outdir = l[1]
    #         assert outdir is not None
    #         self.assertEqual(
    #             outdir[:4],
    #             "/tmp",
    #             f"Expecting that the output directory is an absolute path into /tmp. Found {outdir}",
    #         )

    #         self.assertFalse(
    #             any([e.error is not None async for e in recorder.iter()])
    #         )

    @parameterized.expand(
        [
            ("without --e2e enabled", ["--no-e2e"], execution.TestSkipped),
            ("with --e2e enabled", ["--e2e"], execution.TestFailed),
        ]
    )
    @mock.patch("execution.run_command", return_value=None)
    async def test_test_execution_e2e(
        self,
        _unused_name: str,
        extra_flags: list[str],
        expected_exception: type,
        command_mock: mock.MagicMock,
    ) -> None:
        """Test that execution of e2e depends on the --e2e flag"""

        exec_env = _make_exec_env("/fuchsia", "/out/fuchsia")

        flags = args.parse_args(extra_flags)

        test = execution.TestExecution(
            test_list_file.Test(
                tests_json_file.TestEntry(
                    tests_json_file.TestSection(
                        "foo", "//foo", "linux", path="ls"
                    ),
                    environments=[
                        tests_json_file.EnvironmentEntry(
                            dimensions=tests_json_file.DimensionsEntry(
                                device_type="AEMU"
                            )
                        )
                    ],
                ),
                test_list_file.TestListEntry("foo", [], execution=None),
            ),
            exec_env,
            flags,
        )

        self.assertTrue(test._test.is_e2e_test())
        self.assertFalse(test.is_hermetic())
        self.assertFalse(test.is_host_test())
        env = test.environment()
        assert env is not None
        self.assertDictEqual(env, {"CWD": "/out/fuchsia"})
        self.assertFalse(test.should_symbolize())

        recorder = event.EventRecorder()
        recorder.emit_init()

        try:
            await test.run(recorder, flags, event.GLOBAL_RUN_ID)
            self.assertTrue(False, "No exception was raised")
        except Exception as e:
            self.assertIsInstance(e, expected_exception)
        recorder.emit_end()

    @mock.patch("execution.run_command", return_value=None)
    async def test_test_execution_skip_boot_tests(
        self, command_mock: mock.MagicMock
    ) -> None:
        """Test that boot tests are skipped"""

        exec_env = _make_exec_env("/fuchsia", "/out/fuchsia")

        flags = args.parse_args([])

        test = execution.TestExecution(
            test_list_file.Test(
                tests_json_file.TestEntry(
                    tests_json_file.TestSection(
                        "foo", "//foo", "linux", path="ls"
                    ),
                    environments=[
                        tests_json_file.EnvironmentEntry(
                            dimensions=tests_json_file.DimensionsEntry(
                                device_type="AEMU"
                            )
                        )
                    ],
                    is_boot_test=True,
                ),
                test_list_file.TestListEntry("foo", [], execution=None),
            ),
            exec_env,
            flags,
        )

        self.assertFalse(test._test.is_e2e_test())
        self.assertFalse(test.is_hermetic())
        env = test.environment()
        assert env is not None
        self.assertDictEqual(env, {"CWD": "/out/fuchsia"})
        self.assertFalse(test.should_symbolize())

        recorder = event.EventRecorder()
        recorder.emit_init()

        try:
            await test.run(recorder, flags, event.GLOBAL_RUN_ID)
            self.assertTrue(False, "No exception was raised")
        except Exception as e:
            self.assertIsInstance(e, execution.TestSkipped)
            self.assertEqual(
                str(e),
                "Boot tests are not supported by `fx test`. Use `fx run-boot-test` or `fx core-tests`.",
            )
        recorder.emit_end()

    async def test_test_execution_with_package_hash(self) -> None:
        """Ensure that test execution respects --use-package-hash"""
        with tempfile.TemporaryDirectory() as tmp:
            with open(os.path.join(tmp, "package-repositories.json"), "w") as f:
                json.dump(
                    [
                        {
                            "targets": "targets.json",
                        }
                    ],
                    f,
                )
            with open(os.path.join(tmp, "targets.json"), "w") as f:
                json.dump(
                    {
                        "signed": {
                            "targets": {
                                "foo_test/0": {
                                    "custom": {
                                        "merkle": "f00",
                                    }
                                },
                                "bar_test/0": {},
                            }
                        }
                    },
                    f,
                )

            def make_test(name: str) -> test_list_file.Test:
                name = f"fuchsia-pkg://fuchsia.com/{name}#meta/test.cm"
                return test_list_file.Test(
                    tests_json_file.TestEntry(
                        tests_json_file.TestSection(name, "//foo", "linux")
                    ),
                    test_list_file.TestListEntry(
                        name,
                        [],
                        execution=test_list_file.TestListExecutionEntry(name),
                    ),
                )

            def assertTestExecutionFailsUsingMerkleHash(
                error_regex: str,
                test: test_list_file.Test,
                exec_env: environment.ExecutionEnvironment,
            ) -> None:
                self.assertRaisesRegex(
                    execution.TestCouldNotRun,
                    error_regex,
                    lambda: execution.TestExecution(
                        test, exec_env, args.parse_args([])
                    ).command_line(),
                )
                self.assertIsNotNone(
                    execution.TestExecution(
                        test,
                        exec_env,
                        args.parse_args(["--no-use-package-hash"]),
                    ).command_line()
                )

            # This environment is missing a package-repository.json file, so attempts
            # to match a merkle root will fail.
            missing_exec_env = environment.ExecutionEnvironment(
                fuchsia_dir="/fuchsia",
                out_dir="",
                test_json_file="",
                disabled_ctf_tests_file="",
            )

            assertTestExecutionFailsUsingMerkleHash(
                r"(?s)Could not load a Merkle hash.*Try running fx build\.",
                make_test("foo_test"),
                missing_exec_env,
            )

            # This environment contains a package repository, so we need to
            # ensure the merkle hash argument is respected.
            exec_env = environment.ExecutionEnvironment(
                fuchsia_dir="/fuchsia",
                out_dir="",
                test_json_file="",
                disabled_ctf_tests_file="",
                package_repositories_file=os.path.join(
                    tmp, "package-repositories.json"
                ),
            )

            # Hash is present only when use_merkle_hash is True, absent otherwise.
            self.assertIn(
                "fuchsia-pkg://fuchsia.com/foo_test?hash=f00#meta/test.cm",
                execution.TestExecution(
                    make_test("foo_test"), exec_env, args.parse_args([])
                ).command_line(),
            )
            self.assertIn(
                "fuchsia-pkg://fuchsia.com/foo_test#meta/test.cm",
                execution.TestExecution(
                    make_test("foo_test"),
                    exec_env,
                    args.parse_args(["--no-use-package-hash"]),
                ).command_line(),
            )

            # Mangle the component URL such that a name cannot be extracted,
            # and expect an error.
            broken_test = make_test("foo_test")
            assert broken_test.info is not None
            assert broken_test.info.execution is not None
            broken_test.info.execution.component_url = "foo_test"

            assertTestExecutionFailsUsingMerkleHash(
                r"(?s)Failed to parse package name for Merkle root matching.*foo_test.*",
                broken_test,
                exec_env,
            )

            # This test has an entry, but no merkle.
            assertTestExecutionFailsUsingMerkleHash(
                r"(?s)Could not find a Merkle hash for this test.*Try running fx build\.",
                make_test("bar_test"),
                exec_env,
            )

            # This test has no entry.
            assertTestExecutionFailsUsingMerkleHash(
                r"(?s)Could not find a Merkle hash for this test.*Try running fx build\.",
                make_test("baz_test"),
                exec_env,
            )

    def _make_command_output(
        self, stdout: str, return_code: int = 0
    ) -> command.CommandOutput:
        return command.CommandOutput(stdout, "", return_code, 0.2, None)

    @mock.patch("execution.run_command")
    async def test_gemini_analysis_not_called_on_success(
        self, command_mock: mock.AsyncMock
    ) -> None:
        """test that gemini analysis is not called on a successful run."""

        exec_env = _make_exec_env("/fuchsia", "/out/fuchsia")
        flags = args.parse_args(["--gemini-analysis"])

        test = execution.TestExecution(
            test_list_file.Test(
                tests_json_file.TestEntry(
                    tests_json_file.TestSection(
                        "foo", "//foo", "linux", path="ls"
                    )
                ),
                test_list_file.TestListEntry("foo", [], execution=None),
            ),
            exec_env,
            flags,
        )

        # Simulate a successful command
        command_mock.return_value = self._make_command_output(
            "success", return_code=0
        )

        recorder = event.EventRecorder()
        recorder.emit_init()

        await test.run(recorder, flags, event.GLOBAL_RUN_ID)

        # The command should only be called once for the test itself.
        command_mock.assert_called_once()

    @mock.patch("execution.run_command")
    async def test_gemini_analysis_verbosity_level_passed(
        self, command_mock: mock.AsyncMock
    ) -> None:
        """test that the verbosity level is correctly passed to the gemini analysis script."""
        exec_env = _make_exec_env(
            "/fuchsia", "/out_dir", gemini_api_key="fake_key"
        )
        flags = args.parse_args(
            ["--gemini-analysis=3", "--env", "GEMINI_API_KEY=fake_key"]
        )

        test = execution.TestExecution(
            test_list_file.Test(
                tests_json_file.TestEntry(
                    tests_json_file.TestSection(
                        "foo", "//foo", "linux", path="ls"
                    )
                ),
                test_list_file.TestListEntry("foo", [], execution=None),
            ),
            exec_env,
            flags,
        )

        # Simulate a failing command
        command_mock.return_value = self._make_command_output(
            "some error", return_code=1
        )

        recorder = event.EventRecorder()
        recorder.emit_init()

        with self.assertRaises(execution.TestFailed):
            await test.run(recorder, flags, event.GLOBAL_RUN_ID)

        # The first call is to the test itself, the second is to gemini_analysis.py
        self.assertEqual(command_mock.call_count, 2)
        gemini_call = command_mock.call_args_list[1]
        self.assertIn("gemini_analysis.py", gemini_call.args[0])
        self.assertIn("--verbosity", gemini_call.args)
        self.assertIn("3", gemini_call.args)
        self.assertEqual(gemini_call.kwargs["input_bytes"], b"some error\n")

    @mock.patch("execution.run_command")
    async def test_gemini_analysis_with_no_api_key(
        self, command_mock: mock.AsyncMock
    ) -> None:
        """test that gemini analysis prints an error if no api key is provided."""

        exec_env = _make_exec_env("/fuchsia", "/out/fuchsia")
        flags = args.parse_args(["--gemini-analysis"])

        test = execution.TestExecution(
            test_list_file.Test(
                tests_json_file.TestEntry(
                    tests_json_file.TestSection(
                        "foo", "//foo", "linux", path="ls"
                    )
                ),
                test_list_file.TestListEntry("foo", [], execution=None),
            ),
            exec_env,
            flags,
        )

        # Simulate a failing command
        command_mock.return_value = self._make_command_output(
            "some error", return_code=1
        )

        recorder = event.EventRecorder()
        recorder.emit_init()

        with self.assertRaises(execution.TestFailed):
            await test.run(recorder, flags, event.GLOBAL_RUN_ID)

        # The command should only be called once for the test itself, as gemini analysis should be skipped.
        command_mock.assert_called_once()

    @mock.patch("execution.run_command")
    async def test_gemini_analysis_does_not_pass_api_key_in_command_line_args(
        self, command_mock: mock.AsyncMock
    ) -> None:
        """Test that the Gemini API key is passed via environment variables,
        not in CLI arguments or unredacted event logs."""
        secret_key = "super_secret_gemini_key_12345"
        exec_env = _make_exec_env(
            "/fuchsia", "/out_dir", gemini_api_key=secret_key
        )
        flags = args.parse_args(
            ["--gemini-analysis=2", "--env", f"GEMINI_API_KEY={secret_key}"]
        )

        test = execution.TestExecution(
            test_list_file.Test(
                tests_json_file.TestEntry(
                    tests_json_file.TestSection(
                        "foo", "//foo", "linux", path="ls"
                    )
                ),
                test_list_file.TestListEntry("foo", [], execution=None),
            ),
            exec_env,
            flags,
        )

        command_mock.return_value = self._make_command_output(
            "some failure log", return_code=1
        )

        recorder = event.EventRecorder()
        recorder.emit_init()

        with self.assertRaises(execution.TestFailed):
            await test.run(recorder, flags, event.GLOBAL_RUN_ID)

        self.assertEqual(command_mock.call_count, 2)
        gemini_call = command_mock.call_args_list[1]
        self.assertNotIn(secret_key, gemini_call.args)
        self.assertNotIn("--api-key", gemini_call.args)
        self.assertEqual(
            gemini_call.kwargs.get("env", {}).get("GEMINI_API_KEY"), secret_key
        )


class TestExecutionUtils(unittest.IsolatedAsyncioTestCase):
    def _make_command_output(
        self, stdout: str, return_code: int = 0
    ) -> command.CommandOutput:
        return command.CommandOutput(stdout, "", return_code, 0.2, None)

    def setUp(self) -> None:
        self._temp_dir = tempfile.TemporaryDirectory()
        self._env = _make_exec_env(self._temp_dir.name, "")
        self._ssh_key_file = os.path.join(
            self._temp_dir.name, "fuchsia_ed25519"
        )
        with open(self._ssh_key_file, "w") as f:
            f.write("placeholder key")
        return super().setUp()

    def tearDown(self) -> None:
        self._temp_dir.cleanup()
        return super().tearDown()

    @mock.patch("execution.run_command")
    async def test_get_device_environment_success(
        self, command_patch: mock.AsyncMock
    ) -> None:
        command_patch.side_effect = [
            self._make_command_output(""),  # target wait
            self._make_command_output("foo-bar"),  # target default get
            self._make_command_output("127.0.0.1:6000"),  # target list
            # config get ssh.priv
            self._make_command_output(self._ssh_key_file),
        ]
        device_env = await execution.get_device_environment_from_exec_env(
            self._env
        )
        expected_device_subset = {
            "address": "127.0.0.1",
            "port": "6000",
            "name": "foo-bar",
            "private_key_path": self._ssh_key_file,
        }
        self.assertTrue(
            expected_device_subset.items() <= vars(device_env).items()
        )

    @mock.patch("execution.run_command")
    async def test_get_device_environment_ipv6(
        self, command_patch: mock.AsyncMock
    ) -> None:
        command_patch.side_effect = [
            self._make_command_output(""),  # target wait
            self._make_command_output("foo-bar"),  # target default get
            self._make_command_output("[::1]:6000"),  # target list
            # config get ssh.priv
            self._make_command_output(self._ssh_key_file),
        ]
        device_env = await execution.get_device_environment_from_exec_env(
            self._env
        )
        expected_device_ipv6_subset = {
            "address": "[::1]",
            "port": "6000",
            "name": "foo-bar",
            "private_key_path": self._ssh_key_file,
        }
        self.assertTrue(
            expected_device_ipv6_subset.items() <= vars(device_env).items()
        )

    @mock.patch("execution.run_command")
    async def test_get_device_environment_ssh_error(
        self, command_patch: mock.AsyncMock
    ) -> None:
        command_patch.side_effect = [
            self._make_command_output(""),  # target wait
            self._make_command_output("foo-bar"),  # target default get
            self._make_command_output("", return_code=1),  # target list fails
        ]

        with self.assertRaisesRegex(
            execution.DeviceConfigError, "Failed to get the ssh address"
        ):
            await execution.get_device_environment_from_exec_env(self._env)

    @mock.patch("execution.run_command")
    async def test_get_device_environment_wait_error(
        self, command_patch: mock.AsyncMock
    ) -> None:
        command_patch.side_effect = [
            self._make_command_output("", return_code=1),  # target wait fails
        ]

        with self.assertRaisesRegex(
            execution.DeviceConfigError,
            "Failed to wait for target to become reachable",
        ):
            await execution.get_device_environment_from_exec_env(self._env)

    @mock.patch("execution.run_command")
    async def test_get_device_environment_bad_ip_format(
        self, command_patch: mock.AsyncMock
    ) -> None:
        command_patch.side_effect = [
            self._make_command_output(""),  # target wait
            self._make_command_output("foo-bar"),  # target default get
            self._make_command_output("foo"),  # target list
        ]

        with self.assertRaisesRegex(
            execution.DeviceConfigError,
            r"Could not parse target address: 'foo'\.\nExpected 'ip:port' format\.\nReturn code: 0,\nStderr: ''",
        ):
            await execution.get_device_environment_from_exec_env(self._env)

    @mock.patch("execution.run_command")
    async def test_get_device_environment_no_target_name(
        self, command_patch: mock.AsyncMock
    ) -> None:
        command_patch.side_effect = [
            self._make_command_output(""),  # target wait
            self._make_command_output(
                "", return_code=1
            ),  # target default get fails
        ]

        with self.assertRaisesRegex(
            execution.DeviceConfigError,
            "Failed to get the target name. Please ensure you have set a default target using 'fx set-device'. See 'fx set-device --help' for more details on target resolution.",
        ):
            await execution.get_device_environment_from_exec_env(self._env)

    @mock.patch("execution.run_command")
    async def test_get_device_environment_ssh_key_not_found(
        self, command_patch: mock.AsyncMock
    ) -> None:
        command_patch.side_effect = [
            self._make_command_output(""),  # target wait
            self._make_command_output("foo-bar"),  # target default get
            self._make_command_output("127.0.0.1:6000"),  # target list
            self._make_command_output(
                "/nonexistent/path"
            ),  # config get ssh.priv
        ]

        with self.assertRaisesRegex(
            execution.DeviceConfigError,
            "Path returned by 'ssh.priv' does not exist",
        ):
            await execution.get_device_environment_from_exec_env(self._env)

    async def test_test_execution_gets_env_from_flags(self) -> None:
        """Test TestExecution wrapper uses the environment provided by flags"""

        with tempfile.TemporaryDirectory() as tmp:
            exec_env = _make_exec_env("/fuchsia", tmp)

            flags = args.parse_args(
                ["-e", "foo=bar", "--env", "my_setting=baz"]
            )

            test = execution.TestExecution(
                test_list_file.Test(
                    tests_json_file.TestEntry(
                        tests_json_file.TestSection(
                            "foo", "//foo", "linux", path="wont_exist"
                        )
                    ),
                    test_list_file.TestListEntry("foo", [], execution=None),
                ),
                exec_env,
                flags,
            )

            e = test.environment()
            assert e is not None
            self.assertDictEqual(
                e,
                {"CWD": tmp, "foo": "bar", "my_setting": "baz"},
            )

    @parameterized.expand(
        [
            (
                None,
                None,
                300,
            ),
            (
                None,
                1234,
                1234,
            ),
            (
                1234,
                None,
                1234,
            ),
            (
                0,
                None,
                None,
            ),
            (
                0,
                1234,
                None,
            ),
        ]
    )
    def test_calculate_timeout(
        self,
        arg: float | None,
        config: int | None,
        result: float | None,
    ) -> None:
        """Test _calculate_timeout function works properly"""

        exec_env = _make_exec_env("/fuchsia", "/out/fuchsia")

        test = execution.TestExecution(
            test_list_file.Test(
                tests_json_file.TestEntry(
                    tests_json_file.TestSection(
                        "foo",
                        "//foo",
                        "fuchsia",
                        timeout_secs=config,
                    )
                ),
                test_list_file.TestListEntry(
                    "foo",
                    [],
                    test_list_file.TestListExecutionEntry(
                        "fuchsia-pkg://fuchsia.com/foo#meta/foo_test.cm",
                        realm="foo_tests",
                        max_severity_logs="INFO",
                        min_severity_logs="TRACE",
                    ),
                ),
            ),
            exec_env,
            args.parse_args(["--timeout", str(arg)] if arg is not None else []),
        )

        self.assertEqual(test._calculate_timeout(), result)

    async def test_get_device_ssh_address_ipv6_brackets_and_multiline(
        self,
    ) -> None:
        """Test that get_device_environment_from_exec_env parses IPv6 bracketed addresses and ignores earlier lines."""
        exec_env = _make_exec_env("/fuchsia", "/out/fuchsia")
        recorder = mock.MagicMock()

        with tempfile.NamedTemporaryFile() as tf:

            async def fake_run_command(
                *args_list: str, **kwargs: typing.Any
            ) -> command.CommandOutput:
                cmd = " ".join(args_list)
                if "wait" in cmd:
                    return command.CommandOutput(
                        stdout="",
                        stderr="",
                        return_code=0,
                        runtime=0.1,
                        wrapper_return_code=0,
                    )
                elif "default get" in cmd:
                    return command.CommandOutput(
                        stdout="my-target\n",
                        stderr="",
                        return_code=0,
                        runtime=0.1,
                        wrapper_return_code=0,
                    )
                elif "target list" in cmd:
                    return command.CommandOutput(
                        stdout="Some warning notice\n[fe80::1%eth0]:8022\n",
                        stderr="",
                        return_code=0,
                        runtime=0.1,
                        wrapper_return_code=0,
                    )
                elif "ssh.priv" in cmd:
                    return command.CommandOutput(
                        stdout=f"{tf.name}\n",
                        stderr="",
                        return_code=0,
                        runtime=0.1,
                        wrapper_return_code=0,
                    )
                return command.CommandOutput(
                    stdout="",
                    stderr="",
                    return_code=1,
                    runtime=0.1,
                    wrapper_return_code=1,
                )

            with mock.patch(
                "execution.run_command", side_effect=fake_run_command
            ):
                dev_env = await execution.get_device_environment_from_exec_env(
                    exec_env, recorder
                )
                self.assertEqual(dev_env.address, "[fe80::1%eth0]")
                self.assertEqual(dev_env.port, "8022")
                self.assertEqual(dev_env.private_key_path, tf.name)

    async def test_get_device_ssh_address_parses_unbracketed_ipv6_and_default_port(
        self,
    ) -> None:
        """Test that get_device_environment_from_exec_env parses unbracketed IPv6 addresses without mangling the address."""
        exec_env = _make_exec_env("/fuchsia", "/out/fuchsia")
        recorder = mock.MagicMock()

        with tempfile.NamedTemporaryFile() as tf:

            async def fake_run_command(
                *args_list: str, **kwargs: typing.Any
            ) -> command.CommandOutput:
                cmd = " ".join(args_list)
                if "wait" in cmd:
                    return command.CommandOutput(
                        stdout="",
                        stderr="",
                        return_code=0,
                        runtime=0.1,
                        wrapper_return_code=0,
                    )
                elif "default get" in cmd:
                    return command.CommandOutput(
                        stdout="my-target\n",
                        stderr="",
                        return_code=0,
                        runtime=0.1,
                        wrapper_return_code=0,
                    )
                elif "target list" in cmd:
                    return command.CommandOutput(
                        stdout="fe80::1\n",
                        stderr="",
                        return_code=0,
                        runtime=0.1,
                        wrapper_return_code=0,
                    )
                elif "ssh.priv" in cmd:
                    return command.CommandOutput(
                        stdout=f"{tf.name}\n",
                        stderr="",
                        return_code=0,
                        runtime=0.1,
                        wrapper_return_code=0,
                    )
                return command.CommandOutput(
                    stdout="",
                    stderr="",
                    return_code=1,
                    runtime=0.1,
                    wrapper_return_code=1,
                )

            with mock.patch(
                "execution.run_command", side_effect=fake_run_command
            ):
                dev_env = await execution.get_device_environment_from_exec_env(
                    exec_env, recorder
                )
                self.assertEqual(dev_env.address, "fe80::1")
                self.assertEqual(dev_env.port, "22")

    @parameterized.expand(
        [
            (
                "bracketed_ipv6_with_port",
                "[fe80::1%eth0]:8022",
                "[fe80::1%eth0]",
                "8022",
            ),
            ("bracketed_ipv6_default_port", "[fe80::1]", "[fe80::1]", "22"),
            (
                "bracketed_ipv6_explicit_standard_port",
                "[fe80::1]:22",
                "[fe80::1]",
                "22",
            ),
            ("bracketed_loopback_with_port", "[::1]:8022", "[::1]", "8022"),
            ("bracketed_loopback_default_port", "[::1]", "[::1]", "22"),
            (
                "bracketed_global_unicast_with_port",
                "[2001:db8::1]:53",
                "[2001:db8::1]",
                "53",
            ),
            (
                "bracketed_full_ipv6_with_port",
                "[2001:0db8:85a3:0000:0000:8a2e:0370:7334]:8022",
                "[2001:0db8:85a3:0000:0000:8a2e:0370:7334]",
                "8022",
            ),
            (
                "bracketed_ipv4_mapped_ipv6",
                "[::ffff:192.0.2.1]:8022",
                "[::ffff:192.0.2.1]",
                "8022",
            ),
            ("bracketed_unspecified_ipv6", "[::]:22", "[::]", "22"),
            ("unbracketed_link_local", "fe80::1", "fe80::1", "22"),
            (
                "unbracketed_link_local_with_scope",
                "fe80::1%eth0",
                "fe80::1%eth0",
                "22",
            ),
            ("unbracketed_loopback", "::1", "::1", "22"),
            ("unbracketed_global_unicast", "2001:db8::1", "2001:db8::1", "22"),
            (
                "unbracketed_full_ipv6",
                "2001:db8:85a3:8d3:1319:8a2e:370:7348",
                "2001:db8:85a3:8d3:1319:8a2e:370:7348",
                "22",
            ),
            ("ipv4_with_port", "192.168.1.1:8022", "192.168.1.1", "8022"),
            ("ipv4_loopback_with_port", "127.0.0.1:22", "127.0.0.1", "22"),
            ("hostname_with_port", "localhost:8022", "localhost", "8022"),
            ("usb_cid", "usb:cid:3", "usb:cid", "3"),
            ("vsock_cid", "vsock:cid:4", "vsock:cid", "4"),
        ]
    )
    def test_parse_device_address_valid(
        self,
        _name: str,
        target_line: str,
        expected_ip: str,
        expected_port: str,
    ) -> None:
        """Test _parse_device_address parses various valid address formats correctly."""
        ip, port = execution._parse_device_address(target_line)
        self.assertEqual(ip, expected_ip)
        self.assertEqual(port, expected_port)

    @parameterized.expand(
        [
            ("missing_closing_bracket", "[fe80::1"),
            ("empty_bracket", "[]:22"),
            ("empty_port_after_colon", "[fe80::1]:"),
            ("non_numeric_port", "[fe80::1]:abc"),
            ("port_zero", "[fe80::1]:0"),
            ("port_too_large", "[fe80::1]:65536"),
            ("negative_port", "[fe80::1]:-1"),
            ("junk_after_bracket", "[fe80::1]extra:22"),
            ("invalid_ipv6_triple_colon", "[fe80:::1]:22"),
            ("invalid_ipv6_hex_chars", "[fe80::xyz]:22"),
            ("empty_scope_id_in_bracket", "[fe80::1%]:22"),
            ("unbracketed_triple_colon", "fe80:::1"),
            ("unbracketed_invalid_hex", "fe80::xyz"),
            ("unbracketed_empty_scope_id", "fe80::1%"),
            ("missing_host_before_colon", ":8022"),
            ("no_colon_no_ipv6", "nohost"),
            ("usb_cid_non_numeric", "usb:cid:abc"),
            ("usb_cid_empty", "usb:cid:"),
        ]
    )
    def test_parse_device_address_invalid(
        self,
        _name: str,
        target_line: str,
    ) -> None:
        """Test _parse_device_address rejects malformed addresses and invalid ports."""
        with self.assertRaises(execution.DeviceConfigError):
            execution._parse_device_address(target_line)

    @mock.patch("async_utils.command.AsyncCommand.create")
    async def test_command_abort_waits_for_process_exit_without_sleeping_full_timeout(
        self, mock_create: mock.AsyncMock
    ) -> None:
        """Test that abort_signal waits on process runner task rather than sleeping 5s."""
        mock_cmd = mock.MagicMock()
        runner_future = asyncio.get_running_loop().create_future()

        def terminate_side_effect() -> None:
            if not runner_future.done():
                runner_future.set_result(None)

        mock_cmd.terminate.side_effect = terminate_side_effect
        mock_cmd._runner_task = runner_future
        mock_cmd.run_to_completion = mock.AsyncMock(
            return_value=mock.MagicMock(return_code=-15)
        )
        mock_create.return_value = mock_cmd

        recorder = mock.MagicMock()
        abort_signal = asyncio.Event()

        task = asyncio.create_task(
            execution.run_command(
                "my_cmd",
                recorder=recorder,
                abort_signal=abort_signal,
            )
        )
        await asyncio.sleep(0.01)
        abort_signal.set()
        await task

        mock_cmd.kill.assert_not_called()
        for call in recorder.emit_warning_message.call_args_list:
            self.assertNotIn("did not terminate in time", call[0][0])

    def test_is_host_test(self) -> None:
        """Test that TestExecution.is_host_test returns True for a pure host test."""
        exec_env = _make_exec_env("/fuchsia", "/out/fuchsia")
        test = execution.TestExecution(
            test_list_file.Test(
                tests_json_file.TestEntry(
                    tests_json_file.TestSection(
                        "foo", "//foo", "linux", path="host_x64/foo"
                    )
                ),
                test_list_file.TestListEntry("foo", [], execution=None),
            ),
            exec_env,
            args.parse_args([]),
        )
        self.assertTrue(test.is_host_test())
        self.assertFalse(test.is_hermetic())

#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

_SCRIPT_DIR = os.path.dirname(__file__)
sys.path.insert(0, _SCRIPT_DIR)
import build_utils
from build_utils import BazelLauncher, BazelQueryCache, CommandResult


class FindFuchsiaDirTest(unittest.TestCase):
    def test_find_fuchsia_dir(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            not_fuchsia_dir = Path(tmp_dir) / "this_is_not_fuchsia"
            not_fuchsia_dir.mkdir()
            fuchsia_dir = Path(tmp_dir) / "this_is_fuchsia"
            fuchsia_dir.mkdir()
            (fuchsia_dir / ".jiri_manifest").write_text("")
            (fuchsia_dir / "src" / "foo").mkdir(parents=True)

            # Check function when searching from the current path.
            saved_cwd = os.getcwd()
            try:
                os.chdir(not_fuchsia_dir)
                with self.assertRaises(ValueError):
                    build_utils.find_fuchsia_dir()

                for path in (
                    fuchsia_dir,
                    fuchsia_dir / "src",
                    fuchsia_dir / "src" / "foo",
                ):
                    os.chdir(path)
                    self.assertEqual(
                        build_utils.find_fuchsia_dir(),
                        fuchsia_dir,
                        f"From {path}",
                    )

                # Ensure the result is absolute even if the starting path is relative.
                os.chdir(fuchsia_dir)
                for path in (Path("."), Path("src"), Path("src/foo")):
                    self.assertEqual(
                        build_utils.find_fuchsia_dir(path),
                        fuchsia_dir,
                        f"From {path}",
                    )

            finally:
                os.chdir(saved_cwd)

            # Check function when searching from a given path.
            with self.assertRaises(ValueError):
                build_utils.find_fuchsia_dir(not_fuchsia_dir)

            for path in (
                fuchsia_dir,
                fuchsia_dir / "src",
                fuchsia_dir / "src" / "foo",
            ):
                self.assertEqual(
                    build_utils.find_fuchsia_dir(path),
                    fuchsia_dir,
                    f"From {path}",
                )


class FindFxBuildDirTest(unittest.TestCase):
    def test_find_fx_build_dir(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            fuchsia_dir = Path(tmp_dir)

            # No .fx-build-dir present -> Path()
            self.assertEqual(build_utils.find_fx_build_dir(fuchsia_dir), None)

            # Empty .fx-build-dir content -> Path()
            fx_build_dir_path = fuchsia_dir / ".fx-build-dir"
            fx_build_dir_path.write_text("")
            self.assertEqual(build_utils.find_fx_build_dir(fuchsia_dir), None)

            # Invalid .fx-build-dir content -> Path()
            fx_build_dir_path.write_text("does/not/exist\n")
            self.assertEqual(build_utils.find_fx_build_dir(fuchsia_dir), None)

            # Valid build directory.
            build_dir = fuchsia_dir / "some" / "build_dir"
            build_dir.mkdir(parents=True)

            fx_build_dir_path.write_text("some/build_dir\n")
            self.assertEqual(
                build_utils.find_fx_build_dir(fuchsia_dir), build_dir
            )


class FindBazelPathsTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self.fuchsia_dir = Path(self._td.name)
        config_dir = self.fuchsia_dir / "build" / "bazel" / "config"
        config_dir.mkdir(parents=True)
        (config_dir / "bazel_top_dir").write_text("some/top/dir\n")

        self.build_dir = self.fuchsia_dir / "out" / "build_dir"
        self.launcher_path = self.build_dir / "some/top/dir/bazel"
        self.workspace_path = self.build_dir / "some/top/dir/workspace"

    def tearDown(self) -> None:
        self._td.cleanup()

    def test_find_bazel_launcher_path(self) -> None:
        self.assertEqual(
            build_utils.find_bazel_launcher_path(
                self.fuchsia_dir, self.build_dir
            ),
            None,
        )

        self.launcher_path.parent.mkdir(parents=True)
        self.launcher_path.write_text("!")
        self.assertEqual(
            build_utils.find_bazel_launcher_path(
                self.fuchsia_dir, self.build_dir
            ),
            self.launcher_path,
        )

    def test_find_bazel_workspace_path(self) -> None:
        self.assertEqual(
            build_utils.find_bazel_workspace_path(
                self.fuchsia_dir, self.build_dir
            ),
            None,
        )

        self.workspace_path.mkdir(parents=True)
        self.assertEqual(
            build_utils.find_bazel_workspace_path(
                self.fuchsia_dir, self.build_dir
            ),
            self.workspace_path,
        )


class FindBazelWorkspacePathTest(unittest.TestCase):
    def test_find_bazel_workspace_path(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            fuchsia_dir = Path(tmp_dir)
            config_dir = fuchsia_dir / "build" / "bazel" / "config"
            config_dir.mkdir(parents=True)
            (config_dir / "bazel_top_dir").write_text("some/top/dir\n")

            build_dir = fuchsia_dir / "out" / "build_dir"
            launcher_path = build_dir / "some/top/dir/bazel"

            self.assertEqual(
                build_utils.find_bazel_launcher_path(fuchsia_dir, build_dir),
                None,
            )

            launcher_path.parent.mkdir(parents=True)
            launcher_path.write_text("!")
            self.assertEqual(
                build_utils.find_bazel_launcher_path(fuchsia_dir, build_dir),
                launcher_path,
            )


class GetBazelRelativeTopDirTest(unittest.TestCase):
    def test_get_bazel_relative_topdir(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            fuchsia_dir = Path(tmp_dir)
            config_dir = fuchsia_dir / "build" / "bazel" / "config"
            config_dir.mkdir(parents=True)

            main_config = config_dir / "bazel_top_dir"
            main_config.write_text("gen/test/bazel_workspace\n")

            topdir, input_files = build_utils.get_bazel_relative_topdir(
                fuchsia_dir
            )
            self.assertEqual(topdir, "gen/test/bazel_workspace")
            self.assertListEqual(list(input_files), [main_config])

            topdir, input_files = build_utils.get_bazel_relative_topdir(
                str(fuchsia_dir)
            )
            self.assertEqual(topdir, "gen/test/bazel_workspace")
            self.assertListEqual(list(input_files), [main_config])


class ForceSymlinkTest(unittest.TestCase):
    def test_force_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp_path = Path(tmp_dir).resolve()

            # Create a new symlink, then ensure its embedded target is relative.
            # The target doesn't need to exist.
            target_path = tmp_path / "target" / "file"
            link_path = tmp_path / "links" / "dir" / "symlink"

            build_utils.force_symlink(link_path, target_path)

            self.assertTrue(link_path.is_symlink())
            self.assertEqual(str(link_path.readlink()), "../../target/file")

            # Update the target to a new path, verify the symlink was updated.
            target_path = tmp_path / "target" / "new_file"

            build_utils.force_symlink(link_path, target_path)
            self.assertTrue(link_path.is_symlink())
            self.assertEqual(str(link_path.readlink()), "../../target/new_file")


class IsHexadecimalStringTest(unittest.TestCase):
    def test_is_hexadecimal_string(self) -> None:
        TEST_CASES = [
            ("", False),
            ("0", True),
            ("0.", False),
            ("F", True),
            ("G", False),
            ("0123456789abcdefABCDEF", True),
            ("0123456789abcdefghijklmnopqrstuvwxyz", False),
        ]
        for input, expected in TEST_CASES:
            self.assertEqual(
                build_utils.is_hexadecimal_string(input),
                expected,
                msg=f"For input [{input}]",
            )


class IsLikelyBuildIdPathTest(unittest.TestCase):
    def test_one(self) -> None:
        TEST_CASES = [
            ("", False),
            ("/src/.build-id", False),
            ("/src/.build-id/0", False),
            ("/src/.build-id/00", False),
            ("/src/.build-id/000", False),
            ("/src/.build-id/0/foo", False),
            ("/src/.build-id/00/foo", True),
            ("/src/.build-id/000/foo", False),
            ("/src/.build-id/af/foo", True),
            ("/src/.build-id/ag/foo", False),
            ("/src/..build-id/00/foo", False),
            ("/src.build-id/00/foo", False),
            ("/src/.build-id/log.txt", False),
            (".build-id/00/foo", True),
        ]
        for input, expected in TEST_CASES:
            self.assertEqual(
                build_utils.is_likely_build_id_path(input),
                expected,
                msg=f"For input [{input}]",
            )


class IsLikelyContentHashPathTest(unittest.TestCase):
    def test_one(self) -> None:
        TEST_CASES = [
            ("", False),
            ("/src/.build-id/0/foo", False),
            ("/src/.build-id/00/foo", True),
            ("/src/blobs/0123456789abcdef0000", True),
            ("/src/blobs/01234", False),  # Too short
            ("0123456789abcdef0000", True),
        ]
        for input, expected in TEST_CASES:
            self.assertEqual(
                build_utils.is_likely_content_hash_path(input),
                expected,
                msg=f"For input [{input}]",
            )


class MockCurrentTime(object):
    """A mock time.time() implementation used for TimeProfileTest.

    Initial time is always 100 seconds, first call is 10 seconds
    """

    def __init__(self) -> None:
        self._current_time = 100.0

    def reset(self) -> None:
        self._current_time = 100.0

    def increment(self, increment: float) -> None:
        self._current_time += increment

    def __call__(self) -> float:
        return self._current_time


class TimeProfileTest(unittest.TestCase):
    def setUp(self) -> None:
        self._now = MockCurrentTime()

    def test_empty(self) -> None:
        p = build_utils.TimeProfile(now=self._now)
        self.assertDictEqual(p.to_json_timings(), {})

    def test_single_step(self) -> None:
        now = self._now

        # Verifies that if time does not increment, duration is 0.
        p = build_utils.TimeProfile(now=now)
        p.start("first_step", "A first step")
        self.assertDictEqual(p.to_json_timings(), {"first_step": 0})

        # Increment time by 10 seconds after start(), then calls to_json_timings()
        # directly without a stop() call.
        now.reset()
        p = build_utils.TimeProfile(now=self._now)
        p.start("first_step_again", "Another first step")
        now.increment(10)
        self.assertDictEqual(p.to_json_timings(), {"first_step_again": 10.0})

        # Same as above, but calls stop() before increment time again.
        now.reset()
        p = build_utils.TimeProfile(now=self._now)
        p.start("first_step_again", "Another first step")
        now.increment(10)
        p.stop()
        now.increment(20)
        self.assertDictEqual(p.to_json_timings(), {"first_step_again": 10.0})

    def test_multiple_steps(self) -> None:
        now = self._now
        p = build_utils.TimeProfile(now=now)
        p.start("first", "First step")
        now.increment(20)
        p.start("second", "Second step")
        now.increment(10)
        p.stop()
        now.increment(10)
        p.start("third", "Third step")
        now.increment(40)
        p.stop()
        now.increment(1000)

        self.assertDictEqual(
            p.to_json_timings(),
            {
                "first": 20,
                "second": 10,
                "third": 40,
            },
        )

    def test_log(self) -> None:
        now = self._now

        log_messages = []

        def log(msg: str) -> None:
            log_messages.append(msg)

        p = build_utils.TimeProfile(now=now, log=log)
        p.start("first", "first message")
        now.increment(10)
        p.start("second", "second message")
        now.increment(10)
        p.start("third", "third message")

        self.assertListEqual(
            log_messages, ["first message", "second message", "third message"]
        )


class MockBazelLauncher(BazelLauncher):
    """A BazelLauncher sub-class used to mock subprocess invocation.

    The class manages a FIFO of CommandResult values that is
    filled by calling push_result(), and which is consumed when
    run_command() is called.
    """

    def __init__(self) -> None:
        """Create instance."""

        def log(msg: str) -> None:
            self.logs.append(msg)

        def log_error(msg: str) -> None:
            self.errors.append(msg)

        super().__init__("/path/to/bazel", log=log, log_err=log_error)
        self.commands: list[list[str]] = []
        self.result_queue: list[CommandResult] = []
        self.logs: list[str] = []
        self.errors: list[str] = []

    def push_result(
        self, returncode: int = 0, stdout: str = "", stderr: str = ""
    ) -> None:
        """Add one result value to the FIFO.

        Args:
            returncode: Optional process return code. default to 0.
            stdout: Optional process stdout, as a string, default to empty.
            stderr: Optional process stderr, as a string, default to empty.
        """
        self.result_queue.append(CommandResult(returncode, stdout, stderr))

    def run_command_internal(
        self,
        cmd_args: list[str],
        print_stdout: bool = False,
        print_stderr: bool = False,
    ) -> CommandResult:
        """Simulate command invocation by popping one value from the FIFO.

        Args:
            cmd_args: Command arguments, these are simply saved into
                self.commands for later inspection.
            print_stderr: Optional flag, set to True to not capture stdout.
            print_stderr: Optional flag, set to True to not capture stderr.
        Returns:
            The CommandResult at the start of the FIFO.
        Raises:
            AssertionError if there are no results in the FIFO.
        """
        self.commands.append(cmd_args[:])
        assert self.result_queue
        result = self.result_queue[0]
        self.result_queue = self.result_queue[1:]
        return result


class BazelQueryCacheTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self._root = Path(self._td.name)

    def tearDown(self) -> None:
        self._td.cleanup()

    def test_cache(self) -> None:
        launcher = MockBazelLauncher()
        cache_dir = self._root / "cache"
        cache = BazelQueryCache(cache_dir)

        launcher.push_result(stdout="some\nresult\nlines")
        result = cache.get_query_output("query", ["deps(//src:foo)"], launcher)

        self.assertListEqual(result, ["some", "result", "lines"])
        self.assertTrue(cache_dir.is_dir())
        key, args = cache.compute_cache_key_and_args(
            "query", ["deps(//src:foo)"]
        )
        key_file = cache_dir / f"{key}.json"
        self.assertTrue(key_file.exists())
        with key_file.open() as f:
            value = json.load(f)
        self.assertDictEqual(
            value,
            {
                "key_args": ["query", "deps(//src:foo)"],
                "output_lines": ["some", "result", "lines"],
            },
        )

        # A second call with the same query and arguments should retrieve
        # the result from the cache, and not run any command. To detect this
        # push a new launcher result, which will not be popped as no
        # command will be run by the cache.
        launcher.push_result(stdout="some\nother\nresult\nlines")
        result2 = cache.get_query_output("query", ["deps(//src:foo)"], launcher)
        self.assertListEqual(result2, result)

        # By changing the query type, this will pop the last pushed
        # result value, while creating a new cache entry.
        result3 = cache.get_query_output(
            "cquery", ["deps(//src:foo)"], launcher
        )
        self.assertListEqual(result3, ["some", "other", "result", "lines"])

        key3, args3 = cache.compute_cache_key_and_args(
            "cquery", ["deps(//src:foo)"]
        )
        self.assertNotEqual(key3, key)
        key3_file = cache_dir / f"{key3}.json"
        self.assertTrue(key3_file.exists())
        with key3_file.open() as f:
            value3 = json.load(f)
        self.assertDictEqual(
            value3,
            {
                "key_args": ["cquery", "deps(//src:foo)"],
                "output_lines": ["some", "other", "result", "lines"],
            },
        )

    def test_cache_with_starlark_file(self) -> None:
        launcher = MockBazelLauncher()
        cache_dir = self._root / "cache"
        cache = BazelQueryCache(cache_dir)

        # Create starlark file. Exact content does not matter as real Bazel
        # queries are not run by this test.
        starlark_file = self._root / "query.starlark"
        starlark_file.write_text("1")

        query_cmd = "query"
        query_args = [
            "deps(//src:foo)",
            "--output=starlark",
            "--starlark:file",
            str(starlark_file),
        ]

        # First invocation creates cache entry.
        launcher.push_result(stdout="some\nresult\nlines")
        result = cache.get_query_output(query_cmd, query_args, launcher)
        self.assertListEqual(result, ["some", "result", "lines"])
        self.assertTrue(cache_dir.is_dir())

        key, args = cache.compute_cache_key_and_args(query_cmd, query_args)
        key_file = cache_dir / f"{key}.json"
        self.assertTrue(key_file.exists())
        with key_file.open() as f:
            value = json.load(f)
        self.assertDictEqual(
            value,
            {
                "key_args": [query_cmd] + query_args,
                "output_lines": ["some", "result", "lines"],
            },
        )

        # Second invocation returns the cache value directly without
        # invoking anything.
        launcher.push_result(stdout="other\nresult\nlines")
        result2 = cache.get_query_output(query_cmd, query_args, launcher)
        self.assertEqual(result2, result)

        key, args = cache.compute_cache_key_and_args(query_cmd, query_args)

        # Now modifying the contenf ot the input file should change
        # the key value and force a launcher invocation. The arguments
        # will be the same though.
        starlark_file.write_text("2")
        result3 = cache.get_query_output(query_cmd, query_args, launcher)
        self.assertEqual(result3, ["other", "result", "lines"])

        key2, args2 = cache.compute_cache_key_and_args(query_cmd, query_args)
        self.assertNotEqual(key2, key)
        self.assertEqual(args, args2)

        # Now change the query args to use --starlark:file=FILE
        # This will end up creating a new cache key.
        query_args = [
            "deps(//src:foo)",
            "--output=starlark",
            f"--starlark:file={starlark_file}",
        ]
        key3, args3 = cache.compute_cache_key_and_args(query_cmd, query_args)
        self.assertNotEqual(key3, key)
        self.assertNotEqual(key3, key2)

        # Modifying the file should change the key, but not the args.
        starlark_file.write_text("3")
        key4, args4 = cache.compute_cache_key_and_args(query_cmd, query_args)
        self.assertNotEqual(key4, key3)
        self.assertNotEqual(key4, key2)
        self.assertNotEqual(key4, key)
        self.assertEqual(args3, args4)


if __name__ == "__main__":
    unittest.main()

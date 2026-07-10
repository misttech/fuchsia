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
from build_utils import BazelLauncher, BazelQueryCache, MockCommandRunner


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

    def test_force_symlink_to_ninja_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            tmp_path = Path(tmp_dir).resolve()

            # Create a new symlink, then ensure this also creates the target with a timestamp of 0.
            target1_path = tmp_path / "target" / "file1"
            link1_path = tmp_path / "links" / "dir" / "symlink1"

            build_utils.force_symlink_to_ninja_artifact(
                link1_path, target1_path
            )

            self.assertTrue(link1_path.is_symlink())
            self.assertEqual(str(link1_path.readlink()), "../../target/file1")

            self.assertTrue(target1_path.exists())
            self.assertEqual(target1_path.stat().st_mtime_ns, 0)

            # Create another symlink to an existing file, verify that this does not modify its
            # timestamp.
            target2_path = tmp_path / "target" / "file2"
            target2_path.write_text("hi")
            original_target2_mtime = target2_path.stat().st_mtime_ns

            link2_path = tmp_path / "links" / "dir" / "symlink2"

            build_utils.force_symlink_to_ninja_artifact(
                link2_path, target2_path
            )
            self.assertTrue(link2_path.is_symlink())
            self.assertEqual(str(link2_path.readlink()), "../../target/file2")
            self.assertTrue(target2_path.exists())
            self.assertEqual(
                target2_path.stat().st_mtime_ns, original_target2_mtime
            )


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


class BazelPathsTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self.fuchsia_dir = Path(self._td.name) / "fuchsia"
        self.fuchsia_dir.mkdir()
        (self.fuchsia_dir / ".jiri_manifest").write_text("")

        self.build_dir = self.fuchsia_dir / "out" / "build_dir"
        self.build_dir.mkdir(parents=True)
        (self.fuchsia_dir / ".fx-build-dir").write_text("out/build_dir\n")

        build_utils.BazelPaths.write_topdir_config_for_test(
            self.fuchsia_dir, "some/top/dir"
        )
        self.launcher_path = self.build_dir / "some/top/dir/bazel"
        self.workspace_path = self.build_dir / "some/top/dir/workspace"
        self.output_base_path = self.build_dir / "some/top/dir/output_base"

    def tearDown(self) -> None:
        self._td.cleanup()

    def test_new_with_no_valid_fuchsia_or_build_dirs(self) -> None:
        saved_current_dir = Path.cwd()
        try:
            os.chdir(self._td.name)

            # This fails because BazelPaths.new() cannot find a Fuchsia source directory
            # from the current directory.
            with self.assertRaises(ValueError) as cm:
                build_utils.BazelPaths.new()
            self.assertEqual(
                str(cm.exception),
                f"Could not find Fuchsia checkout directory from: {self._td.name}",
            )
        finally:
            os.chdir(saved_current_dir)

    def test_new_with_no_valid_build_dir(self) -> None:
        saved_current_dir = Path.cwd()
        (self.fuchsia_dir / ".fx-build-dir").write_text("out/does_not_exist")
        try:
            os.chdir(self._td.name)

            # This fails because BazelPaths.new() cannot find a .fx-build-dir file from
            # the Fuchsia source directory.
            with self.assertRaises(ValueError) as cm:
                paths = build_utils.BazelPaths.new(fuchsia_dir=self.fuchsia_dir)
            self.assertEqual(
                str(cm.exception),
                f"Could not detect current build-directory from Fuchsia directory: {self.fuchsia_dir}",
            )
        finally:
            os.chdir(saved_current_dir)

    def test_new_with_fuchsia_dir_only(self) -> None:
        paths = build_utils.BazelPaths.new(fuchsia_dir=self.fuchsia_dir)
        self.assertTrue(paths)
        self.assertEqual(paths.fuchsia_dir, self.fuchsia_dir)
        self.assertEqual(paths.ninja_build_dir, self.build_dir)
        self.assertEqual(paths.workspace, self.workspace_path)
        self.assertEqual(paths.output_base, self.output_base_path)
        self.assertEqual(paths.launcher, self.launcher_path)

    def test_new_with_build_dir_only(self) -> None:
        paths = build_utils.BazelPaths.new(build_dir=self.build_dir)
        self.assertTrue(paths)
        self.assertEqual(paths.fuchsia_dir, self.fuchsia_dir)
        self.assertEqual(paths.ninja_build_dir, self.build_dir)
        self.assertEqual(paths.workspace, self.workspace_path)
        self.assertEqual(paths.output_base, self.output_base_path)
        self.assertEqual(paths.launcher, self.launcher_path)

    def test_new_with_fuchsia_and_build_dirs(self) -> None:
        paths = build_utils.BazelPaths.new(
            fuchsia_dir=self.fuchsia_dir, build_dir=self.build_dir
        )
        self.assertTrue(paths)
        self.assertEqual(paths.fuchsia_dir, self.fuchsia_dir)
        self.assertEqual(paths.ninja_build_dir, self.build_dir)
        self.assertEqual(paths.workspace, self.workspace_path)
        self.assertEqual(paths.output_base, self.output_base_path)
        self.assertEqual(paths.launcher, self.launcher_path)

    def test_write_topdir_config_for_test(self) -> None:
        build_utils.BazelPaths.write_topdir_config_for_test(
            self.fuchsia_dir, "some/other/topdir"
        )
        paths = build_utils.BazelPaths(self.fuchsia_dir, self.build_dir)
        self.assertEqual(paths.top_dir, self.build_dir / "some/other/topdir")

    def test_constructor(self) -> None:
        paths = build_utils.BazelPaths(self.fuchsia_dir, self.build_dir)
        self.assertEqual(paths.fuchsia_dir, self.fuchsia_dir)
        self.assertEqual(paths.ninja_build_dir, self.build_dir)
        self.assertEqual(paths.workspace, self.workspace_path)
        self.assertEqual(paths.output_base, self.output_base_path)
        self.assertEqual(paths.launcher, self.launcher_path)


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


class BazelBuildInvocationTest(unittest.TestCase):
    def test_new_instance(self) -> None:
        with self.assertRaises(ValueError) as cm:
            build_utils.BazelBuildInvocation(bazel_targets=[], build_args=[])

        i = build_utils.BazelBuildInvocation(
            bazel_targets=["//src:foo"],
            build_args=[],
        )
        self.assertListEqual(i.bazel_targets, ["//src:foo"])
        self.assertListEqual(i.build_args, [])
        self.assertIsNone(i.gn_label)
        self.assertIsNone(i.gn_targets_dir)
        self.assertIsNone(i.bazel_action_timings)

        i = build_utils.BazelBuildInvocation(
            bazel_targets=["//first:target", "//second:target"],
            build_args=["--config=host", "--config=remote_cache_only"],
            gn_label="//some:bazel_action",
            gn_targets_dir="obj/some/bazel_action/gn_targets_dir",
            bazel_action_timings={"foo": 1.0},
        )
        self.assertListEqual(
            i.bazel_targets, ["//first:target", "//second:target"]
        )
        self.assertListEqual(
            i.build_args, ["--config=host", "--config=remote_cache_only"]
        )
        self.assertEqual(i.gn_label, "//some:bazel_action")
        self.assertEqual(
            i.gn_targets_dir, "obj/some/bazel_action/gn_targets_dir"
        )
        assert i.bazel_action_timings  # make mypy happy
        self.assertDictEqual(i.bazel_action_timings, {"foo": 1})

    def test_to_json(self) -> None:
        i = build_utils.BazelBuildInvocation(
            bazel_targets=["//src:foo"],
            build_args=[],
        )
        self.assertDictEqual(
            i.to_json(),
            {
                "bazel_targets": ["//src:foo"],
                "build_args": [],
            },
        )

        i = build_utils.BazelBuildInvocation(
            bazel_targets=["//first:target", "//second:target"],
            build_args=["--config=host", "--config=remote_cache_only"],
            gn_label="//some:bazel_action",
            gn_targets_dir="obj/some/bazel_action/gn_targets_dir",
            bazel_action_timings={"bar": 42.0},
        )
        self.assertDictEqual(
            i.to_json(),
            {
                "bazel_targets": ["//first:target", "//second:target"],
                "build_args": ["--config=host", "--config=remote_cache_only"],
                "gn_label": "//some:bazel_action",
                "gn_targets_dir": "obj/some/bazel_action/gn_targets_dir",
                "bazel_action_timings": {"bar": 42},
            },
        )

    def test_from_json(self) -> None:
        with self.assertRaises(ValueError) as cm:
            i = build_utils.BazelBuildInvocation.from_json([])  # type: ignore
        self.assertEqual(str(cm.exception), "Input JSON is not an object: []")

        with self.assertRaises(ValueError) as cm:
            i = build_utils.BazelBuildInvocation.from_json({})
        self.assertEqual(
            str(cm.exception), "Missing JSON object key 'bazel_targets'"
        )

        with self.assertRaises(ValueError) as cm:
            i = build_utils.BazelBuildInvocation.from_json(
                {"bazel_targets": []}
            )
        self.assertEqual(
            str(cm.exception), "Missing JSON object key 'build_args'"
        )

        with self.assertRaises(ValueError) as cm:
            i = build_utils.BazelBuildInvocation.from_json(
                {"bazel_targets": [], "build_args": []}
            )
        self.assertEqual(str(cm.exception), "Empty bazel_targets list")

        i = build_utils.BazelBuildInvocation.from_json(
            {
                "bazel_targets": ["//src:foo"],
                "build_args": [],
            }
        )
        self.assertListEqual(i.bazel_targets, ["//src:foo"])
        self.assertListEqual(i.build_args, [])
        self.assertIsNone(i.gn_label)
        self.assertIsNone(i.gn_targets_dir)

        i = build_utils.BazelBuildInvocation.from_json(
            {
                "bazel_targets": ["//src:foo"],
                "build_args": ["--config=host", "--config=remote_cache_only"],
                "gn_label": "//some:bazel_action",
                "gn_targets_dir": "obj/some/bazel_action/gn_targets_dir",
            }
        )
        self.assertListEqual(i.bazel_targets, ["//src:foo"])
        self.assertListEqual(
            i.build_args, ["--config=host", "--config=remote_cache_only"]
        )
        self.assertEqual(i.gn_label, "//some:bazel_action")
        self.assertEqual(
            i.gn_targets_dir, "obj/some/bazel_action/gn_targets_dir"
        )


class LastBazelBuildInvocationsTest(unittest.TestCase):
    def test_from_json(self) -> None:
        with self.assertRaises(ValueError) as cm:
            last = build_utils.LastBazelBuildInvocations.new_from_json({})  # type: ignore
        self.assertEqual(
            str(cm.exception),
            "Input is not a JSON array, got <class 'dict'> instead!",
        )

        last = build_utils.LastBazelBuildInvocations.new_from_json([])
        self.assertListEqual(last.invocations, [])

        last = build_utils.LastBazelBuildInvocations.new_from_json(
            [
                {
                    "bazel_targets": ["//first:target"],
                    "build_args": ["--config=host"],
                    "gn_label": "//first:bazel_action",
                },
                {
                    "bazel_targets": ["//second:target"],
                    "build_args": ["--config=fuchsia"],
                    "gn_label": "//second:bazel_action",
                    "gn_targets_dir": "obj/second/gn_targets_dir",
                },
            ]
        )

        self.assertEqual(len(last.invocations), 2)
        i = last.invocations[0]
        self.assertListEqual(i.bazel_targets, ["//first:target"])
        self.assertListEqual(i.build_args, ["--config=host"])
        self.assertEqual(i.gn_label, "//first:bazel_action")
        self.assertIsNone(i.gn_targets_dir)

        i = last.invocations[1]
        self.assertListEqual(i.bazel_targets, ["//second:target"])
        self.assertListEqual(i.build_args, ["--config=fuchsia"])
        self.assertEqual(i.gn_label, "//second:bazel_action")
        self.assertEqual(i.gn_targets_dir, "obj/second/gn_targets_dir")

    def test_to_json(self) -> None:
        last = build_utils.LastBazelBuildInvocations.new_from_json([])
        self.assertListEqual(last.invocations, [])

        last.append(
            build_utils.BazelBuildInvocation(
                bazel_targets=["//first:target"],
                build_args=["--config=host"],
                gn_label="//first:bazel_action",
            )
        )

        last.append(
            build_utils.BazelBuildInvocation(
                bazel_targets=["//second:target"],
                build_args=["--config=fuchsia"],
                gn_label="//second:bazel_action",
                gn_targets_dir="obj/second/gn_targets_dir",
            )
        )

        last.to_json()

        self.assertListEqual(
            last.to_json(),
            [
                {
                    "bazel_targets": ["//first:target"],
                    "build_args": ["--config=host"],
                    "gn_label": "//first:bazel_action",
                },
                {
                    "bazel_targets": ["//second:target"],
                    "build_args": ["--config=fuchsia"],
                    "gn_label": "//second:bazel_action",
                    "gn_targets_dir": "obj/second/gn_targets_dir",
                },
            ],
        )

    def test_append_to_build_dir(self) -> None:
        with tempfile.TemporaryDirectory() as build_dir:
            # Create initial empty list file.
            file_path = (
                build_utils.LastBazelBuildInvocations.get_build_file_path(
                    build_dir
                )
            )
            file_path.write_text("[]")

            build_utils.LastBazelBuildInvocations.append_to_build_dir(
                build_dir,
                build_utils.BazelBuildInvocation(
                    bazel_targets=["//first:target"],
                    build_args=["--config=host"],
                    gn_label="//first:bazel_action",
                ),
            )

            build_utils.LastBazelBuildInvocations.append_to_build_dir(
                build_dir,
                build_utils.BazelBuildInvocation(
                    bazel_targets=["//second:target"],
                    build_args=["--config=fuchsia"],
                    gn_label="//second:bazel_action",
                    gn_targets_dir="obj/second/gn_targets_dir",
                ),
            )

            last_invocations = (
                build_utils.LastBazelBuildInvocations.new_from_build(build_dir)
            )
            self.assertListEqual(
                last_invocations.to_json(),
                [
                    {
                        "bazel_targets": ["//first:target"],
                        "build_args": ["--config=host"],
                        "gn_label": "//first:bazel_action",
                    },
                    {
                        "bazel_targets": ["//second:target"],
                        "build_args": ["--config=fuchsia"],
                        "gn_label": "//second:bazel_action",
                        "gn_targets_dir": "obj/second/gn_targets_dir",
                    },
                ],
            )


class BazelQueryCacheTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self._root = Path(self._td.name)

    def tearDown(self) -> None:
        self._td.cleanup()

    def test_cache(self) -> None:
        mock_runner = MockCommandRunner()
        launcher = BazelLauncher("/path/to/bazel", runner=mock_runner)
        cache_dir = self._root / "cache"
        cache = BazelQueryCache(cache_dir)

        mock_runner.push_result(stdout="some\nresult\nlines")
        result = cache.get_query_output("query", ["deps(//src:foo)"], launcher)

        assert result  # make mypy happy, self.assertIsNotNone() doesn't work
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
        mock_runner.push_result(stdout="some\nother\nresult\nlines")
        result2 = cache.get_query_output("query", ["deps(//src:foo)"], launcher)
        assert result2
        self.assertIsNotNone(result2)
        self.assertListEqual(result2, result)

        # By changing the query type, this will pop the last pushed
        # result value, while creating a new cache entry.
        result3 = cache.get_query_output(
            "cquery", ["deps(//src:foo)"], launcher
        )
        assert result3
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
        mock_runner = MockCommandRunner()
        launcher = BazelLauncher("/path/to/bazel", runner=mock_runner)
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
        mock_runner.push_result(stdout="some\nresult\nlines")
        result = cache.get_query_output(query_cmd, query_args, launcher)
        assert result
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
        mock_runner.push_result(stdout="other\nresult\nlines")
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


class MockCommandRunnerTest(unittest.TestCase):
    def test_set_command_filter(self) -> None:
        mock_runner = MockCommandRunner()

        def my_filter(args: list[str]) -> build_utils.CommandResult:
            if args == ["echo", "hello"]:
                return build_utils.CommandResult(
                    returncode=0, stdout="world\n", stderr=""
                )
            raise ValueError(f"Unexpected command: {args}")

        mock_runner.set_command_filter(my_filter)

        result = mock_runner.run_command(["echo", "hello"])
        self.assertEqual(result.stdout, "world\n")

        with self.assertRaises(ValueError):
            mock_runner.run_command(["echo", "bad"])

    def test_new_command_filter_from_list(self) -> None:
        mock_runner = MockCommandRunner()

        cmd_list = [
            ("echo hello", "world\n"),
            ("echo bar", "baz\n"),
        ]
        filter_func = MockCommandRunner.new_command_filter_from_list(cmd_list)
        mock_runner.set_command_filter(filter_func)

        result = mock_runner.run_command(["echo", "hello"])
        self.assertEqual(result.stdout, "world\n")

        result = mock_runner.run_command(["echo", "bar"])
        self.assertEqual(result.stdout, "baz\n")

        # Test exhausted list
        with self.assertRaises(ValueError) as context:
            mock_runner.run_command(["echo", "extra"])
        self.assertIn("Too many command invocations", str(context.exception))

    def test_new_command_filter_from_list_mismatch(self) -> None:
        mock_runner = MockCommandRunner()

        cmd_list = [
            ("echo hello", "world\n"),
        ]
        filter_func = MockCommandRunner.new_command_filter_from_list(cmd_list)
        mock_runner.set_command_filter(filter_func)

        with self.assertRaises(ValueError) as context:
            mock_runner.run_command(["echo", "bad"])
        self.assertIn("Unexpected command arguments", str(context.exception))


class MockBazelLauncherTest(unittest.TestCase):
    def test_new_with_empty_outputs(self) -> None:
        launcher = build_utils.MockBazelLauncher.new_with_empty_outputs()

        result = launcher.run_query(
            "query", ["deps(//src:foo)"], ignore_errors=False
        )

        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(result.stderr, "")


if __name__ == "__main__":
    unittest.main()

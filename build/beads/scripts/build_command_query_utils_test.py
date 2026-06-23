# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import pathlib
import unittest
from unittest import mock

import build_command_query_utils
import build_utils
import ninja_artifacts


class TestBuildCommandQueryUtils(unittest.TestCase):
    def test_query_ninja_commands(self) -> None:
        mock_ninja = ninja_artifacts.MockNinjaRunner(
            pathlib.Path("/fuchsia/out/default"),
            "rustc --crate-name bar obj/foo/bar.o\n"
            + "rustc --crate-name baz obj/foo/baz.o\n",
        )

        with mock.patch(
            "pathlib.Path.open",
            mock.mock_open(
                read_data=json.dumps(
                    {
                        "//foo:foo": ["obj/foo/foo.o"],
                        "//foo:bar": ["obj/foo/bar.o"],
                        "//foo:baz": ["obj/foo/baz.o"],
                    }
                )
            ),
        ):
            self.assertDictEqual(
                build_command_query_utils.query_ninja_commands(
                    mock_ninja,
                    pathlib.Path("ninja_outputs.json"),
                    ["//foo:bar", "//foo:baz"],
                ),
                {
                    "//foo:bar": "rustc --crate-name bar obj/foo/bar.o",
                    "//foo:baz": "rustc --crate-name baz obj/foo/baz.o",
                },
            )

        self.assertEqual(
            mock_ninja.last_ninja_args(),
            ["-t", "commands", "-s", "obj/foo/bar.o", "obj/foo/baz.o"],
        )

    def test_query_ninja_commands_empty_labels(self) -> None:
        mock_ninja = mock.Mock()
        self.assertEqual(
            build_command_query_utils.query_ninja_commands(
                mock_ninja, pathlib.Path("ninja_outputs.json"), []
            ),
            {},
        )
        mock_ninja.run_and_extract_output.assert_not_called()

    def test_query_ninja_commands_ninja_error(self) -> None:
        mock_ninja = mock.Mock()
        mock_ninja.run_and_extract_output.side_effect = Exception(
            "Ninja failed"
        )

        with mock.patch(
            "pathlib.Path.open",
            mock.mock_open(
                read_data=json.dumps({"//foo:bar": ["obj/foo/bar.o"]})
            ),
        ):
            with self.assertRaisesRegex(Exception, "Ninja failed"):
                build_command_query_utils.query_ninja_commands(
                    mock_ninja,
                    pathlib.Path("ninja_outputs.json"),
                    ["//foo:bar"],
                )

    def test_query_ninja_commands_mismatch(self) -> None:
        mock_ninja = ninja_artifacts.MockNinjaRunner(
            pathlib.Path("/fuchsia/out/default"),
            "rustc --crate-name bar obj/foo/BLOOP.o\n"
            + "rustc --crate-name baz obj/foo/baz.o\n",
        )

        with mock.patch(
            "pathlib.Path.open",
            mock.mock_open(
                read_data=json.dumps(
                    {
                        "//foo:bar": ["obj/foo/bar.o"],
                        "//foo:baz": ["obj/foo/baz.o"],
                    }
                )
            ),
        ):
            with self.assertRaisesRegex(ValueError, "Could not find command"):
                build_command_query_utils.query_ninja_commands(
                    mock_ninja,
                    pathlib.Path("ninja_outputs.json"),
                    ["//foo:bar", "//foo:baz"],
                )

    def test_query_ninja_commands_missing_command(self) -> None:
        mock_ninja = ninja_artifacts.MockNinjaRunner(
            pathlib.Path("/fuchsia/out/default"),
            "rustc --crate-name bar obj/foo/bar.o\n",
        )
        with mock.patch(
            "pathlib.Path.open",
            mock.mock_open(
                read_data=json.dumps(
                    {
                        "//foo:bar": ["obj/foo/bar.o"],
                        "//foo:baz": ["obj/foo/baz.o"],
                    }
                )
            ),
        ):
            with self.assertRaisesRegex(ValueError, "Could not find command"):
                build_command_query_utils.query_ninja_commands(
                    mock_ninja,
                    pathlib.Path("ninja_outputs.json"),
                    ["//foo:bar", "//foo:baz"],
                )

    def test_query_ninja_commands_missing_label(self) -> None:
        mock_ninja = mock.Mock()
        with mock.patch(
            "pathlib.Path.open",
            mock.mock_open(
                read_data=json.dumps({"//foo:bar": ["obj/foo/bar.o"]})
            ),
        ):
            with self.assertRaisesRegex(
                ValueError, "Could not find outputs for label"
            ):
                build_command_query_utils.query_ninja_commands(
                    mock_ninja,
                    pathlib.Path("ninja_outputs.json"),
                    ["//foo:baz"],
                )

    def test_query_bazel_commands(self) -> None:
        mock_bazel_launcher = build_utils.MockBazelLauncher()
        mock_bazel_launcher.push_expected_outputs(
            [
                json.dumps(
                    {
                        "targets": [
                            {"id": "1", "label": "//foo:bar"},
                            {"id": "2", "label": "//foo:baz"},
                        ],
                        "actions": [
                            {
                                "targetId": "1",
                                "arguments": ["rustc", "--crate-name", "bar"],
                            },
                            {
                                "targetId": "2",
                                "arguments": ["rustc", "--crate-name", "baz"],
                            },
                        ],
                    }
                )
            ]
        )

        self.assertDictEqual(
            build_command_query_utils.query_bazel_commands(
                mock_bazel_launcher, ["//foo:bar", "//foo:baz"]
            ),
            {
                "//foo:bar": "rustc --crate-name bar",
                "//foo:baz": "rustc --crate-name baz",
            },
        )

        last_args = mock_bazel_launcher.command_runner.results[0].args
        self.assertEqual(
            last_args,
            [
                "bazel",
                "aquery",
                "--config=host",
                "--config=quiet",
                "--output=jsonproto",
                'mnemonic("Rustc", //foo:bar + //foo:baz)',
            ],
        )

    def test_query_bazel_commands_error(self) -> None:
        mock_bazel_launcher = build_utils.MockBazelLauncher()
        mock_bazel_launcher.command_runner.push_result(returncode=1)

        with self.assertRaisesRegex(ValueError, "Failed to run bazel aquery"):
            build_command_query_utils.query_bazel_commands(
                mock_bazel_launcher, ["//foo:bar"]
            )

    def test_query_bazel_commands_empty_labels(self) -> None:
        mock_launcher = build_utils.MockBazelLauncher()
        self.assertDictEqual(
            build_command_query_utils.query_bazel_commands(mock_launcher, []),
            {},
        )
        self.assertEqual(len(mock_launcher.command_runner.results), 0)

    def test_query_bazel_commands_invalid_json(self) -> None:
        mock_launcher = build_utils.MockBazelLauncher()
        mock_launcher.push_expected_outputs(["invalid json"])
        with self.assertRaises(json.decoder.JSONDecodeError):
            build_command_query_utils.query_bazel_commands(
                mock_launcher, ["//foo:bar"]
            )

    def test_query_bazel_commands_missing_actions(self) -> None:
        mock_launcher = build_utils.MockBazelLauncher()
        mock_launcher.push_expected_outputs(
            [
                json.dumps(
                    {
                        "targets": [{"id": "1", "label": "//foo:bar"}],
                    }
                )
            ]
        )
        with self.assertRaisesRegex(ValueError, "Could not find command"):
            build_command_query_utils.query_bazel_commands(
                mock_launcher, ["//foo:bar"]
            )

    def test_query_bazel_commands_target_not_in_results(self) -> None:
        mock_launcher = build_utils.MockBazelLauncher()
        mock_launcher.push_expected_outputs(
            [
                json.dumps(
                    {
                        "targets": [{"id": "1", "label": "//foo:baz"}],
                        "actions": [
                            {"targetId": "1", "arguments": ["rustc", "baz"]}
                        ],
                    }
                )
            ]
        )
        with self.assertRaisesRegex(ValueError, "Could not find command"):
            build_command_query_utils.query_bazel_commands(
                mock_launcher, ["//foo:bar"]
            )

    def test_query_bazel_commands_empty_arguments(self) -> None:
        mock_launcher = build_utils.MockBazelLauncher()
        mock_launcher.push_expected_outputs(
            [
                json.dumps(
                    {
                        "targets": [{"id": "1", "label": "//foo:bar"}],
                        "actions": [{"targetId": "1", "arguments": []}],
                    }
                )
            ]
        )
        with self.assertRaisesRegex(ValueError, "Could not find command"):
            build_command_query_utils.query_bazel_commands(
                mock_launcher, ["//foo:bar"]
            )


if __name__ == "__main__":
    unittest.main()

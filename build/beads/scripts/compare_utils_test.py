# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import pathlib
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from build.bazel.scripts import build_utils
from build.bazel.scripts.build_utils import MockCommandRunner, NinjaRunner
from build.beads.scripts import compare_utils
from build.beads.scripts.compare_utils import (
    ACTION_C_COMPILE,
    ACTION_CPP_COMPILE,
    ACTION_CPP_LINK,
    ACTION_RUSTC,
    CompareCommandsQuery,
    CompareCommandsResult,
    ConfiguredBazelLabel,
    compare_gn_and_bazel_commands_for,
)


class TestBuildCommandQueryUtils(unittest.TestCase):
    MOCK_NINJA_BIN = "/mock-ninja"

    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self.root_dir = Path(self._td.name)
        self.build_dir = self.root_dir / "out/default"
        self.build_dir.mkdir(parents=True)
        self.ninja_outputs_json = self.build_dir / "ninja_outputs.json"
        self.ninja_outputs_json.write_text("{}")

    def tearDown(self) -> None:
        self._td.cleanup()

    def _write_ninja_outputs(self, mapping: dict[str, list[str]]) -> None:
        """Write new ninja_outputs.json file."""
        with self.ninja_outputs_json.open("w") as f:
            json.dump(mapping, f)

    def _get_ninja_runners(
        self, output: None | str = None
    ) -> tuple[MockCommandRunner, NinjaRunner]:
        """Return a MockCommandRunner and mock NinjaRunner.

        Args:
            output: Optional string. If not None, a (0, output, "")
               result will be pushed to mock_runner before this function exits.
        Returns:
            a (MockCommandRunner, NinjaRunner) tuple.
        """
        mock_runner = MockCommandRunner()
        mock_ninja = NinjaRunner(
            Path(self.MOCK_NINJA_BIN), self.build_dir, mock_runner
        )
        if output is not None:
            mock_runner.push_result(0, output, "")
        return mock_runner, mock_ninja

    def test_query_ninja_commands(self) -> None:
        mock_runner, mock_ninja = self._get_ninja_runners(
            "rustc --crate-name bar obj/foo/bar.o\n"
            + "rustc --crate-name baz obj/foo/baz.o\n",
        )

        self._write_ninja_outputs(
            {
                "//foo:foo": ["obj/foo/foo.o"],
                "//foo:bar": ["obj/foo/bar.o"],
                "//foo:baz": ["obj/foo/baz.o"],
            }
        )
        self.assertDictEqual(
            compare_utils.query_ninja_commands(
                mock_ninja,
                [
                    CompareCommandsQuery(gn="//foo:bar"),
                    CompareCommandsQuery(gn="//foo:baz"),
                ],
            ),
            {
                "//foo:bar": "rustc --crate-name bar obj/foo/bar.o",
                "//foo:baz": "rustc --crate-name baz obj/foo/baz.o",
            },
        )

        self.assertListEqual(
            mock_runner.results[-1].args,
            [
                self.MOCK_NINJA_BIN,
                "-C",
                str(self.build_dir),
                "-t",
                "commands",
                "-s",
                "obj/foo/bar.o",
                "obj/foo/baz.o",
            ],
        )

    def test_query_ninja_commands_empty_labels(self) -> None:
        mock_runner, mock_ninja = self._get_ninja_runners("")

        self.assertDictEqual(
            compare_utils.query_ninja_commands(mock_ninja, []),
            {},
        )
        self.assertListEqual(mock_runner.commands, [])

    def test_query_ninja_commands_ninja_error(self) -> None:
        mock_runner, mock_ninja = self._get_ninja_runners()
        mock_runner.push_result(1, "", "Ninja failed!")

        self._write_ninja_outputs(
            {
                "//foo:bar": ["obj/foo/bar.o"],
            }
        )
        result = compare_utils.query_ninja_commands(
            mock_ninja, [CompareCommandsQuery(gn="//foo:bar")]
        )
        self.assertDictEqual(result, {})

    def test_query_ninja_commands_mismatch(self) -> None:
        _, mock_ninja = self._get_ninja_runners(
            "rustc --crate-name bar obj/foo/BLOOP.o\n"
            + "rustc --crate-name baz obj/foo/baz.o\n",
        )

        self._write_ninja_outputs(
            {
                "//foo:bar": ["obj/foo/bar.o"],
                "//foo:baz": ["obj/foo/baz.o"],
            }
        )
        result = compare_utils.query_ninja_commands(
            mock_ninja,
            [
                CompareCommandsQuery(gn="//foo:bar"),
                CompareCommandsQuery(gn="//foo:baz"),
            ],
        )
        self.assertDictEqual(
            result,
            {
                "//foo:baz": "rustc --crate-name baz obj/foo/baz.o",
            },
        )

    def test_query_ninja_commands_missing_command(self) -> None:
        _, mock_ninja = self._get_ninja_runners(
            "rustc --crate-name bar obj/foo/bar.o\n",
        )
        self._write_ninja_outputs(
            {
                "//foo:bar": ["obj/foo/bar.o"],
                "//foo:baz": ["obj/foo/baz.o"],
            }
        )
        result = compare_utils.query_ninja_commands(
            mock_ninja,
            [
                CompareCommandsQuery(gn="//foo:bar"),
                CompareCommandsQuery("//foo:baz"),
            ],
        )
        self.assertDictEqual(
            result,
            {"//foo:bar": "rustc --crate-name bar obj/foo/bar.o"},
        )

    def test_query_ninja_commands_missing_label(self) -> None:
        mock_runner, mock_ninja = self._get_ninja_runners()

        self._write_ninja_outputs(
            {
                "//foo:bar": ["obj/foo/bar.o"],
            }
        )
        with self.assertRaisesRegex(
            ValueError, "Could not find outputs for label"
        ):
            compare_utils.query_ninja_commands(
                mock_ninja,
                [CompareCommandsQuery(gn="//foo:baz")],
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
                                "mnemonic": "Rustc",
                            },
                            {
                                "targetId": "2",
                                "arguments": ["rustc", "--crate-name", "baz"],
                                "mnemonic": "Rustc",
                            },
                        ],
                    }
                )
            ]
        )

        self.assertDictEqual(
            compare_utils.query_bazel_commands(
                mock_bazel_launcher,
                "execroot",
                [
                    CompareCommandsQuery(
                        bazel=ConfiguredBazelLabel("//foo:bar", "host")
                    ),
                    CompareCommandsQuery(
                        bazel=ConfiguredBazelLabel("//foo:baz", "host")
                    ),
                ],
            ),
            {
                ConfiguredBazelLabel(
                    "//foo:bar", "host"
                ): "rustc --crate-name bar",
                ConfiguredBazelLabel(
                    "//foo:baz", "host"
                ): "rustc --crate-name baz",
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
                "--consistent_labels",
                "--output=jsonproto",
                'mnemonic("Rustc", //foo:bar + //foo:baz)',
            ],
        )

    def test_query_bazel_commands_multiple_configs(self) -> None:
        mock_bazel_launcher = build_utils.MockBazelLauncher()
        mock_bazel_launcher.push_expected_outputs(
            [
                json.dumps(
                    {
                        "targets": [
                            {"id": "1", "label": "//foo:bar"},
                        ],
                        "actions": [
                            {
                                "targetId": "1",
                                "arguments": [
                                    "rustc",
                                    "--crate-name",
                                    "bar",
                                    "--target=x86_64-unknown-linux-gnu",
                                ],
                                "mnemonic": "Rustc",
                            },
                        ],
                    }
                ),
                json.dumps(
                    {
                        "targets": [
                            {"id": "1", "label": "//foo:bar"},
                        ],
                        "actions": [
                            {
                                "targetId": "1",
                                "arguments": [
                                    "rustc",
                                    "--crate-name",
                                    "bar",
                                    "--target=x86_64-unknown-fuchsia",
                                ],
                                "mnemonic": "Rustc",
                            },
                        ],
                    }
                ),
            ]
        )

        self.assertDictEqual(
            compare_utils.query_bazel_commands(
                mock_bazel_launcher,
                "execroot",
                [
                    CompareCommandsQuery(
                        bazel=ConfiguredBazelLabel("//foo:bar", "host")
                    ),
                    CompareCommandsQuery(
                        bazel=ConfiguredBazelLabel(
                            "//foo:bar", "fuchsia_platform"
                        )
                    ),
                ],
            ),
            {
                ConfiguredBazelLabel(
                    "//foo:bar",
                    "host",
                ): "rustc --crate-name bar --target=x86_64-unknown-linux-gnu",
                ConfiguredBazelLabel(
                    "//foo:bar",
                    "fuchsia_platform",
                ): "rustc --crate-name bar --target=x86_64-unknown-fuchsia",
            },
        )

        self.assertEqual(len(mock_bazel_launcher.command_runner.results), 2)
        self.assertEqual(
            mock_bazel_launcher.command_runner.results[0].args,
            [
                "bazel",
                "aquery",
                "--config=host",
                "--config=quiet",
                "--consistent_labels",
                "--output=jsonproto",
                'mnemonic("Rustc", //foo:bar)',
            ],
        )
        self.assertEqual(
            mock_bazel_launcher.command_runner.results[1].args,
            [
                "bazel",
                "aquery",
                "--config=fuchsia_platform",
                "--config=quiet",
                "--consistent_labels",
                "--output=jsonproto",
                'mnemonic("Rustc", //foo:bar)',
            ],
        )

    def test_query_bazel_commands_with_env_vars(self) -> None:
        mock_bazel_launcher = build_utils.MockBazelLauncher()
        mock_bazel_launcher.push_expected_outputs(
            [
                json.dumps(
                    {
                        "targets": [
                            {"id": "1", "label": "//foo:bar"},
                        ],
                        "actions": [
                            {
                                "targetId": "1",
                                "arguments": ["rustc", "--crate-name", "bar"],
                                "environmentVariables": [
                                    {"key": "CARGO_PKG_NAME", "value": "bar"}
                                ],
                                "mnemonic": "Rustc",
                            },
                        ],
                    }
                )
            ]
        )

        self.assertDictEqual(
            compare_utils.query_bazel_commands(
                mock_bazel_launcher,
                "execroot",
                [
                    CompareCommandsQuery(
                        bazel=ConfiguredBazelLabel("//foo:bar", "host")
                    )
                ],
            ),
            {
                ConfiguredBazelLabel(
                    "//foo:bar",
                    "host",
                ): "CARGO_PKG_NAME=bar rustc --crate-name bar",
            },
        )

    def test_query_bazel_targets_commands_normalized_label(self) -> None:
        mock_bazel_launcher = build_utils.MockBazelLauncher()
        mock_bazel_launcher.push_expected_outputs(
            [
                json.dumps(
                    {
                        "targets": [
                            {"id": "1", "label": "@@//foo:bar"},
                        ],
                        "actions": [
                            {
                                "targetId": "1",
                                "arguments": ["rustc", "--crate-name", "bar"],
                                "mnemonic": "Rustc",
                            },
                        ],
                    }
                )
            ]
        )

        self.assertDictEqual(
            compare_utils.query_bazel_commands(
                mock_bazel_launcher,
                "execroot",
                [
                    CompareCommandsQuery(
                        bazel=ConfiguredBazelLabel("//foo:bar", "host")
                    )
                ],
            ),
            {
                ConfiguredBazelLabel(
                    "//foo:bar", "host"
                ): "rustc --crate-name bar",
            },
        )

    def test_query_bazel_commands_error(self) -> None:
        mock_bazel_launcher = build_utils.MockBazelLauncher()
        mock_bazel_launcher.command_runner.push_result(returncode=1)

        with self.assertRaisesRegex(
            ValueError, "Failed to run bazel action expansion"
        ):
            compare_utils.query_bazel_commands(
                mock_bazel_launcher,
                "execroot",
                [
                    CompareCommandsQuery(
                        bazel=ConfiguredBazelLabel("//foo:bar", "host")
                    )
                ],
            )

    def test_query_bazel_commands_empty_labels(self) -> None:
        mock_launcher = build_utils.MockBazelLauncher()
        self.assertDictEqual(
            compare_utils.query_bazel_commands(mock_launcher, "execroot", []),
            {},
        )
        self.assertEqual(len(mock_launcher.command_runner.results), 0)

    def test_query_bazel_commands_invalid_json(self) -> None:
        mock_launcher = build_utils.MockBazelLauncher()
        mock_launcher.push_expected_outputs(["invalid json"])
        with self.assertRaisesRegex(ValueError, "Could not find command"):
            compare_utils.query_bazel_commands(
                mock_launcher,
                "execroot",
                [
                    CompareCommandsQuery(
                        bazel=ConfiguredBazelLabel("//foo:bar", "host")
                    )
                ],
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
            compare_utils.query_bazel_commands(
                mock_launcher,
                "execroot",
                [
                    CompareCommandsQuery(
                        bazel=ConfiguredBazelLabel("//foo:bar", "host")
                    )
                ],
            )

    def test_query_bazel_commands_target_not_in_results(self) -> None:
        mock_launcher = build_utils.MockBazelLauncher()
        mock_launcher.push_expected_outputs(
            [
                json.dumps(
                    {
                        "targets": [{"id": "1", "label": "//foo:baz"}],
                        "actions": [
                            {
                                "targetId": "1",
                                "arguments": ["rustc", "baz"],
                                "mnemonic": "Rustc",
                            }
                        ],
                    }
                )
            ]
        )
        with self.assertRaisesRegex(ValueError, "Could not find command"):
            compare_utils.query_bazel_commands(
                mock_launcher,
                "execroot",
                [
                    CompareCommandsQuery(
                        bazel=ConfiguredBazelLabel("//foo:bar", "host")
                    )
                ],
            )

    def test_is_pic_compile_action(self) -> None:
        self.assertTrue(
            compare_utils._is_pic_compile_action(
                ["clang++", "-c", "foo.cc", "-o", "bazel-out/foo.pic.o"]
            )
        )
        self.assertTrue(
            compare_utils._is_pic_compile_action(
                ["clang++", "-c", "foo.cc", "-o=bazel-out/foo.pic.o"]
            )
        )
        self.assertTrue(
            compare_utils._is_pic_compile_action(
                ["clang++", "-c", "foo.cc", "--output=bazel-out/foo.pic.o"]
            )
        )
        self.assertTrue(
            compare_utils._is_pic_compile_action(
                ["clang++", "-c", "foo.cc", "-obazel-out/foo.pic.o"]
            )
        )
        self.assertFalse(
            compare_utils._is_pic_compile_action(
                ["clang++", "-c", "foo.cc", "-o", "bazel-out/foo.o"]
            )
        )
        self.assertFalse(
            compare_utils._is_pic_compile_action(
                ["clang++", "-c", "foo.cc", "-o=bazel-out/foo.o"]
            )
        )
        self.assertFalse(
            compare_utils._is_pic_compile_action(
                ["clang++", "-c", "foo.cc", "--output=bazel-out/foo.o"]
            )
        )
        self.assertFalse(
            compare_utils._is_pic_compile_action(["clang++", "-c", "foo.cc"])
        )

    def test_query_bazel_commands_prefer_non_pic_for_static(self) -> None:
        mock_bazel_launcher = build_utils.MockBazelLauncher()
        mock_bazel_launcher.push_expected_outputs(
            [
                json.dumps(
                    {
                        "targets": [{"id": "1", "label": "//foo:bar"}],
                        "actions": [
                            {
                                "targetId": "1",
                                "arguments": [
                                    "clang++",
                                    "-fPIC",
                                    "-c",
                                    "foo.cc",
                                    "-o",
                                    "bazel-out/foo.pic.o",
                                ],
                                "mnemonic": "CppCompile",
                            },
                            {
                                "targetId": "1",
                                "arguments": [
                                    "clang++",
                                    "-c",
                                    "foo.cc",
                                    "-o",
                                    "bazel-out/foo.o",
                                ],
                                "mnemonic": "CppCompile",
                            },
                        ],
                    }
                )
            ]
        )

        res = compare_utils.query_bazel_commands(
            mock_bazel_launcher,
            "execroot",
            [
                CompareCommandsQuery(
                    gn="//foo:bar",
                    bazel=ConfiguredBazelLabel("//foo:bar", "host"),
                    action_type=ACTION_CPP_COMPILE,
                )
            ],
        )
        self.assertEqual(
            res[ConfiguredBazelLabel("//foo:bar", "host")],
            "clang++ -c foo.cc -o bazel-out/foo.o",
        )

    def test_query_bazel_commands_prefer_pic_for_shared_toolchain(self) -> None:
        mock_bazel_launcher = build_utils.MockBazelLauncher()
        mock_bazel_launcher.push_expected_outputs(
            [
                json.dumps(
                    {
                        "targets": [{"id": "1", "label": "//foo:bar"}],
                        "actions": [
                            {
                                "targetId": "1",
                                "arguments": [
                                    "clang++",
                                    "-fPIC",
                                    "-c",
                                    "foo.cc",
                                    "-o",
                                    "bazel-out/foo.pic.o",
                                ],
                                "mnemonic": "CppCompile",
                            },
                            {
                                "targetId": "1",
                                "arguments": [
                                    "clang++",
                                    "-c",
                                    "foo.cc",
                                    "-o",
                                    "bazel-out/foo.o",
                                ],
                                "mnemonic": "CppCompile",
                            },
                        ],
                    }
                )
            ]
        )

        res = compare_utils.query_bazel_commands(
            mock_bazel_launcher,
            "execroot",
            [
                CompareCommandsQuery(
                    gn="//foo:bar(//build/toolchain/fuchsia:x64-shared)",
                    bazel=ConfiguredBazelLabel("//foo:bar", "fuchsia_platform"),
                    action_type=ACTION_CPP_COMPILE,
                )
            ],
        )
        self.assertEqual(
            res[
                ConfiguredBazelLabel(
                    "//foo:bar", "fuchsia_platform", use_pic=True
                )
            ],
            "clang++ -fPIC -c foo.cc -o bazel-out/foo.pic.o",
        )

    def test_query_bazel_commands_pic_fallback(self) -> None:
        mock_bazel_launcher = build_utils.MockBazelLauncher()
        mock_bazel_launcher.push_expected_outputs(
            [
                json.dumps(
                    {
                        "targets": [{"id": "1", "label": "//foo:bar"}],
                        "actions": [
                            {
                                "targetId": "1",
                                "arguments": [
                                    "clang++",
                                    "-fPIC",
                                    "-c",
                                    "foo.cc",
                                    "-o",
                                    "bazel-out/foo.pic.o",
                                ],
                                "mnemonic": "CppCompile",
                            },
                        ],
                    }
                )
            ]
        )

        res = compare_utils.query_bazel_commands(
            mock_bazel_launcher,
            "execroot",
            [
                CompareCommandsQuery(
                    gn="//foo:bar",
                    bazel=ConfiguredBazelLabel("//foo:bar", "host"),
                    action_type=ACTION_CPP_COMPILE,
                )
            ],
        )
        self.assertEqual(
            res[ConfiguredBazelLabel("//foo:bar", "host")],
            "clang++ -fPIC -c foo.cc -o bazel-out/foo.pic.o",
        )

    def test_query_bazel_commands_empty_arguments(self) -> None:
        mock_launcher = build_utils.MockBazelLauncher()
        mock_launcher.push_expected_outputs(
            [
                json.dumps(
                    {
                        "targets": [{"id": "1", "label": "//foo:bar"}],
                        "actions": [
                            {
                                "targetId": "1",
                                "arguments": [],
                                "mnemonic": "Rustc",
                            }
                        ],
                    }
                )
            ]
        )
        with self.assertRaisesRegex(ValueError, "Could not find command"):
            compare_utils.query_bazel_commands(
                mock_launcher,
                "execroot",
                [
                    CompareCommandsQuery(
                        bazel=ConfiguredBazelLabel("//foo:bar", "host")
                    )
                ],
            )

    def test_query_ninja_and_bazel_commands(self) -> None:
        mock_runner, mock_ninja = self._get_ninja_runners(
            "rustc --crate-name bar obj/foo/bar.o\n"
            + "rustc --crate-name baz obj/foo/baz.o\n",
        )

        self._write_ninja_outputs(
            {
                "//foo:foo": ["obj/foo/foo.o"],
                "//foo:bar": ["obj/foo/bar.o"],
                "//foo:baz": ["obj/foo/baz.o"],
            }
        )

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
                                "mnemonic": "Rustc",
                            },
                            {
                                "targetId": "2",
                                "arguments": ["rustc", "--crate-name", "baz"],
                                "mnemonic": "Rustc",
                            },
                        ],
                    }
                )
            ]
        )

        (
            gn_cmds_map,
            bazel_cmds_map,
        ) = compare_utils.query_ninja_and_bazel_commands(
            [
                CompareCommandsQuery(
                    gn="//foo:bar",
                    bazel=ConfiguredBazelLabel("//foo:bar", "host"),
                ),
                CompareCommandsQuery(
                    gn="//foo:baz",
                    bazel=ConfiguredBazelLabel("//foo:baz", "host"),
                ),
            ],
            mock_ninja,
            mock_bazel_launcher,
            "execroot",
        )

        self.assertDictEqual(
            gn_cmds_map,
            {
                "//foo:bar": "rustc --crate-name bar obj/foo/bar.o",
                "//foo:baz": "rustc --crate-name baz obj/foo/baz.o",
            },
        )

        self.assertDictEqual(
            bazel_cmds_map,
            {
                ConfiguredBazelLabel(
                    "//foo:bar", "host"
                ): "rustc --crate-name bar",
                ConfiguredBazelLabel(
                    "//foo:baz", "host"
                ): "rustc --crate-name baz",
            },
        )

        self.assertListEqual(
            mock_runner.results[-1].args,
            [
                "/mock-ninja",
                "-C",
                str(self.build_dir),
                "-t",
                "commands",
                "-s",
                "obj/foo/bar.o",
                "obj/foo/baz.o",
            ],
        )

    def test_action_type_to_mnemonic(self) -> None:
        self.assertEqual(
            compare_utils.action_type_to_mnemonic("rustc"), "Rustc"
        )
        self.assertEqual(
            compare_utils.action_type_to_mnemonic("c_compile"),
            "CppCompile",
        )
        self.assertEqual(
            compare_utils.action_type_to_mnemonic("cpp_compile"),
            "CppCompile",
        )
        self.assertEqual(
            compare_utils.action_type_to_mnemonic("assemble"),
            "CppCompile",
        )
        self.assertEqual(
            compare_utils.action_type_to_mnemonic("cpp_link"),
            "CppLink",
        )
        with self.assertRaises(ValueError):
            compare_utils.action_type_to_mnemonic("invalid_type")

    def test_query_ninja_commands_cpp(self) -> None:
        mock_output = (
            "clang++ -std=c++20 -c ../../src/main.cc -o obj/src/main.o\n"
            + "clang -c ../../src/helper.c -o obj/src/helper.o\n"
            + "clang -c ../../src/entry.S -o obj/src/entry.o\n"
            + "clang++ obj/src/main.o obj/src/helper.o -o host_x64/my_bin\n"
        )
        self._write_ninja_outputs({"//cc:my_bin": ["host_x64/my_bin"]})

        mock_runner, mock_ninja = self._get_ninja_runners(mock_output)
        res_cpp = compare_utils.query_ninja_commands(
            mock_ninja,
            [CompareCommandsQuery(gn="//cc:my_bin", action_type="cpp_compile")],
        )
        self.assertIn("src/main.cc", res_cpp["//cc:my_bin"])

        mock_runner, mock_ninja = self._get_ninja_runners(mock_output)
        res_c = compare_utils.query_ninja_commands(
            mock_ninja,
            [CompareCommandsQuery(gn="//cc:my_bin", action_type="c_compile")],
        )
        self.assertIn("src/helper.c", res_c["//cc:my_bin"])

        mock_runner, mock_ninja = self._get_ninja_runners(mock_output)
        res_asm = compare_utils.query_ninja_commands(
            mock_ninja,
            [CompareCommandsQuery(gn="//cc:my_bin", action_type="assemble")],
        )
        self.assertIn("src/entry.S", res_asm["//cc:my_bin"])

        mock_runner, mock_ninja = self._get_ninja_runners(mock_output)
        res_link = compare_utils.query_ninja_commands(
            mock_ninja,
            [CompareCommandsQuery(gn="//cc:my_bin", action_type="cpp_link")],
        )
        self.assertIn("host_x64/my_bin", res_link["//cc:my_bin"])

    def test_query_ninja_commands_batched(self) -> None:
        mock_output = (
            "rustc --crate-name my_lib obj/rust/libmy_lib.rlib\n"
            + "clang++ -std=c++20 -c ../../src/cc_lib.cc -o obj/cc/cc_lib.o\n"
            + "clang -c ../../src/c_lib.c -o obj/cc/c_lib.o\n"
            + "clang++ obj/cc/cc_lib.o obj/cc/c_lib.o -o host_x64/cc_bin\n"
        )
        mock_runner, mock_ninja = self._get_ninja_runners(mock_output)
        self._write_ninja_outputs(
            {
                "//rust:my_lib": ["obj/rust/libmy_lib.rlib"],
                "//cc:cc_lib": ["obj/cc/cc_lib.o"],
                "//cc:c_lib": ["obj/cc/c_lib.o"],
                "//cc:cc_bin": ["host_x64/cc_bin"],
            }
        )
        targets = [
            CompareCommandsQuery(gn="//rust:my_lib", action_type="rustc"),
            CompareCommandsQuery(gn="//cc:cc_lib", action_type="cpp_compile"),
            CompareCommandsQuery(gn="//cc:c_lib", action_type="c_compile"),
            CompareCommandsQuery(gn="//cc:cc_bin", action_type="cpp_link"),
        ]
        results = compare_utils.query_ninja_commands(
            mock_ninja,
            targets,
        )
        self.assertEqual(len(results), 4)
        self.assertIn("rustc", results["//rust:my_lib"])
        self.assertIn("cc_lib.cc", results["//cc:cc_lib"])
        self.assertIn("c_lib.c", results["//cc:c_lib"])
        self.assertIn("host_x64/cc_bin", results["//cc:cc_bin"])

        self.assertEqual(
            mock_runner.results[-1].args,
            [
                "/mock-ninja",
                "-C",
                str(self.build_dir),
                "-t",
                "commands",
                "-s",
                "obj/rust/libmy_lib.rlib",
                "obj/cc/cc_lib.o",
                "obj/cc/c_lib.o",
                "host_x64/cc_bin",
            ],
        )

    def test_query_ninja_commands_host_vs_fuchsia_outputs(self) -> None:
        mock_output = (
            "rustc --target=x86_64-unknown-linux-gnu -o host_x64/obj/rust/libmy_lib.rlib\n"
            + "clang -target x86_64-unknown-linux-gnu -c ../../src/c_lib.c -o host_x64/obj/cc/c_lib.o\n"
            + "rustc --target=x86_64-unknown-fuchsia -o obj/rust/libmy_lib.rlib\n"
            + "clang -target x86_64-unknown-fuchsia -c ../../src/c_lib.c -o obj/cc/c_lib.o\n"
        )
        _, mock_ninja = self._get_ninja_runners(mock_output)
        self._write_ninja_outputs(
            {
                "//rust:my_lib(//build/toolchain:host_x64)": [
                    "host_x64/obj/rust/libmy_lib.rlib"
                ],
                "//rust:my_lib": ["obj/rust/libmy_lib.rlib"],
                "//cc:c_lib(//build/toolchain:host_x64)": [
                    "host_x64/obj/cc/c_lib.o"
                ],
                "//cc:c_lib": ["obj/cc/c_lib.o"],
            }
        )
        targets = [
            CompareCommandsQuery(
                gn="//rust:my_lib(//build/toolchain:host_x64)",
                action_type="rustc",
            ),
            CompareCommandsQuery(
                gn="//cc:c_lib(//build/toolchain:host_x64)",
                action_type="c_compile",
            ),
            CompareCommandsQuery(gn="//rust:my_lib", action_type="rustc"),
            CompareCommandsQuery(gn="//cc:c_lib", action_type="c_compile"),
        ]
        results = compare_utils.query_ninja_commands(mock_ninja, targets)
        self.assertIn(
            "x86_64-unknown-linux-gnu",
            results["//rust:my_lib(//build/toolchain:host_x64)"],
        )
        self.assertIn(
            "x86_64-unknown-fuchsia",
            results["//rust:my_lib"],
        )
        self.assertIn(
            "x86_64-unknown-linux-gnu",
            results["//cc:c_lib(//build/toolchain:host_x64)"],
        )
        self.assertIn(
            "x86_64-unknown-fuchsia",
            results["//cc:c_lib"],
        )

    def test_query_bazel_commands_batched(self) -> None:
        mock_launcher = build_utils.MockBazelLauncher()
        mock_launcher.push_expected_outputs(
            [
                json.dumps(
                    {
                        "targets": [
                            {"id": "1", "label": "//rust:my_lib"},
                            {"id": "2", "label": "//cc:cc_lib"},
                            {"id": "3", "label": "//cc:cc_bin"},
                        ],
                        "actions": [
                            {
                                "targetId": "1",
                                "arguments": [
                                    "rustc",
                                    "--crate-name",
                                    "my_lib",
                                ],
                                "mnemonic": "Rustc",
                            },
                            {
                                "targetId": "2",
                                "arguments": [
                                    "clang++",
                                    "-std=c++20",
                                    "-c",
                                    "src/cc_lib.cc",
                                    "-o",
                                    "cc_lib.o",
                                ],
                                "mnemonic": "CppCompile",
                            },
                            {
                                "targetId": "3",
                                "arguments": [
                                    "clang++",
                                    "cc_lib.o",
                                    "-o",
                                    "cc_bin",
                                ],
                                "mnemonic": "CppLink",
                            },
                        ],
                    }
                )
            ]
        )
        targets = [
            CompareCommandsQuery(
                bazel=ConfiguredBazelLabel("//rust:my_lib", "host"),
                action_type="rustc",
            ),
            CompareCommandsQuery(
                bazel=ConfiguredBazelLabel("//cc:cc_lib", "host"),
                action_type="cpp_compile",
            ),
            CompareCommandsQuery(
                bazel=ConfiguredBazelLabel("//cc:cc_bin", "host"),
                action_type="cpp_link",
            ),
        ]
        results = compare_utils.query_bazel_commands(
            mock_launcher,
            "execroot",
            targets,
        )
        self.assertEqual(len(results), 3)
        self.assertIn(
            "rustc", results[ConfiguredBazelLabel("//rust:my_lib", "host")]
        )
        self.assertIn(
            "src/cc_lib.cc",
            results[ConfiguredBazelLabel("//cc:cc_lib", "host")],
        )
        self.assertIn(
            "cc_bin", results[ConfiguredBazelLabel("//cc:cc_bin", "host")]
        )

        last_args = mock_launcher.command_runner.results[0].args
        self.assertEqual(
            last_args,
            [
                "bazel",
                "aquery",
                "--config=host",
                "--config=quiet",
                "--consistent_labels",
                "--output=jsonproto",
                'mnemonic("Rustc|CppCompile|CppLink", //rust:my_lib + //cc:cc_lib + //cc:cc_bin)',
            ],
        )

    def test_query_bazel_commands_cpp(self) -> None:
        mock_launcher = build_utils.MockBazelLauncher()
        mock_launcher.push_expected_outputs(
            [
                json.dumps(
                    {
                        "targets": [
                            {"id": "1", "label": "//cc:my_bin"},
                        ],
                        "actions": [
                            {
                                "targetId": "1",
                                "arguments": [
                                    "clang++",
                                    "-c",
                                    "main.cc",
                                    "-o",
                                    "main.o",
                                ],
                                "mnemonic": "CppCompile",
                            },
                            {
                                "targetId": "1",
                                "arguments": [
                                    "clang++",
                                    "main.o",
                                    "-o",
                                    "my_bin",
                                ],
                                "mnemonic": "CppLink",
                            },
                        ],
                    }
                )
            ]
        )
        link_cmd = compare_utils.query_bazel_commands(
            mock_launcher,
            "execroot",
            [
                CompareCommandsQuery(
                    bazel=ConfiguredBazelLabel("//cc:my_bin", "host"),
                    action_type="cpp_link",
                )
            ],
        )
        self.assertIn(
            "my_bin", link_cmd[ConfiguredBazelLabel("//cc:my_bin", "host")]
        )

    def test_query_bazel_commands_cpp_pic_selection(self) -> None:
        mock_launcher = build_utils.MockBazelLauncher()
        mock_launcher.push_expected_outputs(
            [
                json.dumps(
                    {
                        "targets": [
                            {"id": "1", "label": "//cc:my_lib"},
                        ],
                        "actions": [
                            {
                                "targetId": "1",
                                "arguments": [
                                    "clang++",
                                    "-fPIE",
                                    "-c",
                                    "lib.cc",
                                    "-o",
                                    "lib.o",
                                ],
                                "mnemonic": "CppCompile",
                            },
                            {
                                "targetId": "1",
                                "arguments": [
                                    "clang++",
                                    "-fPIC",
                                    "-c",
                                    "lib.cc",
                                    "-o",
                                    "lib.pic.o",
                                ],
                                "mnemonic": "CppCompile",
                            },
                        ],
                    }
                )
            ]
        )
        non_pic_query = CompareCommandsQuery(
            gn="//cc:my_lib(//build/toolchain/fuchsia:x64)",
            bazel=ConfiguredBazelLabel(
                "//cc:my_lib", "fuchsia_platform", use_pic=False
            ),
            action_type="cpp_compile",
        )
        pic_query = CompareCommandsQuery(
            gn="//cc:my_lib(//build/toolchain/fuchsia:x64-shared)",
            bazel=ConfiguredBazelLabel(
                "//cc:my_lib", "fuchsia_platform", use_pic=True
            ),
            action_type="cpp_compile",
        )
        cmds = compare_utils.query_bazel_commands(
            mock_launcher,
            "execroot",
            [non_pic_query, pic_query],
        )
        assert non_pic_query.bazel
        assert pic_query.bazel
        self.assertIn("-fPIE", cmds[non_pic_query.bazel])
        self.assertIn("lib.o", cmds[non_pic_query.bazel])
        self.assertIn("-fPIC", cmds[pic_query.bazel])
        self.assertIn("lib.pic.o", cmds[pic_query.bazel])

    def test_target_query_dataclass(self) -> None:
        t_gn = CompareCommandsQuery(gn="//foo:bar")
        self.assertEqual(t_gn.gn, "//foo:bar")
        self.assertIsNone(t_gn.bazel)
        self.assertEqual(t_gn.action_type, "rustc")
        self.assertIsNone(t_gn.source)

        t_bazel = CompareCommandsQuery(
            bazel=ConfiguredBazelLabel("//foo:baz", "host"),
            action_type="cpp_compile",
            source="foo.cc",
        )
        self.assertEqual(t_bazel.gn, "")
        self.assertIsNotNone(t_bazel.bazel)
        self.assertEqual(t_bazel.bazel_label, "//foo:baz")
        self.assertEqual(t_bazel.bazel_config, "host")
        self.assertEqual(t_bazel.action_type, "cpp_compile")
        self.assertEqual(t_bazel.source, "foo.cc")

        t_dict = CompareCommandsQuery.from_dict(
            {
                "gn": "//a:b",
                "bazel": "//c:d",
                "bazel_config": "fuchsia_platform",
                "type": "c_compile",
                "source": "src.c",
            }
        )
        self.assertEqual(t_dict.gn, "//a:b")
        self.assertIsNotNone(t_dict.bazel)
        assert t_dict.bazel  # make mypy happy
        self.assertEqual(t_dict.bazel.label, "//c:d")
        self.assertEqual(t_dict.bazel.config, "fuchsia_platform")
        self.assertEqual(t_dict.action_type, "c_compile")
        self.assertEqual(t_dict.source, "src.c")

        # Must have at least one of gn or bazel set
        with self.assertRaises(ValueError):
            CompareCommandsQuery()

        with self.assertRaises(ValueError):
            CompareCommandsQuery(gn="", bazel=None)

        # Must have bazel_config when bazel is set
        with self.assertRaises(ValueError):
            CompareCommandsQuery(bazel=ConfiguredBazelLabel("//foo:bar", ""))

        with self.assertRaises(ValueError):
            CompareCommandsQuery.from_dict({"gn": "//a:b", "bazel": "//c:d"})

        # Invalid bazel_config
        with self.assertRaises(ValueError):
            CompareCommandsQuery(
                bazel=ConfiguredBazelLabel("//foo:bar", "invalid_config")
            )

        # Invalid action type
        with self.assertRaises(ValueError):
            CompareCommandsQuery(gn="//foo:bar", action_type="unknown_action")


class CompareCommandsResultTest(unittest.TestCase):
    def test_default_fields(self) -> None:
        query = CompareCommandsQuery(
            gn="//src:foo", bazel=ConfiguredBazelLabel("//src:foo", "host")
        )
        res = CompareCommandsResult(query=query)

        self.assertEqual(res.query, query)
        self.assertEqual(res.error, "")
        self.assertEqual(res.gn_cmd_str, "")
        self.assertEqual(res.bazel_cmd_str, "")
        self.assertEqual(res.normalized_gn_args, [])
        self.assertEqual(res.normalized_bazel_args, [])

    def test_custom_fields(self) -> None:
        query = CompareCommandsQuery(
            gn="//src:foo",
            bazel=ConfiguredBazelLabel("//src:foo", "host"),
            action_type=ACTION_RUSTC,
        )
        res = CompareCommandsResult(
            query=query,
            error="error message",
            gn_cmd_str="gn raw command",
            bazel_cmd_str="bazel raw command",
            normalized_gn_args=["--arg1", "val1"],
            normalized_bazel_args=["--arg1", "val2"],
        )

        self.assertEqual(res.query, query)
        self.assertEqual(res.error, "error message")
        self.assertEqual(res.gn_cmd_str, "gn raw command")
        self.assertEqual(res.bazel_cmd_str, "bazel raw command")
        self.assertEqual(res.normalized_gn_args, ["--arg1", "val1"])
        self.assertEqual(res.normalized_bazel_args, ["--arg1", "val2"])


class CompareGnAndBazelCommandsForTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self.fuchsia_dir = pathlib.Path(self._td.name) / "fuchsia"
        self.fuchsia_dir.mkdir()
        (self.fuchsia_dir / ".jiri_manifest").write_text("")

        self.build_dir = self.fuchsia_dir / "out" / "default"
        self.build_dir.mkdir(parents=True)

        build_utils.BazelPaths.write_topdir_config_for_test(
            self.fuchsia_dir, "gen/build/bazel"
        )
        self.bazel_paths = build_utils.BazelPaths(
            self.fuchsia_dir, self.build_dir
        )

    def tearDown(self) -> None:
        self._td.cleanup()

    def test_empty_target_queries(self) -> None:
        with mock.patch.object(
            compare_utils, "query_ninja_and_bazel_commands"
        ) as mock_query:
            mock_query.return_value = ({}, {})
            results = compare_gn_and_bazel_commands_for([], self.bazel_paths)

        self.assertEqual(results, [])
        mock_query.assert_called_once_with(
            [],
            mock.ANY,
            mock.ANY,
            self.bazel_paths.execroot,
            read_response_files=False,
            debug=None,
        )

    def test_compare_rustc_success(self) -> None:
        query = CompareCommandsQuery(
            gn="//src/lib:bar",
            bazel=ConfiguredBazelLabel("//src/lib:bar", "host"),
            action_type=ACTION_RUSTC,
        )
        gn_cmd = "../../prebuilt/third_party/rust/bin/rustc --crate-name=bar ../../src/lib/bar.rs"
        bazel_cmd = "prebuilt/third_party/rust/bin/rustc --crate-name=bar src/lib/bar.rs"

        with mock.patch.object(
            compare_utils, "query_ninja_and_bazel_commands"
        ) as mock_query:
            mock_query.return_value = (
                {query.gn: gn_cmd},
                {query.bazel: bazel_cmd},
            )
            results = compare_gn_and_bazel_commands_for(
                [query], self.bazel_paths
            )

        self.assertEqual(len(results), 1)
        res = results[0]
        self.assertEqual(res.query, query)
        self.assertEqual(res.error, "")
        self.assertEqual(res.gn_cmd_str, gn_cmd)
        self.assertEqual(res.bazel_cmd_str, bazel_cmd)
        self.assertEqual(
            res.normalized_gn_args,
            ["--crate-name=bar", "rustc", "{SOURCE_ROOT}/src/lib/bar.rs"],
        )
        self.assertEqual(
            res.normalized_bazel_args,
            ["--crate-name=bar", "rustc", "{SOURCE_ROOT}/src/lib/bar.rs"],
        )

    def test_compare_clang_cpp_compile_success(self) -> None:
        query = CompareCommandsQuery(
            gn="//src/app:foo",
            bazel=ConfiguredBazelLabel("//src/app:foo", "host"),
            action_type=ACTION_CPP_COMPILE,
        )
        gn_cmd = "../../prebuilt/third_party/clang/bin/clang++ -c ../../src/app/foo.cc -o obj/src/app/foo.o"
        bazel_cmd = "prebuilt/third_party/clang/bin/clang++ -c src/app/foo.cc -o bazel-out/src/app/foo.o"

        with mock.patch.object(
            compare_utils, "query_ninja_and_bazel_commands"
        ) as mock_query:
            mock_query.return_value = (
                {query.gn: gn_cmd},
                {query.bazel: bazel_cmd},
            )
            results = compare_gn_and_bazel_commands_for(
                [query], self.bazel_paths
            )

        self.assertEqual(len(results), 1)
        res = results[0]
        self.assertEqual(res.query, query)
        self.assertEqual(res.error, "")
        self.assertEqual(res.gn_cmd_str, gn_cmd)
        self.assertEqual(res.bazel_cmd_str, bazel_cmd)
        self.assertIn("clang++", res.normalized_gn_args)
        self.assertIn("{SOURCE_ROOT}/src/app/foo.cc", res.normalized_gn_args)
        self.assertIn("clang++", res.normalized_bazel_args)
        self.assertIn("{SOURCE_ROOT}/src/app/foo.cc", res.normalized_bazel_args)

    def test_compare_clang_other_tool_candidates(self) -> None:
        # Test c_compile with clang
        query_c = CompareCommandsQuery(
            gn="//src/app:c_target",
            bazel=ConfiguredBazelLabel("//src/app:c_target", "host"),
            action_type=ACTION_C_COMPILE,
        )
        gn_c_cmd = "../../prebuilt/third_party/clang/bin/clang -c ../../src/app/foo.c -o obj/foo.o"
        bazel_c_cmd = "prebuilt/third_party/clang/bin/clang -c src/app/foo.c -o bazel-out/foo.o"

        # Test cpp_link with lld
        query_link = CompareCommandsQuery(
            gn="//src/app:link_target",
            bazel=ConfiguredBazelLabel("//src/app:link_target", "host"),
            action_type=ACTION_CPP_LINK,
        )
        gn_link_cmd = "../../prebuilt/third_party/clang/bin/lld -o obj/foo.so ../../src/app/foo.o"
        bazel_link_cmd = "prebuilt/third_party/clang/bin/lld -o bazel-out/foo.so src/app/foo.o"

        with mock.patch.object(
            compare_utils, "query_ninja_and_bazel_commands"
        ) as mock_query:
            mock_query.return_value = (
                {query_c.gn: gn_c_cmd, query_link.gn: gn_link_cmd},
                {
                    query_c.bazel: bazel_c_cmd,
                    query_link.bazel: bazel_link_cmd,
                },
            )
            results = compare_gn_and_bazel_commands_for(
                [query_c, query_link], self.bazel_paths
            )

        self.assertEqual(len(results), 2)
        self.assertEqual(results[0].error, "")
        self.assertIn("clang", results[0].normalized_gn_args)
        self.assertIn("clang", results[0].normalized_bazel_args)
        self.assertEqual(results[1].error, "")
        self.assertIn("lld", results[1].normalized_gn_args)
        self.assertIn("lld", results[1].normalized_bazel_args)

    def test_wrapped_and_chained_commands(self) -> None:
        query = CompareCommandsQuery(
            gn="//src/lib:bar",
            bazel=ConfiguredBazelLabel("//src/lib:bar", "host"),
            action_type=ACTION_RUSTC,
        )
        gn_cmd = "touch obj/stamp && ../../prebuilt/third_party/rust/bin/rustc --crate-name=bar ../../src/lib/bar.rs"
        bazel_cmd = "wrapper -- prebuilt/third_party/rust/bin/rustc --crate-name=bar src/lib/bar.rs"

        with mock.patch.object(
            compare_utils, "query_ninja_and_bazel_commands"
        ) as mock_query:
            mock_query.return_value = (
                {query.gn: gn_cmd},
                {query.bazel: bazel_cmd},
            )
            results = compare_gn_and_bazel_commands_for(
                [query], self.bazel_paths
            )

        self.assertEqual(len(results), 1)
        res = results[0]
        self.assertEqual(res.error, "")
        self.assertEqual(res.gn_cmd_str, gn_cmd)
        self.assertEqual(res.bazel_cmd_str, bazel_cmd)
        self.assertEqual(
            res.normalized_gn_args,
            ["--crate-name=bar", "rustc", "{SOURCE_ROOT}/src/lib/bar.rs"],
        )
        self.assertEqual(
            res.normalized_bazel_args,
            ["--crate-name=bar", "rustc", "{SOURCE_ROOT}/src/lib/bar.rs"],
        )

    def test_no_matching_tool_candidate_fallback(self) -> None:
        query = CompareCommandsQuery(
            gn="//src/lib:bar",
            bazel=ConfiguredBazelLabel("//src/lib:bar", "host"),
            action_type=ACTION_RUSTC,
        )
        gn_cmd = "custom_tool --crate-name=bar ../../src/lib/bar.rs"
        bazel_cmd = "custom_tool --crate-name=bar src/lib/bar.rs"

        with mock.patch.object(
            compare_utils, "query_ninja_and_bazel_commands"
        ) as mock_query:
            mock_query.return_value = (
                {query.gn: gn_cmd},
                {query.bazel: bazel_cmd},
            )
            results = compare_gn_and_bazel_commands_for(
                [query], self.bazel_paths
            )

        self.assertEqual(len(results), 1)
        res = results[0]
        self.assertEqual(res.error, "")
        self.assertIn("custom_tool", res.normalized_gn_args)
        self.assertIn("{SOURCE_ROOT}/custom_tool", res.normalized_bazel_args)

    def test_missing_gn_command(self) -> None:
        query = CompareCommandsQuery(
            gn="//src:bar",
            bazel=ConfiguredBazelLabel("//src:bar", "host"),
            action_type=ACTION_RUSTC,
        )
        bazel_cmd = "rustc src/bar.rs"

        with mock.patch.object(
            compare_utils, "query_ninja_and_bazel_commands"
        ) as mock_query:
            mock_query.return_value = (
                {},
                {query.bazel: bazel_cmd},
            )
            results = compare_gn_and_bazel_commands_for(
                [query], self.bazel_paths
            )

        self.assertEqual(len(results), 1)
        res = results[0]
        self.assertEqual(
            res.error,
            "Failed to get GN or Bazel command for //src:bar vs //src:bar.",
        )
        self.assertEqual(res.gn_cmd_str, "")
        self.assertEqual(res.bazel_cmd_str, bazel_cmd)
        self.assertEqual(res.normalized_gn_args, [])
        self.assertEqual(res.normalized_bazel_args, [])

    def test_missing_bazel_command(self) -> None:
        query = CompareCommandsQuery(
            gn="//src:bar",
            bazel=ConfiguredBazelLabel("//src:bar", "host"),
            action_type=ACTION_RUSTC,
        )
        gn_cmd = "rustc ../../src/bar.rs"

        with mock.patch.object(
            compare_utils, "query_ninja_and_bazel_commands"
        ) as mock_query:
            mock_query.return_value = ({query.gn: gn_cmd}, {})
            results = compare_gn_and_bazel_commands_for(
                [query], self.bazel_paths
            )

        self.assertEqual(len(results), 1)
        res = results[0]
        self.assertEqual(
            res.error,
            "Failed to get GN or Bazel command for //src:bar vs //src:bar.",
        )
        self.assertEqual(res.gn_cmd_str, gn_cmd)
        self.assertEqual(res.bazel_cmd_str, "")
        self.assertEqual(res.normalized_gn_args, [])
        self.assertEqual(res.normalized_bazel_args, [])

    def test_missing_both_commands(self) -> None:
        query = CompareCommandsQuery(
            gn="//src:bar",
            bazel=ConfiguredBazelLabel("//src:bar", "host"),
            action_type=ACTION_RUSTC,
        )

        with mock.patch.object(
            compare_utils, "query_ninja_and_bazel_commands"
        ) as mock_query:
            mock_query.return_value = ({}, {})
            results = compare_gn_and_bazel_commands_for(
                [query], self.bazel_paths
            )

        self.assertEqual(len(results), 1)
        res = results[0]
        self.assertEqual(
            res.error,
            "Failed to get GN or Bazel command for //src:bar vs //src:bar.",
        )
        self.assertEqual(res.gn_cmd_str, "")
        self.assertEqual(res.bazel_cmd_str, "")
        self.assertEqual(res.normalized_gn_args, [])
        self.assertEqual(res.normalized_bazel_args, [])

    def test_multiple_queries_mixed_results(self) -> None:
        q_success = CompareCommandsQuery(
            gn="//src:success",
            bazel=ConfiguredBazelLabel("//src:success", "host"),
            action_type=ACTION_RUSTC,
        )
        q_fail = CompareCommandsQuery(
            gn="//src:fail",
            bazel=ConfiguredBazelLabel("//src:fail", "host"),
            action_type=ACTION_RUSTC,
        )
        q_cpp = CompareCommandsQuery(
            gn="//src:cpp",
            bazel=ConfiguredBazelLabel("//src:cpp", "fuchsia_platform"),
            action_type=ACTION_CPP_COMPILE,
        )

        gn_cmds = {
            "//src:success": "rustc --crate-name=success ../../src/success.rs",
            "//src:cpp": "clang++ -c ../../src/cpp.cc -o obj/cpp.o",
        }
        bazel_cmds = {
            ConfiguredBazelLabel(
                "//src:success",
                "host",
            ): "rustc --crate-name=success src/success.rs",
            ConfiguredBazelLabel("//src:fail", "host"): "rustc src/fail.rs",
            ConfiguredBazelLabel(
                "//src:cpp",
                "fuchsia_platform",
            ): "clang++ -c src/cpp.cc -o bazel-out/cpp.o",
        }

        with mock.patch.object(
            compare_utils, "query_ninja_and_bazel_commands"
        ) as mock_query:
            mock_query.return_value = (gn_cmds, bazel_cmds)
            results = compare_gn_and_bazel_commands_for(
                [q_success, q_fail, q_cpp], self.bazel_paths
            )

        self.assertEqual(len(results), 3)
        self.assertEqual(results[0].error, "")
        self.assertEqual(results[0].query, q_success)
        self.assertIn("--crate-name=success", results[0].normalized_gn_args)

        self.assertTrue(results[1].error)
        self.assertEqual(results[1].query, q_fail)

        self.assertEqual(results[2].error, "")
        self.assertEqual(results[2].query, q_cpp)
        self.assertIn("clang++", results[2].normalized_gn_args)

    def test_read_response_files_forwarded(self) -> None:
        query = CompareCommandsQuery(
            gn="//src:foo",
            bazel=ConfiguredBazelLabel("//src:foo", "host"),
        )
        with mock.patch.object(
            compare_utils, "query_ninja_and_bazel_commands"
        ) as mock_query:
            mock_query.return_value = ({}, {})
            compare_gn_and_bazel_commands_for(
                [query], self.bazel_paths, read_response_files=True
            )

        mock_query.assert_called_once()
        _, kwargs = mock_query.call_args
        self.assertTrue(kwargs.get("read_response_files"))

        ninja_runner = mock_query.call_args[0][1]
        self.assertEqual(ninja_runner.ninja, self.bazel_paths.ninja_path)
        self.assertEqual(ninja_runner.build_dir, self.build_dir)

        bazel_launcher = mock_query.call_args[0][2]
        self.assertEqual(bazel_launcher.script, self.bazel_paths.launcher)

        execroot = mock_query.call_args[0][3]
        self.assertEqual(execroot, self.bazel_paths.execroot)


if __name__ == "__main__":
    unittest.main()

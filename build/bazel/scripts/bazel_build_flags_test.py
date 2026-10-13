#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import sys
import tempfile
import unittest
from pathlib import Path

_SCRIPT_DIR = Path(__file__).parent
sys.path.insert(0, str(_SCRIPT_DIR))

from bazel_build_flags import (
    DefaultBuildFlagsMap,
    DefaultBuildFlagsSet,
    get_build_flags_targets_from_ast,
)


class DefaultBuildFlagsTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self.fuchsia_dir = Path(self._td.name) / "fuchsia"
        self.fuchsia_dir.mkdir()
        self.build_dir = Path(self._td.name) / "build_dir"
        self.build_dir.mkdir()

        # Create the sub-directories for configs
        self.default_configs_dir = self.build_dir / "bazel_default_configs"
        self.default_configs_dir.mkdir()

    def tearDown(self) -> None:
        self._td.cleanup()

    def test_new_from_gn_config_filtering_and_resolution(self) -> None:
        # 1. Setup mock JSON files in build directory
        fuchsia_json = {
            "cxx_common": [
                "//build/config:compiler",
                "//build/config:unknown_config",
            ],
            "cxx_executable_extra": ["//build/config:executable_config"],
            "cxx_shared_library_extra": [
                "//build/config:shared_library_config"
            ],
            "rust_common": [
                "//build/config:rust_compiler",
                "//build/config:unknown_rust_config",
            ],
        }
        host_json = {
            "cxx_common": ["//build/config:compiler"],
            "cxx_executable_extra": [],
            "cxx_shared_library_extra": [],
            "rust_common": ["//build/config:rust_compiler"],
            "rust_executable_extra": [],
            "rust_shared_library_extra": [],
        }

        with (self.default_configs_dir / "fuchsia.json").open("w") as f:
            json.dump(fuchsia_json, f)
        with (self.default_configs_dir / "host.json").open("w") as f:
            json.dump(host_json, f)

        # 2. Setup mock BUILD.bazel files in fuchsia directory
        # We will define 'compiler' and 'executable_config', but NOT 'shared_library_config' or 'unknown_config'.
        build_config_dir = self.fuchsia_dir / "build" / "config"
        build_config_dir.mkdir(parents=True)

        build_bazel_content = """
compiler_build_flags(
    name = "compiler",
    cflags = ["-O2"],
)

build_flags(
    name = "rust_compiler",
    rustflags = ["-.."],
)

build_flags(
    name = "executable_config",
)
"""
        (build_config_dir / "BUILD.bazel").write_text(build_bazel_content)

        # 3. Invoke parsing
        build_flags_map = DefaultBuildFlagsMap.new_from_gn_config(
            self.build_dir, self.fuchsia_dir
        )

        # 4. Verify results
        self.assertIn("fuchsia", build_flags_map)
        self.assertIn("host", build_flags_map)

        fuchsia_set = build_flags_map["fuchsia"]
        # 'compiler' exists, 'unknown_config' is filtered out
        self.assertEqual(
            fuchsia_set.cxx_common_build_flags, ["//build/config:compiler"]
        )
        # 'executable_config' exists
        self.assertEqual(
            fuchsia_set.cxx_executable_build_flags,
            ["//build/config:executable_config"],
        )
        # 'shared_library_config' does not exist in BUILD.bazel
        self.assertEqual(fuchsia_set.cxx_shared_library_build_flags, [])

        # 'rust_compiler' exists, 'unknown_rust_config' is filtered out
        self.assertEqual(
            fuchsia_set.rust_common_build_flags,
            ["//build/config:rust_compiler"],
        )
        self.assertEqual(fuchsia_set.rust_shared_library_build_flags, [])
        self.assertEqual(fuchsia_set.rust_executable_build_flags, [])

        # Host verification
        host_set = build_flags_map["host"]
        self.assertEqual(
            host_set.cxx_common_build_flags, ["//build/config:compiler"]
        )

        self.assertEqual(host_set.cxx_executable_build_flags, [])
        self.assertEqual(host_set.cxx_shared_library_build_flags, [])
        self.assertEqual(
            host_set.rust_common_build_flags, ["//build/config:rust_compiler"]
        )
        self.assertEqual(host_set.rust_executable_build_flags, [])
        self.assertEqual(host_set.rust_shared_library_build_flags, [])
        self.assertEqual(host_set.target_compatible_with, "HOST_OS_CONSTRAINTS")
        # Check missing configs recorded
        expected_missing = [
            "//build/config:shared_library_config",
            "//build/config:unknown_config",
            "//build/config:unknown_rust_config",
        ]
        self.assertEqual(build_flags_map.missing_configs, expected_missing)

    def test_syntax_error_in_build_bazel_raises(self) -> None:
        fuchsia_json = {
            "cxx_common": ["//build/config:compiler"],
        }
        host_json: dict[str, list[str]] = {
            "cxx_common": [],
        }
        with (self.default_configs_dir / "fuchsia.json").open("w") as f:
            json.dump(fuchsia_json, f)
        with (self.default_configs_dir / "host.json").open("w") as f:
            json.dump(host_json, f)

        build_config_dir = self.fuchsia_dir / "build" / "config"
        build_config_dir.mkdir(parents=True)

        # Missing comma after cflags
        invalid_build_bazel = """
compiler_build_flags(
    name = "compiler",
    cflags = ["-O2"]
    visibility = ["//visibility:public"],
)
"""
        (build_config_dir / "BUILD.bazel").write_text(invalid_build_bazel)

        with self.assertRaises(SyntaxError):
            DefaultBuildFlagsMap.new_from_gn_config(
                self.build_dir, self.fuchsia_dir
            )

    def test_generate_bazel_toolchain_definitions(self) -> None:
        custom_map = DefaultBuildFlagsMap(
            {
                "target_a": DefaultBuildFlagsSet(
                    cxx_common_build_flags=[
                        "//build/flags:cxx_common1",
                        "//build/flags:cxx_common2",
                    ],
                    cxx_executable_build_flags=["//build/flags:cxx_exec1"],
                    cxx_shared_library_build_flags=["//build/flags:cxx_shlib1"],
                    rust_common_build_flags=["//build/flags:rust_common1"],
                    rust_executable_build_flags=["//build/flags:rust_exec1"],
                    rust_shared_library_build_flags=[
                        "//build/flags:rust_shlib1"
                    ],
                    target_compatible_with='["@platforms//os:target_a_os"]',
                ),
                "target_b": DefaultBuildFlagsSet(
                    target_compatible_with='["@platforms//os:target_b_os"]',
                ),
            }
        )
        custom_map.missing_configs = [
            "//build/flags:missing1",
            "//build/flags:missing2",
        ]

        generated_content = custom_map.generate_bazel_toolchain_definitions()

        # Check targets generated
        self.assertIn(
            """\
build_flags_toolchain_instance(
    name = "target_a_default_build_flags",
    cxx_common_build_flags = [
        "@@//build/flags:cxx_common1",
        "@@//build/flags:cxx_common2",
    ],
    cxx_executable_build_flags = [
        "@@//build/flags:cxx_exec1",
    ],
    cxx_shared_library_build_flags = [
        "@@//build/flags:cxx_shlib1",
    ],
    rust_common_build_flags = [
        "@@//build/flags:rust_common1",
    ],
    rust_executable_build_flags = [
        "@@//build/flags:rust_exec1",
    ],
    rust_shared_library_build_flags = [
        "@@//build/flags:rust_shlib1",
    ],
)""",
            generated_content,
        )
        self.assertIn('name = "target_a_toolchain"', generated_content)
        self.assertIn(
            'target_compatible_with = ["@platforms//os:target_a_os"]',
            generated_content,
        )
        self.assertIn(
            'toolchain = ":target_a_default_build_flags"', generated_content
        )

        self.assertIn(
            'name = "target_b_default_build_flags"', generated_content
        )
        self.assertIn("cxx_common_build_flags = []", generated_content)
        self.assertIn("cxx_executable_build_flags = []", generated_content)
        self.assertIn("cxx_shared_library_build_flags = []", generated_content)
        self.assertIn("rust_common_build_flags = []", generated_content)
        self.assertIn("rust_executable_build_flags = []", generated_content)
        self.assertIn("rust_shared_library_build_flags = []", generated_content)
        self.assertIn('name = "target_b_toolchain"', generated_content)
        self.assertIn(
            'target_compatible_with = ["@platforms//os:target_b_os"]',
            generated_content,
        )

        # Check comments for missing configs at the bottom
        self.assertIn("# - //build/flags:missing1", generated_content)
        self.assertIn("# - //build/flags:missing2", generated_content)

    def test_get_build_flags_targets_from_ast_custom_macros_and_comprehensions(
        self,
    ) -> None:
        build_bazel_content = """
riscv64_abi_build_flags(
    name = "abi",
    visibility = ["//visibility:public"],
)

riscv64_abi_build_flags(
    name = "no-vector",
    exclude_letters = ["v"],
)

[
    compiler_build_flags(
        name = v,
        c_family_flags = ["-march=arm{}-a".format(v)],
    )
    for v in ["v9", "v9.1", "v9.2"]
]

[
    custom_build_flags(name = t)
    for t in ("tuple_flag_a", "tuple_flag_b")
]

# Unrelated rules and unbound variables should be ignored.
cc_library(
    name = "ignored_library",
)

[
    cc_library(name = lib)
    for lib in ["ignored_comp_a", "ignored_comp_b"]
]

compiler_build_flags(
    name = unknown_var,
)
"""
        targets = get_build_flags_targets_from_ast(
            build_bazel_content, Path("BUILD.bazel")
        )
        self.assertEqual(
            targets,
            {
                "abi",
                "no-vector",
                "v9",
                "v9.1",
                "v9.2",
                "tuple_flag_a",
                "tuple_flag_b",
            },
        )


if __name__ == "__main__":
    unittest.main()

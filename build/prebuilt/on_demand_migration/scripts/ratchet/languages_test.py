#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for languages.py."""

import unittest
from collections.abc import Sequence

import languages


def _family_name(path: str) -> str | None:
    family = languages.match_language_family(path)
    return family.name if family else None


def _scan(path: str, content: str) -> Sequence[tuple[int, int]]:
    family = languages.match_language_family(path)
    return family.find_matches(content) if family else ()


class LanguagesTest(unittest.TestCase):
    """Tests language family path matching and comment-stripping scanners."""

    def test_gn_direct_prebuilt_rejected(self) -> None:
        matches = _scan(
            "src/foo/BUILD.gn",
            'action("bar") {\n  inputs = [ "//prebuilt/third_party/jq/linux-x64/bin/jq" ]\n}\n',
        )
        self.assertSequenceEqual(matches, ((2, 14),))

    def test_gn_multiple_direct_prebuilts_same_line_counted(self) -> None:
        matches = _scan(
            "src/foo/BUILD.gn",
            'inputs = [ "//prebuilt/third_party/foo", "//prebuilt/third_party/bar" ]\n',
        )
        self.assertSequenceEqual(matches, ((1, 12), (1, 42)))

    def test_gn_cipd_prebuilt_allowed(self) -> None:
        matches = _scan(
            "src/foo/BUILD.gn",
            (
                'cipd_prebuilt("jq") {\n'
                '  paths = [ "//prebuilt/third_party/jq/${host_platform}/bin/jq" ]\n'
                "}\n"
                'action("use_jq") {\n'
                '  inputs = [ "${cipd_prebuilt_root}/prebuilt/third_party/jq/${host_platform}/bin/jq" ]\n'
                "}\n"
            ),
        )
        self.assertSequenceEqual(matches, ())

    def test_gn_multiline_cipd_prebuilt_header_allowed(self) -> None:
        matches = _scan(
            "src/foo/BUILD.gn",
            (
                "cipd_prebuilt(\n"
                '    "very_long_target_name_wrapped_by_gn_format") {\n'
                '  paths = [ "//prebuilt/third_party/jq/${host_platform}/bin/jq" ]\n'
                "}\n"
            ),
        )
        self.assertSequenceEqual(matches, ())

    def test_bazel_cipd_prebuilt_file_and_directory_allowed(self) -> None:
        matches = _scan(
            "src/foo/BUILD.bazel",
            (
                "cipd_prebuilt_file(\n"
                '    name = "jq",\n'
                '    path = "//prebuilt/third_party/jq/linux-x64/bin/jq",\n'
                ")\n"
                "cipd_prebuilt_directory(\n"
                '    name = "firmwares",\n'
                '    path = "//prebuilt/third_party/firmwares",\n'
                ")\n"
                "filegroup(\n"
                '    name = "legacy",\n'
                '    srcs = ["//:prebuilt/third_party/jq/linux-x64/bin/jq"],\n'
                ")\n"
            ),
        )
        self.assertSequenceEqual(matches, ((11, 13),))

    def test_devshell_headers_allowed_direct_usage_rejected(self) -> None:
        matches = _scan(
            "tools/devshell/my_tool",
            (
                "#!/bin/bash\n"
                "#### EXECUTABLE=${FUCHSIA_DIR}/prebuilt/tools/foo/foo\n"
                "#### PREBUILTS=${PREBUILT_JQ}\n"
                'exec "${FUCHSIA_DIR}/prebuilt/tools/unmigrated/bar" "$@"\n'
            ),
        )
        self.assertSequenceEqual(matches, ((4, 21),))

    def test_devshell_non_script_files_classified_accurately(self) -> None:
        self.assertEqual(_family_name("tools/devshell/build"), "devshell")
        self.assertEqual(_family_name("tools/devshell/cmc.fx"), "devshell")
        self.assertEqual(
            _family_name("scripts/devshell/lib/vars.sh"), "devshell"
        )
        self.assertEqual(_family_name("tools/devshell/BUILD.gn"), "gn")
        self.assertEqual(
            _family_name("tools/devshell/contrib/min_owners.py"),
            "hash_comment",
        )
        self.assertIsNone(_family_name("tools/devshell/README.md"))
        self.assertIsNone(_family_name("tools/devshell/OWNERS"))
        self.assertIsNone(_family_name("tools/devshell/tests/data/blobs.json"))

        # `tools/devshell/BUILD.gn` must be parsed using GN rules, not devshell rules.
        gn_matches = _scan(
            "tools/devshell/BUILD.gn",
            (
                'cipd_prebuilt("jq") {\n'
                '  paths = [ "//prebuilt/third_party/jq/${host_platform}/bin/jq" ]\n'
                "}\n"
            ),
        )
        self.assertSequenceEqual(gn_matches, ())

    def test_match_language_family_excludes_allowlisted_trees(self) -> None:
        self.assertIsNone(
            languages.match_language_family(
                "build/bazel/rules/cipd_prebuilt.bzl"
            )
        )
        self.assertIsNone(
            languages.match_language_family("scripts/shac/prebuilts.star")
        )
        self.assertIsNone(
            languages.match_language_family("build/prebuilt/cipd_prebuilt.gni")
        )
        self.assertIsNone(
            languages.match_language_family("third_party/foo/BUILD.gn")
        )
        self.assertIsNone(
            languages.match_language_family("src/foo/testdata/fixture.cc")
        )
        self.assertIsNone(languages.match_language_family("src/foo/README"))
        self.assertIsNotNone(
            languages.match_language_family("src/foo/BUILD.gn")
        )
        self.assertIsNotNone(languages.match_language_family("BUILD.bazel"))
        self.assertIsNotNone(
            languages.match_language_family("src/foo/CMakeLists.txt")
        )
        self.assertIsNotNone(
            languages.match_language_family("src/foo/driver.cc")
        )
        self.assertIsNotNone(languages.match_language_family("src/foo/lib.rs"))
        self.assertIsNotNone(
            languages.match_language_family("tools/foo/main.go")
        )
        self.assertIsNotNone(
            languages.match_language_family("scripts/helper.sh")
        )

    def test_python_and_cmake_direct_prebuilts_ignore_comments(self) -> None:
        py_matches = _scan(
            "scripts/check_tool.py",
            (
                '# Comment mentioning "//prebuilt/third_party/ignored"\n'
                'TOOL_PATH = "//prebuilt/third_party/tool"  # inline comment\n'
                'REL_PATH = "../../prebuilt/tools/bin"\n'
            ),
        )
        self.assertSequenceEqual(py_matches, ((2, 13), (3, 12)))

        cmake_matches = _scan(
            "src/foo/CMakeLists.txt",
            (
                "# Comment with prebuilt/ignored\n"
                'set(PREBUILT_TOOL "${FUCHSIA_DIR}/prebuilt/third_party/clang")\n'
            ),
        )
        self.assertSequenceEqual(cmake_matches, ((2, 20),))

    def test_shell_direct_prebuilts_ignore_comments_and_allowed_prefixes(
        self,
    ) -> None:
        matches = _scan(
            "scripts/run_tool.sh",
            (
                "#!/bin/bash\n"
                "# Comment referencing prebuilt/third_party/ignored\n"
                'ALLOWED="${cipd_prebuilt_root}/prebuilt/third_party/jq/bin/jq"\n'
                "fx build  # inline comment mentioning prebuilt/third_party/ignored\n"
                "UNMIGRATED=prebuilt/tools/shac/shac\n"
            ),
        )
        self.assertSequenceEqual(matches, ((5, 12),))

    def test_c_family_direct_prebuilts_and_comment_stripping(self) -> None:
        cpp_matches = _scan(
            "src/ui/input/drivers/focaltech/ft_firmware.cc",
            (
                '// Single-line comment with "prebuilt/touch/ignored.h"\n'
                "/* Multi-line block comment\n"
                ' * mentioning "prebuilt/touch/also_ignored.h"\n'
                " */\n"
                '#include "prebuilt/touch/focaltech/ft_firmware.h"\n'
                'constexpr char kPrefixOnly[] = "prebuilt/";\n'
            ),
        )
        self.assertSequenceEqual(cpp_matches, ((5, 10),))

        rust_matches = _scan(
            "src/graphics/lib/carnelian/src/font.rs",
            (
                "fn first_lifetime<'a>(x: &'a str) {}\n"
                '// Comment between Rust lifetimes mentioning "prebuilt/ignored"\n'
                "fn second_lifetime<'b>(y: &'b str) {}\n"
                "const FONT: &[u8] =\n"
                '    include_bytes!("../../../../../prebuilt/third_party/fontdata/font.ttf");\n'
            ),
        )
        self.assertSequenceEqual(rust_matches, ((5, 20),))

        go_matches = _scan(
            "tools/shac/main.go",
            ("package main\n" 'const shacBin = "prebuilt/tools/shac/shac"\n'),
        )
        self.assertSequenceEqual(go_matches, ((2, 17),))


if __name__ == "__main__":
    unittest.main()

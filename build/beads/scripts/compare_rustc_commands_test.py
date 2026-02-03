# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest

import compare_rustc_commands


class TestCompareRustcCommands(unittest.TestCase):
    def test_normalize_rustc_arg(self):
        # Basic args
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("params.rs"), "params.rs"
        )

        # Flag conversions
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("--codegen=foo=bar"),
            "-Cfoo=bar",
        )
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("--allow=dead_code"),
            "-Adead_code",
        )
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("--deny=warnings"),
            "-Dwarnings",
        )
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("--warn=unused_imports"),
            "-Wunused_imports",
        )

        # Ignored args
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("--extern"), ""
        )
        self.assertEqual(compare_rustc_commands.normalize_rustc_arg("-L"), "")
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("-Ldependency"), ""
        )
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("@shell:foo"), ""
        )
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("--emit=dep-info"), ""
        )
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg(
                "-Zdep-info-omit-d-target"
            ),
            "",
        )
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("--error-format=human"),
            "",
        )
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("-Cdebug-assertions=y"),
            "",
        )
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("-Cdebuginfo=2"), ""
        )
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("-Cembed-bitcode=no"), ""
        )
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("-Ccodegen-units=16"), ""
        )
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("-Cstrip=debuginfo"), ""
        )
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("-Copt-level=3"), ""
        )
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("--codegen=opt-level=3"),
            "",
        )
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg(
                "--cfg=__rust_toolchain=stable"
            ),
            "",
        )
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("-Cmetadata=123"), ""
        )
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("RUST_BACKTRACE=1"), ""
        )
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("-Clink-arg=-s"), ""
        )

        # Linker args normalization
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg(
                "-Clinker=/path/to/clang"
            ),
            "-Clinker=clang",
        )
        self.assertEqual(
            compare_rustc_commands.normalize_rustc_arg("-Clinker=lld"),
            "-Clinker=lld",
        )

    def test_rindex(self):
        l = ["a", "b", "c", "b", "d"]
        self.assertEqual(compare_rustc_commands.rindex(l, "b"), 3)
        self.assertEqual(compare_rustc_commands.rindex(l, "a"), 0)
        self.assertEqual(compare_rustc_commands.rindex(l, "d"), 4)

        with self.assertRaisesRegex(ValueError, "Value z not found in list"):
            compare_rustc_commands.rindex(l, "z")


if __name__ == "__main__":
    unittest.main()

# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest

import normalize_rustc_args


class TestNormalizeRustcArgs(unittest.TestCase):
    def test_normalize_rustc_arg(self) -> None:
        # Basic args
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("params.rs"), "params.rs"
        )

        # Flag conversions
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("--codegen=foo=bar"),
            "-Cfoo=bar",
        )
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("--allow=dead_code"),
            "-Adead_code",
        )
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("--deny=warnings"),
            "-Dwarnings",
        )
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("--warn=unused_imports"),
            "-Wunused_imports",
        )

        # Ignored args
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("--extern"), ""
        )
        self.assertEqual(normalize_rustc_args.normalize_rustc_arg("-L"), "")
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("-Ldependency"), ""
        )
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("@shell:foo"), ""
        )
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("--emit=dep-info"), ""
        )
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg(
                "-Zdep-info-omit-d-target"
            ),
            "",
        )
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("--error-format=human"),
            "",
        )
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("-Cdebug-assertions=y"),
            "",
        )
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("-Cdebuginfo=2"), ""
        )
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("-Cembed-bitcode=no"), ""
        )
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("-Ccodegen-units=16"), ""
        )
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("-Cstrip=debuginfo"), ""
        )
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("-Copt-level=3"), ""
        )
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("--codegen=opt-level=3"),
            "",
        )
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg(
                "--cfg=__rust_toolchain=stable"
            ),
            "",
        )
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("-Cmetadata=123"), ""
        )
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("RUST_BACKTRACE=1"), ""
        )
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("-Clink-arg=-s"), ""
        )

        # Linker args normalization
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("-Clinker=/path/to/clang"),
            "",
        )
        self.assertEqual(
            normalize_rustc_args.normalize_rustc_arg("-Clinker=lld"),
            "",
        )


if __name__ == "__main__":
    unittest.main()

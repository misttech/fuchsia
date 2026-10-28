# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import pathlib
import tempfile
import unittest

from build.beads.scripts import normalize_rustc_args, path_normalizer


class MockPathNormalizer(path_normalizer.PathNormalizer):
    def normalize_path(self, path: str) -> str:
        return path


class TestNormalizeRustcArgs(unittest.TestCase):
    def test_normalize_rustc_cmd(self) -> None:
        mock_normalizer = MockPathNormalizer()

        BASIC_TEST_CASES = [
            # Basic args
            ("params.rs", ["params.rs"]),
            # Flag conversions
            ("--codegen=foo=bar", ["-Cfoo=bar"]),
            ("-Cfoo=bar=baz", ["-Cfoo=bar=baz"]),
            ("-C foo=bar=baz", ["-Cfoo=bar=baz"]),
            ("-Cfoo-bar -C foo=zoo", ["-Cfoo-bar", "-Cfoo=zoo"]),
            (
                "rustc -obinary --target fuchsia-x64 -C foo=bar",
                ["--target=fuchsia-x64", "-Cfoo=bar", "rustc"],
            ),
        ]
        for arg, expected in BASIC_TEST_CASES:
            self.assertListEqual(
                normalize_rustc_args.normalize_rustc_cmd(arg, mock_normalizer),
                expected,
                msg=f"For input '{arg}'",
            )

    def test_normalize_rustc_arg(self) -> None:
        mock_normalizer = MockPathNormalizer()

        BASIC_TEST_CASES = [
            # Basic args
            ("params.rs", "params.rs"),
            # Flag conversions
            ("--codegen=foo=bar", "-Cfoo=bar"),
            ("--allow=dead_code", "-Adead_code"),
            ("--deny=warnings", "-Dwarnings"),
            ("--warn=unused_imports", "-Wunused_imports"),
            # Ignored args
            ("--extern", ""),
            ("-L", ""),
            ("-Ldependency", ""),
            ("@shell:foo", ""),
            ("--emit=dep-info", ""),
            ("-Zdep-info-omit-d-target", ""),
            ("--error-format=human", ""),
            ("--remap-path-prefix=${pwd}=.", ""),
            ("--remap-path-prefix=${exec_root}=.", ""),
            ("--remap-path-prefix=${output_base}=.", ""),
            ("-Cdebug-assertions=y", ""),
            ("-Cdebuginfo=2", ""),
            ("-Cembed-bitcode=no", ""),
            ("-Ccodegen-units=16", ""),
            ("-Cstrip=debuginfo", ""),
            ("-Copt-level=3", ""),
            ("--codegen=opt-level=3", ""),
            ("--cfg=__rust_toolchain=stable", ""),
            ("-Cmetadata=123", ""),
            ("RUST_BACKTRACE=1", ""),
            ("-Clink-arg=-s", ""),
            # Linker args normalization
            ("-Clinker=/path/to/clang", ""),
            ("-Clinker=lld", ""),
        ]
        for arg, expected in BASIC_TEST_CASES:
            self.assertEqual(
                normalize_rustc_args.normalize_rustc_arg(arg, mock_normalizer),
                expected,
                msg=f"For input '{arg}'",
            )

    def test_prefix_remapping_with_normalizer(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            fuchsia_dir = pathlib.Path(tmp) / "fuchsia"
            build_dir = fuchsia_dir / "out" / "default"
            fuchsia_dir.mkdir(parents=True)
            build_dir.mkdir(parents=True)

            gn_norm = path_normalizer.GnPathNormalizer(fuchsia_dir, build_dir)

            self.assertEqual(
                normalize_rustc_args.normalize_rustc_arg(
                    "--remap-path-prefix=.=../..", normalizer=gn_norm
                ),
                "--remap-path-prefix={BUILD_DIR}={SOURCE_ROOT}",
            )
            self.assertEqual(
                normalize_rustc_args.normalize_rustc_arg(
                    f"--remap-path-prefix={fuchsia_dir}=../..",
                    normalizer=gn_norm,
                ),
                "--remap-path-prefix={SOURCE_ROOT}={SOURCE_ROOT}",
            )


if __name__ == "__main__":
    unittest.main()

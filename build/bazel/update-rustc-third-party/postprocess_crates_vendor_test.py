#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for postprocess_crates_vendor.py."""

import os
import pathlib
import sys
import tempfile
import textwrap
import unittest

sys.path.insert(0, os.path.dirname(__file__))

from postprocess_crates_vendor import (
    _apply_alias_visibility,
    check_package_groups_exist,
    get_restricted_crate_packages,
    get_rust_package_paths,
    get_rust_package_visibilities,
    process_one,
    restrict_crate_package,
)


class PostprocessCratesVendorTest(unittest.TestCase):
    def test_get_rust_package_paths_and_visibilities(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            fuchsia_dir = pathlib.Path(tmp)
            cargo_dir = fuchsia_dir / "third_party/rust_crates"
            cargo_dir.mkdir(parents=True)
            (cargo_dir / "Cargo.toml").write_text(
                textwrap.dedent(
                    """\
                    [patch.crates-io]
                    zeroize = { path = "forks/zeroize" }

                    [gn.package.regex."1.13.1"]
                    group_visibility = { import = "//third_party/rust_crates/visibility.gni", variable = "regex_visibility" }

                    [gn.package.nix."0.31.3"]
                    group_visibility = { import = "//src/lib/fuchsia_nix/nix_visibility.gni", variable = "nix_visibility" }
                    """
                )
            )

            self.assertEqual(
                get_rust_package_paths(fuchsia_dir),
                {"zeroize": "third_party/rust_crates/forks/zeroize"},
            )
            self.assertEqual(
                get_rust_package_visibilities(fuchsia_dir),
                {
                    (
                        "regex",
                        "1.13.1",
                    ): "//third_party/rust_crates:regex_visibility",
                },
            )

    def test_process_one_fixes_paths_removes_missing_and_sets_visibility(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            fuchsia_dir = pathlib.Path(tmp)
            (fuchsia_dir / "third_party/rust_crates/vendor/regex-1.13.1").mkdir(
                parents=True
            )
            (fuchsia_dir / "third_party/rust_crates/vendor/sha1-0.10.7").mkdir(
                parents=True
            )
            (fuchsia_dir / "third_party/rust_crates/vendor/sha1-0.11.0").mkdir(
                parents=True
            )
            (fuchsia_dir / "third_party/rust_crates/forks/zeroize").mkdir(
                parents=True
            )

            build_file = (
                fuchsia_dir / "third_party/rust_crates/vendor/BUILD.bazel"
            )
            build_file.write_text(
                textwrap.dedent(
                    """\
                    alias(
                        name = "missing-1.0.0",
                        actual = "//third_party/rust_crates/vendor/missing-1.0.0:missing",
                        tags = ["manual"],
                    )

                    alias(
                        name = "regex-1.13.1",
                        actual = "//third_party/rust_crates/vendor/regex-1.13.1:regex",
                        tags = ["manual"],
                    )

                    alias(
                        name = "regex",
                        actual = "//third_party/rust_crates/vendor/regex-1.13.1:regex",
                        tags = ["manual"],
                    )

                    alias(
                        name = "sha1-0.10.7",
                        actual = "//third_party/rust_crates/vendor/sha1-0.10.7:sha1",
                        tags = ["manual"],
                    )

                    alias(
                        name = "sha1-0.11.0",
                        actual = "//third_party/rust_crates/vendor/sha1-0.11.0:sha1",
                        tags = ["manual"],
                    )

                    alias(
                        name = "sha1",
                        actual = "//third_party/rust_crates/vendor/sha1-0.11.0:sha1",
                        tags = ["manual"],
                    )

                    alias(
                        name = "zeroize-1.9.0",
                        actual = "//third_party/rust_crates/vendor/zeroize-1.9.0:zeroize",
                        tags = ["manual"],
                    )

                    alias(
                        name = "zeroize",
                        actual = "//third_party/rust_crates/vendor/zeroize-1.9.0:zeroize",
                        tags = ["manual"],
                    )
                    """
                )
            )

            path_overrides = {
                "zeroize": "third_party/rust_crates/forks/zeroize",
            }
            visibilities = {
                (
                    "regex",
                    "1.13.1",
                ): "//third_party/rust_crates:regex_visibility",
                (
                    "sha1",
                    "0.11.0",
                ): "//third_party/rust_crates:sha1_visibility",
                (
                    "zeroize",
                    "1.9.0",
                ): "//third_party/rust_crates:zeroize_visibility",
            }

            process_one(build_file, fuchsia_dir, path_overrides, visibilities)
            expected = textwrap.dedent(
                """\
                alias(
                    name = "regex-1.13.1",
                    actual = "//third_party/rust_crates/vendor/regex-1.13.1:regex",
                    tags = ["manual"],
                    visibility = ["//third_party/rust_crates:regex_visibility"],
                )

                alias(
                    name = "regex",
                    actual = "//third_party/rust_crates/vendor/regex-1.13.1:regex",
                    tags = ["manual"],
                    visibility = ["//third_party/rust_crates:regex_visibility"],
                )

                alias(
                    name = "sha1-0.10.7",
                    actual = "//third_party/rust_crates/vendor/sha1-0.10.7:sha1",
                    tags = ["manual"],
                )

                alias(
                    name = "sha1-0.11.0",
                    actual = "//third_party/rust_crates/vendor/sha1-0.11.0:sha1",
                    tags = ["manual"],
                    visibility = ["//third_party/rust_crates:sha1_visibility"],
                )

                alias(
                    name = "sha1",
                    actual = "//third_party/rust_crates/vendor/sha1-0.11.0:sha1",
                    tags = ["manual"],
                    visibility = ["//third_party/rust_crates:sha1_visibility"],
                )

                alias(
                    name = "zeroize-1.9.0",
                    actual = "//third_party/rust_crates/forks/zeroize:zeroize",
                    tags = ["manual"],
                    visibility = ["//third_party/rust_crates:zeroize_visibility"],
                )

                alias(
                    name = "zeroize",
                    actual = "//third_party/rust_crates/forks/zeroize:zeroize",
                    tags = ["manual"],
                    visibility = ["//third_party/rust_crates:zeroize_visibility"],
                )
                """
            )
            self.assertEqual(build_file.read_text(), expected)

            # Re-running process_one on the already-processed file is idempotent.
            process_one(build_file, fuchsia_dir, path_overrides, visibilities)
            self.assertEqual(build_file.read_text(), expected)

            # Updating a visibility label in Cargo.toml updates the existing
            # visibility attribute in place on both vendored and forked aliases.
            updated_visibilities = {
                **visibilities,
                (
                    "zeroize",
                    "1.9.0",
                ): "//third_party/rust_crates:custom_zeroize_visibility",
            }
            process_one(
                build_file, fuchsia_dir, path_overrides, updated_visibilities
            )
            self.assertEqual(
                build_file.read_text(),
                expected.replace(
                    "//third_party/rust_crates:zeroize_visibility",
                    "//third_party/rust_crates:custom_zeroize_visibility",
                ),
            )

    def test_get_restricted_crate_packages(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            fuchsia_dir = pathlib.Path(tmp)
            (
                fuchsia_dir
                / "third_party/rust_crates/vendor/ash-0.38.0-1.3.281"
            ).mkdir(parents=True)
            (fuchsia_dir / "third_party/rust_crates/forks/zeroize").mkdir(
                parents=True
            )
            path_overrides = {
                "zeroize": "third_party/rust_crates/forks/zeroize",
            }

            self.assertEqual(
                get_restricted_crate_packages(
                    fuchsia_dir,
                    path_overrides,
                    {
                        ("ash", "0.38.0+1.3.281"): "//:ash_visibility",
                        ("zeroize", "1.9.0"): "//:zeroize_visibility",
                    },
                ),
                {
                    "third_party/rust_crates/vendor/ash-0.38.0-1.3.281": "//:ash_visibility",
                    "third_party/rust_crates/forks/zeroize": "//:zeroize_visibility",
                },
            )

            # A stale version must not silently leave the crate public.
            with self.assertRaisesRegex(
                ValueError, "failed to find the package for restricted crate"
            ):
                get_restricted_crate_packages(
                    fuchsia_dir,
                    path_overrides,
                    {("regex", "1.12.0"): "//:regex_visibility"},
                )

    def test_apply_alias_visibility_fails_on_unexpected_format(self) -> None:
        with self.assertRaisesRegex(ValueError, "failed to set visibility"):
            _apply_alias_visibility(
                'alias(name = "regex", actual = ":regex")',
                "//third_party/rust_crates:regex_visibility",
            )

    def test_check_package_groups_exist(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            fuchsia_dir = pathlib.Path(tmp)
            build_file = fuchsia_dir / "third_party/rust_crates/BUILD.bazel"
            build_file.parent.mkdir(parents=True)
            build_file.write_text(
                'package_group(\n    name = "regex_visibility",\n)\n'
            )
            check_package_groups_exist(
                fuchsia_dir,
                {
                    (
                        "regex",
                        "1.13.1",
                    ): "//third_party/rust_crates:regex_visibility"
                },
            )
            with self.assertRaisesRegex(ValueError, "md5_visibility"):
                check_package_groups_exist(
                    fuchsia_dir,
                    {
                        (
                            "md-5",
                            "0.11.0",
                        ): "//third_party/rust_crates:md5_visibility"
                    },
                )

    def test_restrict_crate_package(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            build_file = pathlib.Path(tmp) / "BUILD.bazel"
            build_file.write_text(
                textwrap.dedent(
                    """\
                    package(
                        default_visibility = ["//visibility:public"],
                    )
                    """
                )
            )
            restrict_crate_package(build_file)
            want = textwrap.dedent(
                """\
                package(
                    default_visibility = ["//third_party/rust_crates:__subpackages__"],
                )
                """
            )
            self.assertEqual(build_file.read_text(), want)

            # Rerunning is a no-op.
            restrict_crate_package(build_file)
            self.assertEqual(build_file.read_text(), want)

            build_file.write_text("package()\n")
            with self.assertRaisesRegex(ValueError, "failed to restrict"):
                restrict_crate_package(build_file)


if __name__ == "__main__":
    unittest.main()

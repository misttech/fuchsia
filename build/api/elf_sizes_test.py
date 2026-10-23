#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import tempfile
import typing as T
import unittest
from pathlib import Path

import elf_sizes


def _write_json(path: Path, content: T.Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("wt") as f:
        json.dump(content, f)


class ElfSizesTest(unittest.TestCase):
    def setUp(self) -> None:
        self._temp_dir = tempfile.TemporaryDirectory()
        self._top_dir = Path(self._temp_dir.name)
        self._build_dir = self._top_dir / "out" / "default"
        self._build_dir.mkdir(parents=True)
        self._host_tag = "linux-x64"

    def tearDown(self) -> None:
        self._temp_dir.cleanup()

    def test_extract_elf_sizes_paths(self) -> None:
        elf_sizes_rel = "obj/products/core/elf_sizes.json"
        missing_rel = "obj/products/missing/elf_sizes.json"
        existing_path = self._build_dir / elf_sizes_rel
        _write_json(existing_path, {"files": []})

        images_json = [
            {"name": "blk", "path": "obj/build/images/fvm.blk"},
            {"name": "elf_sizes.json", "path": elf_sizes_rel},
            {"name": "elf_sizes.json", "path": missing_rel},
        ]

        # When last_build_only is False, missing files are ignored.
        self.assertEqual(
            elf_sizes.extract_elf_sizes_paths(
                images_json, self._build_dir, last_build_only=False
            ),
            [existing_path],
        )

        # When last_build_only is True, a missing file triggers an AssertionError.
        with self.assertRaises(AssertionError):
            elf_sizes.extract_elf_sizes_paths(
                images_json, self._build_dir, last_build_only=True
            )

        self.assertEqual(
            elf_sizes.extract_elf_sizes_paths(
                [{"name": "elf_sizes.json", "path": elf_sizes_rel}],
                self._build_dir,
                last_build_only=True,
            ),
            [existing_path],
        )

    def test_collect_elf_sizes_verification_inputs_all_matched(self) -> None:
        elf_sizes_path = self._build_dir / "obj/products/core/elf_sizes.json"
        _write_json(
            elf_sizes_path,
            {
                "files": [
                    {
                        "build_id": "00000000000000001",
                        "path": "bazel-out/fuchsia/blobs/merkle_libfoo",
                    }
                ]
            },
        )
        debug_symbol_entries = [
            {
                "elf_build_id": "00000000000000001",
                "debug": "obj/src/foo/libfoo.so.unstripped",
                "os": "fuchsia",
            }
        ]
        (
            prebuilt_merkles,
            exempt_build_ids,
        ) = elf_sizes.collect_elf_sizes_verification_inputs(
            self._build_dir,
            self._top_dir,
            self._host_tag,
            [elf_sizes_path],
            debug_symbol_entries,
        )
        self.assertEqual(prebuilt_merkles, set())
        self.assertEqual(exempt_build_ids, set())

    def test_collect_elf_sizes_verification_inputs(self) -> None:
        (self._top_dir / ".jiri_manifest").write_text("")
        (self._top_dir / "build/bazel/config").mkdir(
            parents=True, exist_ok=True
        )
        (self._top_dir / "build/bazel/config/bazel_top_dir").write_text(
            "gen/build/bazel\n"
        )

        pkg_manifest_rel = "obj/src/media/codec/package_manifest.json"
        _write_json(
            self._build_dir / pkg_manifest_rel,
            {
                "blobs": [
                    {
                        "merkle": "merkle_gn_bucket2",
                        "path": "lib/libffmpeg.so",
                        "source_path": str(
                            Path("../..")
                            / "prebuilt"
                            / "third_party/ffmpeg/libffmpeg.so"
                        ),
                    }
                ]
            },
        )

        # A GN source-built package imported into Bazel via @gn_targets also gets a
        # rebased_package_manifest.json under bazel-out, but its merkle is recorded in
        # gn_source_merkles from its GN package_manifest.json so it must never be treated
        # as a prebuilt exemption.
        gn_src_pm_rel = "obj/src/pkg/package_manifest.json"
        _write_json(
            self._build_dir / gn_src_pm_rel,
            {
                "blobs": [
                    {
                        "merkle": "merkle_missing_source",
                        "path": "bin/source_elf",
                        "source_path": "source_elf",
                    }
                ]
            },
        )

        _write_json(
            self._build_dir / "ninja_outputs.json",
            {
                "//src/media/codec:pkg": [pkg_manifest_rel],
                "//src/pkg:pkg": [gn_src_pm_rel],
            },
        )

        bazel_execroot = (
            self._build_dir / "gen/build/bazel/output_base/execroot/_main"
        )
        rebased_pm = (
            bazel_execroot
            / "bazel-out/aarch64-opt/bin/vendor/pkg/rebased_package_manifest.json"
        )
        _write_json(
            rebased_pm,
            {
                "blobs": [
                    {
                        "merkle": "merkle_bazel_bucket2",
                        "path": "bin/vendor_bin",
                        "source_path": "content/blobs/merkle_bazel_bucket2",
                    },
                    {
                        "merkle": "merkle_bazel_product_config_bucket2",
                        "path": "bin/product_config_vendor_bin",
                        "source_path": "content/blobs/merkle_bazel_product_config_bucket2",
                    },
                ]
            },
        )

        # Prebuilt assembly container (e.g. fuchsia_prebuilt_product_configuration)
        # recorded via container_dir in a Bazel *.debug_symbols.json manifest.
        prebuilt_container = (
            bazel_execroot / "vendor/product/prebuilt_product_config"
        )
        (prebuilt_container / "blobs").mkdir(parents=True, exist_ok=True)
        (
            prebuilt_container / "blobs/merkle_prebuilt_container_blob"
        ).write_bytes(b"")
        (prebuilt_container / "blobs/unexpected_subdir").mkdir(
            parents=True, exist_ok=True
        )
        _write_json(
            prebuilt_container / "packages/starnix_container",
            {
                "blobs": [
                    {
                        "merkle": "merkle_prebuilt_container_pkg",
                        "path": "system/bin/init",
                        "source_path": "../blobs/merkle_prebuilt_container_pkg",
                    }
                ]
            },
        )
        (prebuilt_container / "packages/README.txt").write_text("not json")
        (prebuilt_container / "subpackages").mkdir(parents=True, exist_ok=True)
        (prebuilt_container / "subpackages/binary.bin").write_bytes(b"\xff\xfe")
        _write_json(
            bazel_execroot
            / "bazel-out/aarch64-opt/bin/vendor/product/elf_sizes.debug_symbols.json",
            [
                {
                    "container_dir": "vendor/product/prebuilt_product_config",
                    "cpu": "arm64",
                    "label": "@@//vendor/product:prebuilt_product_config",
                    "os": "fuchsia",
                }
            ],
        )

        _write_json(
            bazel_execroot
            / "bazel-out/aarch64-opt/bin/src/pkg/pkg_expanded/rebased_package_manifest.json",
            {
                "blobs": [
                    {
                        "merkle": "merkle_missing_source",
                        "path": "bin/source_elf",
                        "source_path": "content/blobs/merkle_missing_source",
                    }
                ]
            },
        )

        toolchain_debug = (
            self._top_dir
            / "prebuilt"
            / "third_party"
            / "clang"
            / self._host_tag
            / "lib/debug/.build-id/aa/1111111111.debug"
        )
        toolchain_debug.parent.mkdir(parents=True, exist_ok=True)
        toolchain_debug.write_bytes(b"")

        elf_sizes_path = self._build_dir / "obj/products/core/elf_sizes.json"
        _write_json(
            elf_sizes_path,
            {
                "files": [
                    {
                        "build_id": "00000000000000001",
                        "path": "bazel-out/fuchsia/blobs/merkle_libfoo",
                    },
                    {
                        "build_id": "bb2222222222",
                        "path": "bazel-out/fuchsia/blobs/merkle_gn_bucket2",
                    },
                    {
                        "build_id": "cc3333333333",
                        "path": "bazel-out/aarch64-opt/bin/vendor/pkg/content/blobs/merkle_bazel_bucket2",
                    },
                    {
                        "build_id": "dd4444444444",
                        "path": "bazel-out/fuchsia_sdk_arm64-opt/bin/vendor/product/smart_product_config/blobs/merkle_bazel_product_config_bucket2",
                    },
                    {
                        "build_id": "ee5555555555",
                        "path": "bazel-out/fuchsia_sdk_arm64-opt/bin/vendor/product/hybrid_product_config/blobs/merkle_prebuilt_container_blob",
                    },
                    {
                        "build_id": "ff6666666666",
                        "path": "bazel-out/fuchsia_sdk_arm64-opt/bin/vendor/product/hybrid_product_config/blobs/merkle_prebuilt_container_pkg",
                    },
                    {
                        "build_id": "aa1111111111",
                        "path": "bazel-out/fuchsia/blobs/merkle_toolchain_libcxx",
                    },
                    {
                        "build_id": "deadbeef99999999",
                        "path": "bazel-out/fuchsia/blobs/merkle_missing_source",
                    },
                ]
            },
        )

        debug_symbol_entries = [
            {
                "elf_build_id": "00000000000000001",
                "debug": "obj/src/foo/lib_shared/libfoo.so.unstripped",
                "os": "fuchsia",
            }
        ]

        (
            prebuilt_merkles,
            exempt_build_ids,
        ) = elf_sizes.collect_elf_sizes_verification_inputs(
            self._build_dir,
            self._top_dir,
            self._host_tag,
            [elf_sizes_path],
            debug_symbol_entries,
        )

        self.assertEqual(
            prebuilt_merkles,
            {
                "merkle_gn_bucket2",
                "merkle_bazel_bucket2",
                "merkle_bazel_product_config_bucket2",
                "merkle_prebuilt_container_blob",
                "merkle_prebuilt_container_pkg",
            },
        )
        self.assertNotIn("merkle_missing_source", prebuilt_merkles)
        self.assertEqual(exempt_build_ids, {"aa1111111111"})


if __name__ == "__main__":
    unittest.main()

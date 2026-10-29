# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import pathlib
import tempfile
import unittest
from pathlib import Path

import generate_idk


class GenerateIdkTests(unittest.TestCase):
    def test_unmergeables_error(self) -> None:
        for atom_type in generate_idk.UNMERGEABLE_ATOM_TYPES:
            meta_a: generate_idk.UnmergeableMeta = {
                "name": "fallback",
                "type": atom_type,
                "stable": True,
            }
            a: generate_idk.PartialIDK = generate_idk.PartialIDK(
                manifest_src=Path("thingy/meta/manifest.json"),
                atoms={
                    Path("foo/bar.json"): generate_idk.PartialAtom(
                        meta_src=pathlib.Path("src/foo/bar.json"),
                        meta=meta_a,
                        dest_to_src={},
                    )
                },
            )
            meta_b: generate_idk.UnmergeableMeta = {
                "name": "different_fallback",
                "type": atom_type,
                "stable": True,
            }
            b: generate_idk.PartialIDK = generate_idk.PartialIDK(
                manifest_src=Path("thingy/meta/manifest.json"),
                atoms={
                    Path("foo/bar.json"): generate_idk.PartialAtom(
                        meta_src=pathlib.Path("src/foo/bar.json"),
                        meta=meta_b,
                        dest_to_src={},
                    )
                },
            )

            with self.assertRaises(generate_idk.AtomMergeError) as e:
                generate_idk.MergedIDK().merge_with(a).merge_with(b)
            self.assertIn(
                "Key 'name' does not match", str(e.exception.__cause__)
            )

    def test_unmergeables_pass(self) -> None:
        for atom_type in generate_idk.UNMERGEABLE_ATOM_TYPES:
            meta_a1: generate_idk.UnmergeableMeta = {
                "name": "fallback",
                "type": atom_type,
                "stable": True,
            }
            meta_a2: generate_idk.UnmergeableMeta = {
                "name": "fallback",
                "type": "cc_source_library",
                "stable": True,
            }
            a: generate_idk.PartialIDK = generate_idk.PartialIDK(
                manifest_src=Path("thingy/meta/manifest.json"),
                atoms={
                    Path("foo/bar.json"): generate_idk.PartialAtom(
                        meta_src=pathlib.Path("src/foo/bar.json"),
                        meta=meta_a1,
                        dest_to_src={},
                    ),
                    Path("foo/qux.json"): generate_idk.PartialAtom(
                        meta_src=pathlib.Path("src/foo/qux.json"),
                        meta=meta_a2,
                        dest_to_src={},
                    ),
                },
            )
            meta_b1: generate_idk.UnmergeableMeta = {
                "name": "fallback",
                "type": atom_type,
                "stable": True,
            }
            meta_b2: generate_idk.UnmergeableMeta = {
                "name": "fallback",
                "type": atom_type,
                "stable": True,
            }
            b: generate_idk.PartialIDK = generate_idk.PartialIDK(
                manifest_src=Path("thingy/meta/manifest.json"),
                atoms={
                    Path("foo/bar.json"): generate_idk.PartialAtom(
                        meta_src=pathlib.Path("src/foo/bar.json"),
                        meta=meta_b1,
                        dest_to_src={},
                    ),
                    Path("foo/baz.json"): generate_idk.PartialAtom(
                        meta_src=pathlib.Path("src/foo/baz.json"),
                        meta=meta_b2,
                        dest_to_src={},
                    ),
                },
            )

            expected_meta_1: generate_idk.UnmergeableMeta = {
                "name": "fallback",
                "type": atom_type,
                "stable": True,
            }
            expected_meta_2: generate_idk.UnmergeableMeta = {
                "name": "fallback",
                "type": atom_type,
                "stable": True,
            }
            expected_meta_3: generate_idk.UnmergeableMeta = {
                "name": "fallback",
                "type": "cc_source_library",
                "stable": True,
            }
            self.assertEqual(
                generate_idk.MergedIDK().merge_with(a).merge_with(b),
                generate_idk.MergedIDK(
                    atoms={
                        Path("foo/bar.json"): expected_meta_1,
                        Path("foo/baz.json"): expected_meta_2,
                        Path("foo/qux.json"): expected_meta_3,
                    },
                    dest_to_src={},
                ),
            )

    def test_merge_cc_prebuilt_library_pass(self) -> None:
        meta_a: generate_idk.CCPrebuiltLibraryMeta = {
            "name": "fallback",
            "type": "cc_prebuilt_library",
            "binaries": {"a": "a"},
            "variants": [{"constraints": "a"}],
            "stable": True,
        }
        a = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_a,
                    dest_to_src={},
                ),
            },
        )
        meta_b: generate_idk.CCPrebuiltLibraryMeta = {
            "name": "fallback",
            "type": "cc_prebuilt_library",
            "binaries": {"b": "b", "c": "c"},
            "variants": [{"constraints": "b"}, {"constraints": "c"}],
            "stable": True,
        }
        b = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_b,
                    dest_to_src={},
                ),
            },
        )

        expected_meta: generate_idk.CCPrebuiltLibraryMeta = {
            "name": "fallback",
            "type": "cc_prebuilt_library",
            "binaries": {"a": "a", "b": "b", "c": "c"},
            "variants": [
                {"constraints": "a"},
                {"constraints": "b"},
                {"constraints": "c"},
            ],
            "stable": True,
        }
        self.assertEqual(
            generate_idk.MergedIDK().merge_with(a).merge_with(b),
            generate_idk.MergedIDK(
                atoms={
                    Path("atom0.json"): expected_meta,
                },
                dest_to_src={},
            ),
        )

    def test_merge_package_pass(self) -> None:
        meta_a: generate_idk.PackageMeta = {
            "name": "fallback",
            "type": "package",
            "variants": [
                {"api_level": 12, "arch": "x64"},
                {"api_level": 13, "arch": "x64"},
            ],
            "stable": True,
        }
        a = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_a,
                    dest_to_src={},
                ),
            },
        )
        meta_b: generate_idk.PackageMeta = {
            "name": "fallback",
            "type": "package",
            "variants": [
                {"api_level": 13, "arch": "arm64"},
                {"api_level": 14, "arch": "arm64"},
            ],
            "stable": True,
        }
        b = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_b,
                    dest_to_src={},
                ),
            },
        )

        expected_meta: generate_idk.PackageMeta = {
            "name": "fallback",
            "type": "package",
            "variants": [
                {"api_level": 12, "arch": "x64"},
                {"api_level": 13, "arch": "x64"},
                {"api_level": 13, "arch": "arm64"},
                {"api_level": 14, "arch": "arm64"},
            ],
            "stable": True,
        }
        self.assertEqual(
            generate_idk.MergedIDK().merge_with(a).merge_with(b),
            generate_idk.MergedIDK(
                atoms={
                    Path("atom0.json"): expected_meta,
                },
                dest_to_src={},
            ),
        )

    def test_merge_loadable_module_pass(self) -> None:
        meta_a: generate_idk.LoadableModuleMeta = {
            "name": "fallback",
            "type": "loadable_module",
            "binaries": {"a": "a"},
            "stable": True,
        }
        a = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_a,
                    dest_to_src={},
                ),
            },
        )
        meta_b: generate_idk.LoadableModuleMeta = {
            "name": "fallback",
            "type": "loadable_module",
            "binaries": {"b": "b", "c": "c"},
            "stable": True,
        }
        b = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_b,
                    dest_to_src={},
                ),
            },
        )

        expected_meta: generate_idk.LoadableModuleMeta = {
            "name": "fallback",
            "type": "loadable_module",
            "binaries": {"a": "a", "b": "b", "c": "c"},
            "stable": True,
        }
        self.assertEqual(
            generate_idk.MergedIDK().merge_with(a).merge_with(b),
            generate_idk.MergedIDK(
                atoms={
                    Path("atom0.json"): expected_meta,
                },
                dest_to_src={},
            ),
        )

    def test_merge_sysroot_pass(self) -> None:
        meta_a: generate_idk.SysrootMeta = {
            "name": "fallback",
            "type": "sysroot",
            "versions": {"a": "a"},
            "variants": [{"constraints": "a"}],
            "stable": True,
        }
        a = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_a,
                    dest_to_src={},
                ),
            },
        )
        meta_b: generate_idk.SysrootMeta = {
            "name": "fallback",
            "type": "sysroot",
            "versions": {"b": "b", "c": "c"},
            "variants": [{"constraints": "b"}, {"constraints": "c"}],
            "stable": True,
        }
        b = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_b,
                    dest_to_src={},
                ),
            },
        )

        expected_meta: generate_idk.SysrootMeta = {
            "name": "fallback",
            "type": "sysroot",
            "versions": {"a": "a", "b": "b", "c": "c"},
            "variants": [
                {"constraints": "a"},
                {"constraints": "b"},
                {"constraints": "c"},
            ],
            "stable": True,
        }
        self.assertEqual(
            generate_idk.MergedIDK().merge_with(a).merge_with(b),
            generate_idk.MergedIDK(
                atoms={
                    Path("atom0.json"): expected_meta,
                },
                dest_to_src={},
            ),
        )

    def test_sysroot_multi_variant_get_file_list(self) -> None:
        sysroot_meta = {
            "name": "sysroot",
            "type": "sysroot",
            "ifs_files": ["pkg/sysroot/libc.ifs", "pkg/sysroot/zircon.ifs"],
            "versions": {
                "arm64": {
                    "root": "arch/arm64/sysroot",
                    "include_dir": "arch/arm64/sysroot/include",
                    "dist_dir": "arch/arm64/sysroot",
                    "link_libs": ["arch/arm64/sysroot/lib/libc.so"],
                    "dist_libs": ["arch/arm64/sysroot/dist/lib/ld.so.1"],
                    "debug_libs": ["arch/arm64/sysroot/debug/libc.so"],
                },
                "x64": {
                    "root": "arch/x64/sysroot",
                    "include_dir": "arch/x64/sysroot/include",
                    "dist_dir": "arch/x64/sysroot",
                    "link_libs": ["arch/x64/sysroot/lib/libc.so"],
                    "dist_libs": ["arch/x64/sysroot/dist/lib/ld.so.1"],
                    "debug_libs": ["arch/x64/sysroot/debug/libc.so"],
                },
            },
            "variants": [
                {
                    "constraints": {"api_level": "27", "arch": "arm64"},
                    "values": {
                        "root": "obj/arm64-api-27/sysroot",
                        "include_dir": "obj/arm64-api-27/sysroot/include",
                        "sysroot_dir": "obj/arm64-api-27/sysroot",
                        "link_libs": ["obj/arm64-api-27/sysroot/lib/libc.so"],
                        "dist_libs": [
                            "obj/arm64-api-27/sysroot/dist/lib/ld.so.1"
                        ],
                        "debug_libs": [
                            "obj/arm64-api-27/sysroot/debug/libc.so"
                        ],
                    },
                },
                {
                    "constraints": {"api_level": "27", "arch": "x64"},
                    "values": {
                        "root": "obj/x64-api-27/sysroot",
                        "include_dir": "obj/x64-api-27/sysroot/include",
                        "sysroot_dir": "obj/x64-api-27/sysroot",
                        "link_libs": ["obj/x64-api-27/sysroot/lib/libc.so"],
                        "dist_libs": [
                            "obj/x64-api-27/sysroot/dist/lib/ld.so.1"
                        ],
                        "debug_libs": ["obj/x64-api-27/sysroot/debug/libc.so"],
                    },
                },
            ],
        }
        files = generate_idk.PartialIDK._get_file_list("sysroot", sysroot_meta)
        self.assertEqual(files.count("pkg/sysroot/libc.ifs"), 1)
        self.assertEqual(files.count("pkg/sysroot/zircon.ifs"), 1)
        self.assertIn("arch/arm64/sysroot/lib/libc.so", files)
        self.assertIn("arch/x64/sysroot/lib/libc.so", files)
        self.assertIn("obj/arm64-api-27/sysroot/lib/libc.so", files)
        self.assertIn("obj/x64-api-27/sysroot/lib/libc.so", files)

    def test_merge_variants_cc_prebuilt_library_pass(self) -> None:
        meta_a: generate_idk.CCPrebuiltLibraryMeta = {
            "name": "fallback",
            "type": "cc_prebuilt_library",
            "binaries": {},
            "variants": [{"constraints": "a"}],
            "stable": True,
        }
        a = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_a,
                    dest_to_src={},
                ),
            },
        )
        meta_b: generate_idk.CCPrebuiltLibraryMeta = {
            "name": "fallback",
            "type": "cc_prebuilt_library",
            "binaries": {},
            "variants": [{"constraints": "b"}, {"constraints": "c"}],
            "stable": True,
        }
        b = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_b,
                    dest_to_src={},
                ),
            },
        )

        expected_meta: generate_idk.CCPrebuiltLibraryMeta = {
            "name": "fallback",
            "type": "cc_prebuilt_library",
            "binaries": {},
            "variants": [
                {"constraints": "a"},
                {"constraints": "b"},
                {"constraints": "c"},
            ],
            "stable": True,
        }
        self.assertEqual(
            generate_idk.MergedIDK().merge_with(a).merge_with(b),
            generate_idk.MergedIDK(
                atoms={
                    Path("atom0.json"): expected_meta,
                },
                dest_to_src={},
            ),
        )

    def test_merge_binaries_cc_prebuilt_library_fail(self) -> None:
        meta_a: generate_idk.CCPrebuiltLibraryMeta = {
            "name": "fallback",
            "type": "cc_prebuilt_library",
            "binaries": {"a": "a"},
            "variants": [],
            "stable": True,
        }
        a = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_a,
                    dest_to_src={},
                ),
            },
        )
        meta_b: generate_idk.CCPrebuiltLibraryMeta = {
            "name": "fallback",
            "type": "cc_prebuilt_library",
            "binaries": {"a": "a", "c": "c"},
            "variants": [],
            "stable": True,
        }
        b = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_b,
                    dest_to_src={},
                ),
            },
        )

        with self.assertRaises(generate_idk.AtomMergeError) as e:
            generate_idk.MergedIDK().merge_with(a).merge_with(b)
        self.assertIn("overlapping keys", str(e.exception.__cause__))

    def test_merge_binaries_loadable_module_fail(self) -> None:
        meta_a: generate_idk.LoadableModuleMeta = {
            "name": "fallback",
            "type": "loadable_module",
            "binaries": {"a": "a"},
            "stable": True,
        }
        a = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_a,
                    dest_to_src={},
                ),
            },
        )
        meta_b: generate_idk.LoadableModuleMeta = {
            "name": "fallback",
            "type": "loadable_module",
            "binaries": {"a": "a", "c": "c"},
            "stable": True,
        }
        b = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_b,
                    dest_to_src={},
                ),
            },
        )

        with self.assertRaises(generate_idk.AtomMergeError) as e:
            generate_idk.MergedIDK().merge_with(a).merge_with(b)
        self.assertIn("overlapping keys", str(e.exception.__cause__))

    def test_merge_versions_fail(self) -> None:
        meta_a: generate_idk.SysrootMeta = {
            "name": "sysroot",
            "type": "sysroot",
            "versions": {
                "a": "a",
            },
            "variants": [],
            "stable": True,
        }
        a = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_a,
                    dest_to_src={},
                ),
            },
        )
        meta_b: generate_idk.SysrootMeta = {
            "name": "sysroot",
            "type": "sysroot",
            "versions": {
                "a": "a",
                "c": "c",
            },
            "variants": [],
            "stable": True,
        }
        b = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_b,
                    dest_to_src={},
                ),
            },
        )

        with self.assertRaises(generate_idk.AtomMergeError) as e:
            generate_idk.MergedIDK().merge_with(a).merge_with(b)
        self.assertIn("overlapping keys", str(e.exception.__cause__))

    def test_merge_variants_cc_prebuilt_library_fail(self) -> None:
        meta_a: generate_idk.CCPrebuiltLibraryMeta = {
            "name": "fallback",
            "type": "cc_prebuilt_library",
            "binaries": {},
            "variants": [{"constraints": "a"}],
            "stable": True,
        }
        a = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_a,
                    dest_to_src={},
                ),
            },
        )
        meta_b: generate_idk.CCPrebuiltLibraryMeta = {
            "name": "fallback",
            "type": "cc_prebuilt_library",
            "binaries": {},
            "variants": [{"constraints": "a"}, {"constraints": "c"}],
            "stable": True,
        }
        b = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_b,
                    dest_to_src={},
                ),
            },
        )

        with self.assertRaises(generate_idk.AtomMergeError) as e:
            generate_idk.MergedIDK().merge_with(a).merge_with(b)
        self.assertIn("duplicate variants", str(e.exception.__cause__))

    def test_merge_variants_sysroot_fail(self) -> None:
        meta_a: generate_idk.SysrootMeta = {
            "name": "fallback",
            "type": "sysroot",
            "versions": {},
            "variants": [{"constraints": "a"}],
            "stable": True,
        }
        a = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_a,
                    dest_to_src={},
                ),
            },
        )
        meta_b: generate_idk.SysrootMeta = {
            "name": "fallback",
            "type": "sysroot",
            "versions": {},
            "variants": [{"constraints": "a"}, {"constraints": "c"}],
            "stable": True,
        }
        b = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_b,
                    dest_to_src={},
                ),
            },
        )

        with self.assertRaises(generate_idk.AtomMergeError) as e:
            generate_idk.MergedIDK().merge_with(a).merge_with(b)
        self.assertIn("duplicate variants", str(e.exception.__cause__))

    def test_merge_variants_package_fail(self) -> None:
        meta_a: generate_idk.PackageMeta = {
            "name": "fallback",
            "type": "package",
            "variants": [{"api_level": 12, "arch": "x64"}],
            "stable": True,
        }
        a = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_a,
                    dest_to_src={},
                ),
            },
        )
        meta_b: generate_idk.PackageMeta = {
            "name": "fallback",
            "type": "package",
            "variants": [
                {"api_level": 12, "arch": "x64"},
                {"api_level": 12, "arch": "arm64"},
            ],
            "stable": True,
        }
        b = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("atom0.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/atom0.json"),
                    meta=meta_b,
                    dest_to_src={},
                ),
            },
        )

        with self.assertRaises(generate_idk.AtomMergeError) as e:
            generate_idk.MergedIDK().merge_with(a).merge_with(b)
        self.assertIn("duplicate variants", str(e.exception.__cause__))

    def test_merge_files(self) -> None:
        meta_a: generate_idk.UnmergeableMeta = {
            "name": "fallback",
            "type": "cc_source_library",
            "stable": True,
        }
        a: generate_idk.PartialIDK = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("foo/bar.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/foo/bar.json"),
                    meta=meta_a,
                    dest_to_src={
                        pathlib.Path("dest/foo"): pathlib.Path("src/foo"),
                        pathlib.Path("dest/bar"): pathlib.Path("src/bar"),
                    },
                )
            },
        )
        meta_b: generate_idk.UnmergeableMeta = {
            "name": "fallback",
            "type": "cc_source_library",
            "stable": True,
        }
        b: generate_idk.PartialIDK = generate_idk.PartialIDK(
            manifest_src=Path("thingy/meta/manifest.json"),
            atoms={
                Path("foo/bar.json"): generate_idk.PartialAtom(
                    meta_src=pathlib.Path("src/foo/bar.json"),
                    meta=meta_b,
                    dest_to_src={
                        pathlib.Path("dest/foo"): pathlib.Path("src/foo"),
                        pathlib.Path("dest/baz"): pathlib.Path("src/baz"),
                    },
                )
            },
        )
        print(generate_idk.MergedIDK().merge_with(a))
        print(generate_idk.MergedIDK().merge_with(a).merge_with(b))

        expected_meta: generate_idk.UnmergeableMeta = {
            "name": "fallback",
            "type": "cc_source_library",
            "stable": True,
        }
        self.assertEqual(
            generate_idk.MergedIDK().merge_with(a).merge_with(b),
            generate_idk.MergedIDK(
                atoms={
                    Path("foo/bar.json"): expected_meta,
                },
                dest_to_src={
                    pathlib.Path("dest/foo"): pathlib.Path("src/foo"),
                    pathlib.Path("dest/bar"): pathlib.Path("src/bar"),
                    pathlib.Path("dest/baz"): pathlib.Path("src/baz"),
                },
            ),
        )

    def test_merge_files_check_equality_pass(self) -> None:
        with tempfile.TemporaryDirectory() as dir:
            d = pathlib.Path(dir)

            file_a = d / "a.txt"
            file_b = d / "b.txt"

            file_a.write_text("foo")
            file_b.write_text("foo")

            meta_a: generate_idk.UnmergeableMeta = {
                "name": "fallback",
                "type": "cc_source_library",
                "stable": True,
            }
            a = generate_idk.PartialIDK(
                manifest_src=Path("thingy/meta/manifest.json"),
                atoms={
                    Path("foo/bar.json"): generate_idk.PartialAtom(
                        meta_src=pathlib.Path("src/foo/bar.json"),
                        meta=meta_a,
                        dest_to_src={
                            pathlib.Path("dest/foo"): file_a,
                        },
                    ),
                },
            )
            meta_b: generate_idk.UnmergeableMeta = {
                "name": "fallback",
                "type": "cc_source_library",
                "stable": True,
            }
            b = generate_idk.PartialIDK(
                manifest_src=Path("thingy/meta/manifest.json"),
                atoms={
                    Path("foo/bar.json"): generate_idk.PartialAtom(
                        meta_src=pathlib.Path("src/foo/bar.json"),
                        meta=meta_b,
                        dest_to_src={
                            pathlib.Path("dest/foo"): file_b,
                        },
                    ),
                },
            )
            expected_meta: generate_idk.UnmergeableMeta = {
                "name": "fallback",
                "type": "cc_source_library",
                "stable": True,
            }
            self.assertEqual(
                generate_idk.MergedIDK().merge_with(a).merge_with(b),
                generate_idk.MergedIDK(
                    atoms={
                        Path("foo/bar.json"): expected_meta,
                    },
                    dest_to_src={
                        pathlib.Path("dest/foo"): file_a,
                    },
                ),
            )

    def test_merge_files_check_equality_fail(self) -> None:
        with tempfile.TemporaryDirectory() as dir:
            d = pathlib.Path(dir)

            file_a = d / "a.txt"
            file_b = d / "b.txt"

            file_a.write_text("foo")
            file_b.write_text("bar")

            meta_a: generate_idk.UnmergeableMeta = {
                "name": "fallback",
                "type": "cc_source_library",
                "stable": True,
            }
            a = generate_idk.PartialIDK(
                manifest_src=Path("thingy/meta/manifest.json"),
                atoms={
                    Path("foo/bar.json"): generate_idk.PartialAtom(
                        meta_src=pathlib.Path("src/foo/bar.json"),
                        meta=meta_a,
                        dest_to_src={
                            pathlib.Path("dest/foo"): file_a,
                        },
                    ),
                },
            )
            meta_b: generate_idk.UnmergeableMeta = {
                "name": "fallback",
                "type": "cc_source_library",
                "stable": True,
            }
            b = generate_idk.PartialIDK(
                manifest_src=Path("thingy/meta/manifest.json"),
                atoms={
                    Path("foo/bar.json"): generate_idk.PartialAtom(
                        meta_src=pathlib.Path("src/foo/bar.json"),
                        meta=meta_b,
                        dest_to_src={
                            pathlib.Path("dest/foo"): file_b,
                        },
                    ),
                },
            )

            with self.assertRaises(AssertionError) as e:
                generate_idk.MergedIDK().merge_with(a).merge_with(b)
            self.assertIn("Multiple non-identical files", str(e.exception))


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import struct
import tempfile
import unittest
from pathlib import Path

from build.api import debug_symbols
from build.bazel.scripts.bazel_action_utils import BazelTargetInfosMap
from build.bazel.scripts.bazel_ninja_delayed_actions import (
    TargetWithPlatform,
    compute_sources_by_owner,
    read_extra_bazel_targets,
    should_update_stamp,
    validate_export_test_packages_owners,
    validate_extra_bazel_targets,
)

_HOST = "//build/bazel/platforms:linux_x64"
_FUCHSIA = "//build/bazel/platforms:fuchsia_x64"


def _target_infos(
    *targets: tuple[str, str],
) -> BazelTargetInfosMap:
    """Builds a BazelTargetInfosMap from (bazel_target, platform) pairs."""
    return BazelTargetInfosMap(
        [
            {
                "bazel_target": target,
                "bazel_platform_label": platform,
                "bazel_platform_config": "host",
                "ninja_depfile": f"gen/{target}.d",
                "gn_targets_manifest": f"gen/{target}.manifest",
                "stamp_path": f"obj/{target}.stamp",
                "update_rust_project": False,
                "type": "file",
                "bazel_file": f"bazel-out/{target}",
                "ninja_file": f"obj/{target}",
            }
            for target, platform in targets
        ]
    )


def _generate_stripped_elf(build_id: bytes) -> bytes:
    """Generate a minimal stripped ELF64 file with a PT_NOTE program header."""
    note_name = b"GNU\x00"
    note_header = struct.pack("<III", len(note_name), len(build_id), 3)
    note_data = note_header + note_name + build_id
    elf_header_size = 64
    ph_size = 56
    ph_offset = elf_header_size
    note_offset = elf_header_size + ph_size
    elf_header = struct.pack(
        "<16sHHIQQQIHHHHHH",
        b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00",
        2,
        0x3E,
        1,
        0,
        ph_offset,
        0,
        0,
        elf_header_size,
        ph_size,
        1,
        0,
        0,
        0,
    )
    ph = struct.pack(
        "<IIQQQQQQ",
        4,
        4,
        note_offset,
        0,
        0,
        len(note_data),
        len(note_data),
        4,
    )
    return elf_header + ph + note_data


class ReadExtraBazelTargetsTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self.addCleanup(self._td.cleanup)
        self.path = Path(self._td.name) / "suites.txt"

    def test_one_label_per_line(self) -> None:
        self.path.write_text("//foo:tests\n//bar:tests\n")
        self.assertEqual(
            read_extra_bazel_targets(self.path), ["//foo:tests", "//bar:tests"]
        )

    def test_missing_trailing_newline(self) -> None:
        self.path.write_text("//foo:tests\n//bar:tests")
        self.assertEqual(
            read_extra_bazel_targets(self.path), ["//foo:tests", "//bar:tests"]
        )

    def test_blank_lines_and_surrounding_whitespace_are_ignored(self) -> None:
        self.path.write_text("\n  //foo:tests  \n\n//bar:tests\n\n")
        self.assertEqual(
            read_extra_bazel_targets(self.path), ["//foo:tests", "//bar:tests"]
        )

    def test_empty_file(self) -> None:
        # A generated_file() whose metadata walk collected nothing still
        # exists, so an empty list must not be an error.
        self.path.write_text("")
        self.assertEqual(read_extra_bazel_targets(self.path), [])

    def test_missing_file_raises_value_error(self) -> None:
        with self.assertRaisesRegex(
            ValueError, r"extra_bazel_targets_file .* does not exist"
        ):
            read_extra_bazel_targets(self.path)


class ValidateExtraBazelTargetsTest(unittest.TestCase):
    def test_no_overlap(self) -> None:
        validate_extra_bazel_targets(
            {
                TargetWithPlatform("//build/bazel/host_tests:stamp", _HOST): [
                    "//foo:tests",
                    "//bar:tests",
                ]
            },
            _target_infos(("//build/bazel/host_tests:stamp", _HOST)),
        )

    def test_extra_target_also_declared_as_bazel_target(self) -> None:
        with self.assertRaisesRegex(
            ValueError, r"//foo:tests is both declared by a bazel_action\(\)"
        ):
            validate_extra_bazel_targets(
                {
                    TargetWithPlatform(
                        "//build/bazel/host_tests:stamp", _HOST
                    ): ["//foo:tests"]
                },
                _target_infos(
                    ("//build/bazel/host_tests:stamp", _HOST),
                    ("//foo:tests", _HOST),
                ),
            )

    def test_extra_target_claimed_by_two_actions(self) -> None:
        with self.assertRaisesRegex(
            ValueError, r"//foo:tests is named by the extra_bazel_targets_file"
        ):
            validate_extra_bazel_targets(
                {
                    TargetWithPlatform("//a:stamp", _HOST): ["//foo:tests"],
                    TargetWithPlatform("//b:stamp", _HOST): ["//foo:tests"],
                },
                _target_infos(("//a:stamp", _HOST), ("//b:stamp", _HOST)),
            )

    def test_same_label_on_different_platforms_is_allowed(self) -> None:
        # The same Bazel label built in two configurations is two distinct
        # builds, and each gets its own batch.
        validate_extra_bazel_targets(
            {
                TargetWithPlatform("//a:stamp", _HOST): ["//foo:tests"],
                TargetWithPlatform("//b:stamp", _FUCHSIA): ["//foo:tests"],
            },
            _target_infos(("//a:stamp", _HOST), ("//b:stamp", _FUCHSIA)),
        )


class ValidateExportTestPackagesOwnersTest(unittest.TestCase):
    def _infos(self, *exporters: str) -> BazelTargetInfosMap:
        return BazelTargetInfosMap(
            [
                {
                    "bazel_target": target,
                    "bazel_platform_label": _FUCHSIA,
                    "bazel_platform_config": "fuchsia",
                    "ninja_depfile": f"gen/{target}.d",
                    "gn_targets_manifest": f"gen/{target}.manifest",
                    "stamp_path": f"obj/{target}.stamp",
                    "update_rust_project": False,
                    "export_test_packages": True,
                    "type": "file",
                    "bazel_file": f"bazel-out/{target}",
                    "ninja_file": f"obj/{target}",
                }
                for target in exporters
            ]
        )

    def test_one_owner(self) -> None:
        validate_export_test_packages_owners(self._infos("//a:stamp"))

    def test_two_owners(self) -> None:
        with self.assertRaisesRegex(ValueError, "//a:stamp, //b:stamp"):
            validate_export_test_packages_owners(
                self._infos("//a:stamp", "//b:stamp")
            )


class ComputeSourcesByOwnerTest(unittest.TestCase):
    def test_requested_targets_own_their_own_sources(self) -> None:
        self.assertEqual(
            compute_sources_by_owner(
                {"//a:stamp": ["a.cc"], "//b:stamp": ["b.cc"]},
                ["//a:stamp", "//b:stamp"],
                {},
                _HOST,
            ),
            {
                TargetWithPlatform("//a:stamp", _HOST): ["a.cc"],
                TargetWithPlatform("//b:stamp", _HOST): ["b.cc"],
            },
        )

    def test_requested_target_without_sources_is_still_present(self) -> None:
        # The caller writes stamp files while iterating the result, so an
        # action whose suite list is empty still needs an entry.
        self.assertEqual(
            compute_sources_by_owner({}, ["//a:stamp"], {}, _HOST),
            {TargetWithPlatform("//a:stamp", _HOST): []},
        )

    def test_expanded_suite_members_belong_to_the_extras_owner(self) -> None:
        owner = TargetWithPlatform("//host_tests:stamp", _HOST)
        self.assertEqual(
            compute_sources_by_owner(
                {
                    "//host_tests:stamp": ["stamp.sh"],
                    "//foo:foo_test": ["foo_test.cc"],
                    "//bar:bar_test": ["bar_test.cc"],
                },
                ["//host_tests:stamp"],
                {owner: ["//foo:tests", "//bar:tests"]},
                _HOST,
            ),
            {owner: ["stamp.sh", "foo_test.cc", "bar_test.cc"]},
        )

    def test_directly_named_extra_targets_belong_only_to_their_owner(
        self,
    ) -> None:
        first = TargetWithPlatform("//a:stamp", _HOST)
        second = TargetWithPlatform("//b:stamp", _HOST)
        self.assertEqual(
            compute_sources_by_owner(
                {
                    "//foo:tests": ["foo/BUILD.bazel"],
                    "//bar:tests": ["bar/BUILD.bazel"],
                },
                ["//a:stamp", "//b:stamp"],
                {first: ["//foo:tests"], second: ["//bar:tests"]},
                _HOST,
            ),
            {
                first: ["foo/BUILD.bazel"],
                second: ["bar/BUILD.bazel"],
            },
        )

    def test_expanded_members_go_to_every_extras_owner_in_batch(self) -> None:
        # Bazel does not report which command line target a member was
        # expanded from, so the depfile is over-approximated rather than
        # under-approximated when several actions supplied extra targets.
        first = TargetWithPlatform("//a:stamp", _HOST)
        second = TargetWithPlatform("//b:stamp", _HOST)
        unbatched = TargetWithPlatform("//c:stamp", _HOST)
        self.assertEqual(
            compute_sources_by_owner(
                {"//foo:foo_test": ["foo_test.cc"]},
                ["//a:stamp", "//b:stamp"],
                {
                    first: ["//foo:tests"],
                    second: ["//bar:tests"],
                    unbatched: ["//baz:tests"],
                },
                _HOST,
            ),
            {first: ["foo_test.cc"], second: ["foo_test.cc"]},
        )

    def test_unaccountable_target_is_an_error(self) -> None:
        # Without this, the sources would be dropped and the depfile would
        # silently under-report the action's inputs.
        with self.assertRaisesRegex(
            ValueError, r"//foo:foo_test .* which no bazel_action\(\) requested"
        ):
            compute_sources_by_owner(
                {"//foo:foo_test": ["foo_test.cc"]},
                ["//a:stamp"],
                {},
                _HOST,
            )


class ShouldUpdateStampTest(unittest.TestCase):
    def test_missing_stamp(self) -> None:
        self.assertTrue(
            should_update_stamp(
                stamp_exists=False,
                declared_outputs=["obj/foo"],
                updated_outputs=set(),
                has_extra_targets=False,
            )
        )

    def test_declared_output_updated(self) -> None:
        self.assertTrue(
            should_update_stamp(
                stamp_exists=True,
                declared_outputs=["obj/foo", "obj/bar"],
                updated_outputs={Path("obj/bar")},
                has_extra_targets=False,
            )
        )

    def test_no_declared_output_updated(self) -> None:
        # Leaving the stamp alone is what lets restat prune dependents.
        self.assertFalse(
            should_update_stamp(
                stamp_exists=True,
                declared_outputs=["obj/foo"],
                updated_outputs={Path("obj/unrelated")},
                has_extra_targets=False,
            )
        )

    def test_extra_targets_always_update(self) -> None:
        # Outputs of extra targets (e.g. a rebuilt test package's meta.far)
        # are never declared to Ninja, so their changes can't be detected
        # here and dependents reading them via depfiles must still rerun.
        self.assertTrue(
            should_update_stamp(
                stamp_exists=True,
                declared_outputs=["obj/foo"],
                updated_outputs=set(),
                has_extra_targets=True,
            )
        )


class MergeDebugSymbolManifestsTest(unittest.TestCase):
    def test_merge_debug_symbol_manifests_by_target(self) -> None:
        import json
        import tempfile
        from pathlib import Path
        from unittest import mock

        import bazel_action_impl
        import build_utils

        with tempfile.TemporaryDirectory() as tmpdir:
            build_dir = Path(tmpdir)
            execroot = build_dir / "execroot"
            bin_dir = execroot / "bazel-out/x64/bin"
            bin_dir.mkdir(parents=True)

            (bin_dir / "foo.debug").write_bytes(b"")
            (bin_dir / "bar.debug").write_bytes(b"")

            (bin_dir / "foo_pkg.debug_symbols.json").write_text(
                json.dumps(
                    [
                        {
                            "label": "@@//src:foo_bin",
                            "debug": "bazel-out/x64/bin/foo.debug",
                            "os": "fuchsia",
                            "cpu": "x64",
                        }
                    ]
                )
            )
            (bin_dir / "bar_pkg.debug_symbols.json").write_text(
                json.dumps(
                    [
                        {
                            "label": "@@//src:foo_bin",
                            "debug": "bazel-out/x64/bin/foo.debug",
                            "dest_path": "bin/foo",
                            "os": "fuchsia",
                            "cpu": "x64",
                        },
                        {
                            "label": "@@//src:bar_bin",
                            "debug": "bazel-out/x64/bin/bar.debug",
                            "os": "fuchsia",
                            "cpu": "x64",
                        },
                    ]
                )
            )

            build_ids = {
                build_dir / "execroot/bazel-out/x64/bin/foo.debug": "11112222",
                build_dir / "execroot/bazel-out/x64/bin/bar.debug": "33334444",
            }

            with mock.patch.object(
                debug_symbols,
                "extract_gnu_build_id",
                side_effect=lambda p: build_ids[Path(p)],
            ):
                (
                    merged,
                    by_target,
                ) = bazel_action_impl.merge_debug_symbol_manifests(
                    [
                        "@@//src:foo_pkg,bazel-out/x64/bin/foo_pkg.debug_symbols.json",
                        "@@//src:bar_pkg,bazel-out/x64/bin/bar_pkg.debug_symbols.json",
                    ],
                    bazel_execroot=execroot,
                    build_dir=build_dir,
                    prebuilt_build_id_dir=build_dir / "prebuilt/.build-id",
                    time_profile=build_utils.TimeProfile(),
                )

            expected_foo = {
                "label": "@@//src:foo_bin",
                "debug": "execroot/bazel-out/x64/bin/foo.debug",
                "dest_path": "bin/foo",
                "os": "fuchsia",
                "cpu": "x64",
                "elf_build_id": "11112222",
            }
            expected_bar = {
                "label": "@@//src:bar_bin",
                "debug": "execroot/bazel-out/x64/bin/bar.debug",
                "os": "fuchsia",
                "cpu": "x64",
                "elf_build_id": "33334444",
            }

            self.assertEqual(merged, [expected_foo, expected_bar])
            self.assertIsInstance(
                by_target, bazel_action_impl.BazelDebugSymbolMap
            )
            self.assertEqual(
                by_target,
                {
                    "//src:foo_pkg": [expected_foo],
                    "//src:bar_pkg": [expected_foo, expected_bar],
                },
            )

    def test_merge_debug_symbol_manifests_missing_comma_raises(self) -> None:
        import tempfile
        from pathlib import Path

        import bazel_action_impl
        import build_utils

        with tempfile.TemporaryDirectory() as tmpdir:
            build_dir = Path(tmpdir)
            with self.assertRaisesRegex(AssertionError, "missing comma"):
                bazel_action_impl.merge_debug_symbol_manifests(
                    ["no_comma_path.json"],
                    bazel_execroot=build_dir,
                    build_dir=build_dir,
                    prebuilt_build_id_dir=build_dir / "prebuilt/.build-id",
                    time_profile=build_utils.TimeProfile(),
                )

    def test_merge_debug_symbol_manifests_with_prebuilt_package(self) -> None:
        import json
        import tempfile
        from pathlib import Path
        from unittest import mock

        import bazel_action_impl
        import build_utils

        with tempfile.TemporaryDirectory() as tmp_dir_str:
            tmp_dir = Path(tmp_dir_str)
            build_dir = tmp_dir / "out/default"
            execroot = build_dir / "execroot"
            bin_dir = execroot / "bazel-out/arm64/bin"
            bin_dir.mkdir(parents=True)

            prebuilt_build_id_dir = tmp_dir / "prebuilt/.build-id"
            debug_file = prebuilt_build_id_dir / "ab/cdef0123.debug"
            debug_file.parent.mkdir(parents=True)
            debug_file.write_text("elf_debug")

            pkg_dir = bin_dir / "prebuilt_pkg"
            (pkg_dir / "blobs").mkdir(parents=True)
            blob_with_sym = pkg_dir / "blobs/libvulkan.so"
            blob_with_sym.write_text("elf_stripped_1")
            blob_missing_sym = pkg_dir / "blobs/libother.so"
            blob_missing_sym.write_text("elf_stripped_2")

            pkg_manifest_path = pkg_dir / "rebased_package_manifest.json"
            pkg_manifest_path.write_text(
                json.dumps(
                    {
                        "version": "1",
                        "blob_sources_relative": "file",
                        "blobs": [
                            {
                                "path": "meta/",
                                "source_path": "blobs/meta.far",
                            },
                            {
                                "path": "lib/libvulkan.so",
                                "source_path": "blobs/libvulkan.so",
                            },
                            {
                                "path": "lib/libother.so",
                                "source_path": "blobs/libother.so",
                            },
                        ],
                    }
                )
            )

            (bin_dir / "prebuilt_pkg.debug_symbols.json").write_text(
                json.dumps(
                    [
                        {
                            "package_manifest": "bazel-out/arm64/bin/prebuilt_pkg/rebased_package_manifest.json",
                            "os": "fuchsia",
                            "cpu": "arm64",
                            "label": "@@//src:prebuilt_pkg_fuchsia_package",
                        }
                    ]
                )
            )

            build_ids = {
                blob_with_sym: "abcdef0123",
                blob_missing_sym: "9999999999",
            }

            with mock.patch.object(
                bazel_action_impl.DebugSymbolsManifestParser,
                "extract_build_id",
                side_effect=lambda p: build_ids.get(Path(p), ""),
            ):
                (
                    merged,
                    by_target,
                ) = bazel_action_impl.merge_debug_symbol_manifests(
                    [
                        "@@//src:prebuilt_pkg,bazel-out/arm64/bin/prebuilt_pkg.debug_symbols.json",
                    ],
                    bazel_execroot=execroot,
                    build_dir=build_dir,
                    time_profile=build_utils.TimeProfile(),
                    prebuilt_build_id_dir=prebuilt_build_id_dir,
                )

            expected_entry = {
                "debug": "../../prebuilt/.build-id/ab/cdef0123.debug",
                "elf_build_id": "abcdef0123",
                "dest_path": "lib/libvulkan.so",
                "os": "fuchsia",
                "cpu": "arm64",
                "label": "@@//src:prebuilt_pkg_fuchsia_package",
            }

            self.assertEqual(merged, [expected_entry])
            self.assertEqual(
                by_target,
                {
                    "//src:prebuilt_pkg": [expected_entry],
                },
            )

    def test_merge_debug_symbol_manifests_with_prebuilt_container(self) -> None:
        import json
        import tempfile
        from pathlib import Path
        from unittest import mock

        import bazel_action_impl
        import build_utils

        with tempfile.TemporaryDirectory() as tmp_dir_str:
            tmp_dir = Path(tmp_dir_str)
            build_dir = tmp_dir / "out/default"
            execroot = build_dir / "execroot"
            container_dir = (
                execroot / "bazel-out/arm64/bin/smart_product_config"
            )
            packages_dir = container_dir / "packages"
            blobs_dir = container_dir / "blobs"
            packages_dir.mkdir(parents=True)
            blobs_dir.mkdir(parents=True)

            prebuilt_build_id_dir = tmp_dir / "prebuilt/.build-id"
            build_id = "a6d0f3618ca5fa790dd486b2f22cf3669964b272"
            debug_file = (
                prebuilt_build_id_dir / f"{build_id[:2]}/{build_id[2:]}.debug"
            )
            debug_file.parent.mkdir(parents=True)
            debug_file.write_text("DEBUG SYMBOLS")

            merkle = "e7d02d73177f1486e5b26ee5d5349bdd43be54817965e30d6a0e499146487f7b"
            blob_file = blobs_dir / merkle
            blob_file.write_text("ELF BINARY")

            manifest_content = [
                {
                    "container_dir": "bazel-out/arm64/bin/smart_product_config",
                    "os": "fuchsia",
                    "cpu": "arm64",
                    "label": "@@//vendor/smart:smart_product_config",
                }
            ]
            manifest_file = (
                execroot / "bazel-out/arm64/bin/smart.debug_symbols.json"
            )
            manifest_file.parent.mkdir(parents=True, exist_ok=True)
            manifest_file.write_text(json.dumps(manifest_content))

            with mock.patch.object(
                bazel_action_impl.DebugSymbolsManifestParser,
                "extract_build_id",
                return_value=build_id,
            ):
                (
                    merged,
                    by_target,
                ) = bazel_action_impl.merge_debug_symbol_manifests(
                    [
                        "@@//vendor/smart:smart_product_config,bazel-out/arm64/bin/smart.debug_symbols.json",
                    ],
                    bazel_execroot=execroot,
                    build_dir=build_dir,
                    time_profile=build_utils.TimeProfile(),
                    prebuilt_build_id_dir=prebuilt_build_id_dir,
                )

            expected_entry = {
                "debug": f"../../prebuilt/.build-id/{build_id[:2]}/{build_id[2:]}.debug",
                "elf_build_id": build_id,
                "dest_path": f"blobs/{merkle}",
                "os": "fuchsia",
                "cpu": "arm64",
                "label": "@@//vendor/smart:smart_product_config",
            }

            self.assertEqual(merged, [expected_entry])
            self.assertEqual(
                by_target,
                {
                    "//vendor/smart:smart_product_config": [expected_entry],
                },
            )

    def test_prebuilt_package_expander(self) -> None:
        import json
        import tempfile
        from pathlib import Path
        from unittest import mock

        import bazel_action_impl

        with tempfile.TemporaryDirectory() as tmp_dir_str:
            tmp_dir = Path(tmp_dir_str)
            build_dir = tmp_dir / "out/default"
            execroot = build_dir / "execroot"
            bin_dir = execroot / "bazel-out/arm64/bin"
            bin_dir.mkdir(parents=True)

            prebuilt_build_id_dir = tmp_dir / "prebuilt/.build-id"
            debug_file = prebuilt_build_id_dir / "12/345678.debug"
            debug_file.parent.mkdir(parents=True)
            debug_file.write_text("elf_debug")

            pkg_dir = bin_dir / "my_pkg"
            (pkg_dir / "blobs").mkdir(parents=True)
            blob_file = pkg_dir / "blobs/libmy.so"
            blob_file.write_text("elf_blob")

            pkg_manifest_path = pkg_dir / "package_manifest.json"
            pkg_manifest_path.write_text(
                json.dumps(
                    {
                        "version": "1",
                        "blob_sources_relative": "file",
                        "blobs": [
                            {
                                "path": "lib/libmy.so",
                                "source_path": "blobs/libmy.so",
                            }
                        ],
                    }
                )
            )

            parser = bazel_action_impl.DebugSymbolsManifestParser(build_dir)
            with mock.patch.object(
                parser,
                "extract_build_id",
                return_value="12345678",
            ):
                expander = bazel_action_impl._PrebuiltPackageExpander(
                    execroot, prebuilt_build_id_dir, build_dir, parser
                )
                entry = {
                    "package_manifest": "bazel-out/arm64/bin/my_pkg/package_manifest.json",
                    "os": "fuchsia",
                    "cpu": "arm64",
                    "label": "@@//src:my_pkg",
                }
                expanded = expander.expand(entry)

            self.assertEqual(len(expanded), 1)
            self.assertEqual(expanded[0]["elf_build_id"], "12345678")
            self.assertEqual(expanded[0]["dest_path"], "lib/libmy.so")

            # Check that implicit input files were tracked.
            self.assertIn(pkg_manifest_path, expander.extra_input_files)
            self.assertIn(blob_file, expander.extra_input_files)
            self.assertIn(debug_file, expander.extra_input_files)

            # Check that parser.extra_input_files also tracked them.
            self.assertIn(pkg_manifest_path, parser.extra_input_files)
            self.assertIn(blob_file, parser.extra_input_files)
            self.assertIn(debug_file, parser.extra_input_files)

            # Check cache hit returns the same list.
            cached = expander.expand(entry)
            self.assertIs(cached, expanded)

    def test_prebuilt_container_expander(self) -> None:
        import json
        import tempfile
        from pathlib import Path
        from unittest import mock

        import bazel_action_impl

        with tempfile.TemporaryDirectory() as tmp_dir_str:
            tmp_dir = Path(tmp_dir_str)
            build_dir = tmp_dir / "out/default"
            execroot = build_dir / "execroot"
            container_dir = (
                execroot / "bazel-out/arm64/bin/smart_product_config"
            )
            packages_dir = container_dir / "packages"
            blobs_dir = container_dir / "blobs"
            packages_dir.mkdir(parents=True)
            blobs_dir.mkdir(parents=True)

            prebuilt_build_id_dir = tmp_dir / "prebuilt/.build-id"
            build_id = "a6d0f3618ca5fa790dd486b2f22cf3669964b272"
            debug_file = (
                prebuilt_build_id_dir / f"{build_id[:2]}/{build_id[2:]}.debug"
            )
            debug_file.parent.mkdir(parents=True)
            debug_file.write_text("DEBUG SYMBOLS")

            merkle = "e7d02d73177f1486e5b26ee5d5349bdd43be54817965e30d6a0e499146487f7b"
            blob_file = blobs_dir / merkle
            blob_file.write_text("ELF BINARY")

            pkg_manifest_path = packages_dir / "smart_pkg"
            pkg_manifest = {
                "version": "1",
                "repository": "fuchsia.com",
                "package": {
                    "name": "smart_pkg",
                    "version": "0",
                },
                "blobs": [
                    {
                        "source_path": f"../../blobs/{merkle}",
                        "path": "bin/smart_daemon",
                        "merkle": merkle,
                        "size": 100,
                    }
                ],
                "blob_sources_relative": "file",
            }
            pkg_manifest_path.write_text(json.dumps(pkg_manifest))

            parser = bazel_action_impl.DebugSymbolsManifestParser(build_dir)
            with mock.patch.object(
                bazel_action_impl.DebugSymbolsManifestParser,
                "extract_build_id",
                return_value=build_id,
            ):
                expander = bazel_action_impl._PrebuiltPackageExpander(
                    execroot, prebuilt_build_id_dir, build_dir, parser
                )
                entry = {
                    "container_dir": "bazel-out/arm64/bin/smart_product_config",
                    "os": "fuchsia",
                    "cpu": "arm64",
                    "label": "@@//vendor/smart:smart_product_config",
                }
                expanded = expander.expand_container(entry)

            self.assertEqual(len(expanded), 1)
            self.assertEqual(expanded[0]["elf_build_id"], build_id)
            self.assertEqual(expanded[0]["dest_path"], "bin/smart_daemon")

            # Check that implicit input files were tracked.
            self.assertIn(pkg_manifest_path, expander.extra_input_files)
            self.assertIn(blob_file, expander.extra_input_files)
            self.assertIn(debug_file, expander.extra_input_files)

            # Check cache hit returns the same list.
            cached = expander.expand_container(entry)
            self.assertIs(cached, expanded)

    def test_prebuilt_container_expander_with_product_configuration_and_merkle_blob(
        self,
    ) -> None:
        import json

        import bazel_action_impl

        with tempfile.TemporaryDirectory() as tmp_dir_str:
            tmp_dir = Path(tmp_dir_str)
            build_dir = tmp_dir / "out/default"
            execroot = build_dir / "execroot"
            container_dir = (
                execroot / "bazel-out/arm64/bin/smart_product_config"
            )
            blobs_dir = container_dir / "blobs"
            blobs_dir.mkdir(parents=True)

            build_id = "a6d0f3618ca5fa790dd486b2f22cf3669964b272"
            prebuilt_build_id_dir = tmp_dir / "prebuilt/.build-id"
            debug_file = (
                prebuilt_build_id_dir / f"{build_id[:2]}/{build_id[2:]}.debug"
            )
            debug_file.parent.mkdir(parents=True)
            debug_file.write_text("DEBUG")

            merkle = "e7d02d73177f1486e5b26ee5d5349bdd43be54817965e30d6a0e499146487f7b"
            blob_file = blobs_dir / merkle
            blob_file.write_bytes(
                _generate_stripped_elf(bytes.fromhex(build_id))
            )

            pkg_manifest_path = container_dir / "packages/amlogic_keys"
            pkg_manifest_path.parent.mkdir(parents=True)
            pkg_manifest = {
                "version": "1",
                "package": {"name": "amlogic-keys", "version": "0"},
                "blobs": [
                    {
                        "source_path": "bin/amlogic-netflix-keys",
                        "path": "bin/amlogic-netflix-keys",
                        "merkle": merkle,
                        "size": 1234,
                    }
                ],
                "blob_sources_relative": "file",
            }
            pkg_manifest_path.write_text(json.dumps(pkg_manifest))

            prod_config = {
                "packages": {"base": [{"manifest": "packages/amlogic_keys"}]}
            }
            (container_dir / "product_configuration.json").write_text(
                json.dumps(prod_config)
            )

            parser = bazel_action_impl.DebugSymbolsManifestParser(build_dir)
            expander = bazel_action_impl._PrebuiltPackageExpander(
                execroot, prebuilt_build_id_dir, build_dir, parser
            )
            entry = {
                "container_dir": "bazel-out/arm64/bin/smart_product_config",
                "os": "fuchsia",
                "cpu": "arm64",
                "label": "@@//vendor/smart:smart_product_config",
            }
            expanded = expander.expand_container(entry)
            self.assertEqual(len(expanded), 1)
            self.assertEqual(expanded[0]["elf_build_id"], build_id)
            self.assertEqual(
                expanded[0]["dest_path"], "bin/amlogic-netflix-keys"
            )

    def test_prebuilt_container_expander_with_image_assembly(
        self,
    ) -> None:
        import json

        import bazel_action_impl

        with tempfile.TemporaryDirectory() as tmp_dir_str:
            tmp_dir = Path(tmp_dir_str)
            build_dir = tmp_dir / "out/default"
            execroot = build_dir / "execroot"
            container_dir = execroot / "bazel-out/arm64/bin/assembly_out"
            container_dir.mkdir(parents=True)

            build_id1 = "1111222233334444555566667777888899990000"
            build_id2 = "aaaabbbbccccddddeeeeffff0000111122223333"
            prebuilt_build_id_dir = tmp_dir / "prebuilt/.build-id"
            for bid in (build_id1, build_id2):
                debug_file = (
                    prebuilt_build_id_dir / f"{bid[:2]}/{bid[2:]}.debug"
                )
                debug_file.parent.mkdir(parents=True, exist_ok=True)
                debug_file.write_text("DEBUG")

            pkg_manifest_path = container_dir / "base_pkg.json"
            blob_file = container_dir / "base_blob"
            blob_file.write_bytes(
                _generate_stripped_elf(bytes.fromhex(build_id1))
            )
            pkg_manifest = {
                "version": "1",
                "package": {"name": "base-pkg", "version": "0"},
                "blobs": [
                    {
                        "source_path": "base_blob",
                        "path": "bin/base_bin",
                        "merkle": "1234",
                        "size": 100,
                    }
                ],
                "blob_sources_relative": "file",
            }
            pkg_manifest_path.write_text(json.dumps(pkg_manifest))

            bootfs_bin = container_dir / "bootfs_bin"
            bootfs_bin.write_bytes(
                _generate_stripped_elf(bytes.fromhex(build_id2))
            )

            image_assembly = {
                "base": ["base_pkg.json"],
                "bootfs_files": [
                    {
                        "source": "bootfs_bin",
                        "destination": "bin/bootfs_bin",
                    }
                ],
            }
            (container_dir / "image_assembly.json").write_text(
                json.dumps(image_assembly)
            )

            parser = bazel_action_impl.DebugSymbolsManifestParser(build_dir)
            expander = bazel_action_impl._PrebuiltPackageExpander(
                execroot, prebuilt_build_id_dir, build_dir, parser
            )
            entry = {
                "container_dir": "bazel-out/arm64/bin/assembly_out",
                "os": "fuchsia",
                "cpu": "arm64",
                "label": "@@//products:assembly",
            }
            expanded = expander.expand_container(entry)
            self.assertEqual(len(expanded), 2)
            bids = {e["elf_build_id"] for e in expanded}
            self.assertEqual(bids, {build_id1, build_id2})


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import errno
import json
import os
import tempfile
import typing as T
import unittest
from pathlib import Path
from unittest import mock

from build.bazel.scripts import bazel_test_packages
from build.bazel.scripts.bazel_test_packages import export_path_for_manifest
from build.bazel.scripts.build_utils import BazelPaths


class ExportPathTest(unittest.TestCase):
    def test_round_trip(self) -> None:
        execroot_path = "bazel-out/cfg/bin/pkg/foo.package_manifest.json"
        export_path = export_path_for_manifest(execroot_path)
        self.assertTrue(
            export_path.startswith(bazel_test_packages.EXPORT_DIR + "/")
        )
        self.assertEqual(
            bazel_test_packages.execroot_path_for_export(export_path),
            execroot_path,
        )

    def test_rejects_other_paths(self) -> None:
        with self.assertRaises(ValueError):
            bazel_test_packages.execroot_path_for_export(
                "gen/build/bazel/output_base/execroot/_main/foo.json"
            )


class ExportPackagesTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self.root = Path(self._td.name)
        fuchsia_dir = self.root / "fuchsia"
        fuchsia_dir.mkdir()
        (fuchsia_dir / ".jiri_manifest").write_text("")
        BazelPaths.write_topdir_config_for_test(fuchsia_dir, "gen/build/bazel")
        self.build_dir = self.root / "out"
        self.build_dir.mkdir()
        self.bazel_paths = BazelPaths(fuchsia_dir, self.build_dir)
        self.execroot = self.bazel_paths.execroot
        self.bin_dir = self.execroot / "bazel-out/cfg/bin"
        self.bin_dir.mkdir(parents=True)

    def export(
        self, export_paths: list[str], incremental: bool = False
    ) -> None:
        bazel_test_packages.export_packages(
            self.bazel_paths, export_paths, incremental=incremental
        )

    def tearDown(self) -> None:
        self._td.cleanup()

    def _write_bazel_file(self, execroot_path: str, content: str) -> Path:
        path = self.execroot / execroot_path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
        return path

    def _write_manifest(
        self,
        execroot_path: str,
        blobs: dict[str, tuple[str, str]],
        subpackages: list[str] | None = None,
    ) -> str:
        """Writes a file-relative package manifest.

        Args:
            execroot_path: Where to write the manifest.
            blobs: Maps package paths to (merkle, execroot-relative source
                path). The source files must already exist.
            subpackages: Execroot-relative paths of subpackage manifests.

        Returns:
            The path that the manifest gets exported to.
        """
        manifest_path = self.execroot / execroot_path
        manifest: dict[str, T.Any] = {
            "version": "1",
            "package": {"name": manifest_path.stem, "version": "0"},
            "blob_sources_relative": "file",
            "blobs": [
                {
                    "path": path,
                    "merkle": merkle,
                    "size": (self.execroot / source).stat().st_size,
                    "source_path": os.path.relpath(
                        self.execroot / source, manifest_path.parent
                    ),
                }
                for path, (merkle, source) in blobs.items()
            ],
        }
        if subpackages:
            manifest["subpackages"] = [
                {
                    "name": Path(s).stem,
                    "merkle": "0" * 64,
                    "manifest_path": os.path.relpath(
                        self.execroot / s, manifest_path.parent
                    ),
                }
                for s in subpackages
            ]
        manifest_path.parent.mkdir(parents=True, exist_ok=True)
        manifest_path.write_text(json.dumps(manifest))
        return export_path_for_manifest(execroot_path)

    def _read_export(self, export_path: str) -> dict[str, T.Any]:
        with open(self.build_dir / export_path) as f:
            manifest: dict[str, T.Any] = json.load(f)
        return manifest

    def _exported_blob(self, export_path: str, blob_path: str) -> Path:
        manifest = self._read_export(export_path)
        self.assertEqual(manifest["blob_sources_relative"], "file")
        [blob] = [b for b in manifest["blobs"] if b["path"] == blob_path]
        return Path(
            os.path.normpath(
                (self.build_dir / export_path).parent / blob["source_path"]
            )
        )

    def test_exports_manifest_and_blobs(self) -> None:
        lib = self._write_bazel_file(
            "bazel-out/cfg/bin/pkg/lib.so_stripped", "lib"
        )
        self._write_bazel_file("bazel-out/cfg/bin/pkg/meta.far", "far")
        export_path = self._write_manifest(
            "bazel-out/cfg/bin/pkg/test.package_manifest.json",
            {
                "lib/lib.so": ("aaaa", "bazel-out/cfg/bin/pkg/lib.so_stripped"),
                "meta/": ("bbbb", "bazel-out/cfg/bin/pkg/meta.far"),
            },
        )

        self.export([export_path])

        exported_lib = self._exported_blob(export_path, "lib/lib.so")
        self.assertEqual(
            exported_lib,
            self.build_dir / bazel_test_packages.EXPORT_DIR / "blobs/aaaa",
        )
        self.assertEqual(exported_lib.read_text(), "lib")
        # Bazel outputs are hardlinked rather than copied.
        self.assertTrue(os.path.samefile(exported_lib, lib))
        self.assertEqual(
            self._exported_blob(export_path, "meta/").read_text(), "far"
        )

    def test_exported_blobs_survive_bazel_deleting_its_copy(self) -> None:
        lib = self._write_bazel_file(
            "bazel-out/cfg/bin/pkg/lib.so_stripped", "lib"
        )
        export_path = self._write_manifest(
            "bazel-out/cfg/bin/pkg/test.package_manifest.json",
            {"lib/lib.so": ("aaaa", "bazel-out/cfg/bin/pkg/lib.so_stripped")},
        )

        self.export([export_path])
        lib.unlink()

        self.assertEqual(
            self._exported_blob(export_path, "lib/lib.so").read_text(), "lib"
        )

    def test_working_dir_relative_manifest(self) -> None:
        self._write_bazel_file("bazel-out/cfg/bin/pkg/lib.so_stripped", "lib")
        manifest_path = self.bin_dir / "pkg/test.package_manifest.json"
        manifest_path.write_text(
            json.dumps(
                {
                    "version": "1",
                    "package": {"name": "test", "version": "0"},
                    "blobs": [
                        {
                            "path": "lib/lib.so",
                            "merkle": "aaaa",
                            "size": 3,
                            "source_path": "bazel-out/cfg/bin/pkg/lib.so_stripped",
                        }
                    ],
                }
            )
        )
        export_path = export_path_for_manifest(
            "bazel-out/cfg/bin/pkg/test.package_manifest.json"
        )

        self.export([export_path])

        self.assertEqual(
            self._exported_blob(export_path, "lib/lib.so").read_text(), "lib"
        )

    def test_copies_files_outside_output_base(self) -> None:
        # Files in the source tree may be edited in place, which would change
        # the contents of a hardlinked blob.
        source_file = self.root / "src/data.txt"
        source_file.parent.mkdir()
        source_file.write_text("data")
        (self.execroot / "src").symlink_to(source_file.parent)
        export_path = self._write_manifest(
            "bazel-out/cfg/bin/pkg/test.package_manifest.json",
            {"data/data.txt": ("cccc", "src/data.txt")},
        )

        self.export([export_path])

        exported = self._exported_blob(export_path, "data/data.txt")
        self.assertEqual(exported.read_text(), "data")
        self.assertFalse(os.path.samefile(exported, source_file))

    def test_missing_blob_is_an_error(self) -> None:
        self._write_bazel_file("bazel-out/cfg/bin/pkg/lib.so_stripped", "lib")
        export_path = self._write_manifest(
            "bazel-out/cfg/bin/pkg/test.package_manifest.json",
            {"lib/lib.so": ("aaaa", "bazel-out/cfg/bin/pkg/lib.so_stripped")},
        )
        (self.bin_dir / "pkg/lib.so_stripped").unlink()

        with self.assertRaisesRegex(FileNotFoundError, "lib/lib.so"):
            self.export([export_path])

    def test_missing_manifest(self) -> None:
        export_path = export_path_for_manifest(
            "bazel-out/cfg/bin/pkg/test.package_manifest.json"
        )

        with self.assertRaises(FileNotFoundError):
            self.export([export_path])

        self.export([export_path], incremental=True)
        self.assertFalse((self.build_dir / export_path).exists())

    def test_exports_subpackages(self) -> None:
        self._write_bazel_file("bazel-out/cfg/bin/sub/data", "sub data")
        self._write_bazel_file("bazel-out/cfg/bin/pkg/data", "pkg data")
        sub_export_path = self._write_manifest(
            "bazel-out/cfg/bin/sub/sub.package_manifest.json",
            {"data": ("dddd", "bazel-out/cfg/bin/sub/data")},
        )
        export_path = self._write_manifest(
            "bazel-out/cfg/bin/pkg/test.package_manifest.json",
            {"data": ("eeee", "bazel-out/cfg/bin/pkg/data")},
            subpackages=["bazel-out/cfg/bin/sub/sub.package_manifest.json"],
        )

        self.export([export_path])

        [subpackage] = self._read_export(export_path)["subpackages"]
        self.assertEqual(
            Path(
                os.path.normpath(
                    (self.build_dir / export_path).parent
                    / subpackage["manifest_path"]
                )
            ),
            self.build_dir / sub_export_path,
        )
        self.assertEqual(
            self._exported_blob(sub_export_path, "data").read_text(),
            "sub data",
        )

    def test_prunes_unreferenced_blobs(self) -> None:
        self._write_bazel_file("bazel-out/cfg/bin/a/data", "a v1")
        self._write_bazel_file("bazel-out/cfg/bin/b/data", "b")
        a_path = self._write_manifest(
            "bazel-out/cfg/bin/a/a.package_manifest.json",
            {"data": ("a1", "bazel-out/cfg/bin/a/data")},
        )
        b_path = self._write_manifest(
            "bazel-out/cfg/bin/b/b.package_manifest.json",
            {"data": ("b1", "bazel-out/cfg/bin/b/data")},
        )
        self.export([a_path, b_path])

        # Rebuild only package a, as `fx build --fuchsia_platform` might.
        # Package b isn't rebuilt this time, so its exported blob must be kept.
        (self.bin_dir / "a/data").unlink()
        (self.bin_dir / "b/b.package_manifest.json").unlink()
        self._write_bazel_file("bazel-out/cfg/bin/a/data", "a v2")
        self._write_manifest(
            "bazel-out/cfg/bin/a/a.package_manifest.json",
            {"data": ("a2", "bazel-out/cfg/bin/a/data")},
        )
        os.utime(self.bin_dir / "a/a.package_manifest.json", (2**31, 2**31))
        self.export([a_path, b_path], incremental=True)

        blobs_dir = self.build_dir / bazel_test_packages.EXPORT_DIR / "blobs"
        self.assertEqual(
            sorted(p.name for p in blobs_dir.iterdir()), ["a2", "b1"]
        )
        self.assertEqual(
            self._exported_blob(a_path, "data").read_text(), "a v2"
        )

    def test_reuses_exported_blob_whose_source_is_gone(self) -> None:
        # E.g. a blob from an external repository that a later Bazel
        # invocation didn't plant in the execroot.
        self._write_bazel_file("bazel-out/cfg/bin/pkg/data", "data")
        export_path = self._write_manifest(
            "bazel-out/cfg/bin/pkg/test.package_manifest.json",
            {"data": ("aaaa", "bazel-out/cfg/bin/pkg/data")},
        )
        self.export([export_path])
        (self.bin_dir / "pkg/data").unlink()

        self.export([export_path])

        self.assertEqual(
            self._exported_blob(export_path, "data").read_text(), "data"
        )

    def test_incremental_skips_packages_bazel_did_not_rebuild(self) -> None:
        self._write_bazel_file("bazel-out/cfg/bin/pkg/data", "data")
        export_path = self._write_manifest(
            "bazel-out/cfg/bin/pkg/test.package_manifest.json",
            {"data": ("aaaa", "bazel-out/cfg/bin/pkg/data")},
        )
        self.export([export_path])
        # Simulate a later Bazel invocation that didn't rebuild the package
        # and left one of its blobs' source paths dangling, e.g. because it
        # didn't plant the blob's external repository.
        (self.bin_dir / "pkg/data").unlink()
        (
            self.build_dir / bazel_test_packages.EXPORT_DIR / "blobs/aaaa"
        ).unlink()

        self.export([export_path], incremental=True)

        # Nothing was re-exported, and nothing failed.
        self.assertFalse(
            (
                self.build_dir / bazel_test_packages.EXPORT_DIR / "blobs/aaaa"
            ).exists()
        )

    def test_incremental_reexports_when_export_is_missing(self) -> None:
        self._write_bazel_file("bazel-out/cfg/bin/pkg/data", "data")
        export_path = self._write_manifest(
            "bazel-out/cfg/bin/pkg/test.package_manifest.json",
            {"data": ("aaaa", "bazel-out/cfg/bin/pkg/data")},
        )
        self.export([export_path])
        (self.build_dir / export_path).unlink()

        self.export([export_path], incremental=True)

        self.assertEqual(
            self._exported_blob(export_path, "data").read_text(), "data"
        )

    def test_incremental_exports_rebuilt_packages(self) -> None:
        self._write_bazel_file("bazel-out/cfg/bin/pkg/data", "v1")
        export_path = self._write_manifest(
            "bazel-out/cfg/bin/pkg/test.package_manifest.json",
            {"data": ("v1", "bazel-out/cfg/bin/pkg/data")},
        )
        self.export([export_path])
        (self.bin_dir / "pkg/data").unlink()
        self._write_bazel_file("bazel-out/cfg/bin/pkg/data", "v2")
        self._write_manifest(
            "bazel-out/cfg/bin/pkg/test.package_manifest.json",
            {"data": ("v2", "bazel-out/cfg/bin/pkg/data")},
        )
        # Same size as before, so only the mtime tells the two apart.
        os.utime(
            self.bin_dir / "pkg/test.package_manifest.json", (2**31, 2**31)
        )

        self.export([export_path], incremental=True)

        self.assertEqual(
            self._exported_blob(export_path, "data").read_text(), "v2"
        )

    def test_prunes_manifests_of_unlisted_packages(self) -> None:
        self._write_bazel_file("bazel-out/cfg/bin/a/data", "a")
        self._write_bazel_file("bazel-out/cfg/bin/b/data", "b")
        a_path = self._write_manifest(
            "bazel-out/cfg/bin/a/a.package_manifest.json",
            {"data": ("a1", "bazel-out/cfg/bin/a/data")},
        )
        b_path = self._write_manifest(
            "bazel-out/cfg/bin/b/b.package_manifest.json",
            {"data": ("b1", "bazel-out/cfg/bin/b/data")},
        )
        self.export([a_path, b_path])

        self.export([a_path])

        self.assertTrue((self.build_dir / a_path).is_file())
        self.assertFalse((self.build_dir / b_path).exists())

    def test_copies_across_filesystems(self) -> None:
        lib = self._write_bazel_file(
            "bazel-out/cfg/bin/pkg/lib.so_stripped", "lib"
        )
        export_path = self._write_manifest(
            "bazel-out/cfg/bin/pkg/test.package_manifest.json",
            {"lib/lib.so": ("aaaa", "bazel-out/cfg/bin/pkg/lib.so_stripped")},
        )

        with mock.patch(
            "os.link", side_effect=OSError(errno.EXDEV, "cross-device link")
        ):
            self.export([export_path])

        exported = self._exported_blob(export_path, "lib/lib.so")
        self.assertEqual(exported.read_text(), "lib")
        self.assertFalse(os.path.samefile(exported, lib))

    def test_replaces_exported_blob_with_wrong_size(self) -> None:
        self._write_bazel_file("bazel-out/cfg/bin/pkg/data", "good")
        export_path = self._write_manifest(
            "bazel-out/cfg/bin/pkg/test.package_manifest.json",
            {"data": ("aaaa", "bazel-out/cfg/bin/pkg/data")},
        )
        stale = self.build_dir / bazel_test_packages.EXPORT_DIR / "blobs/aaaa"
        stale.parent.mkdir(parents=True)
        stale.write_text("stale contents")

        self.export([export_path])

        self.assertEqual(
            self._exported_blob(export_path, "data").read_text(), "good"
        )

    def test_does_not_rewrite_unchanged_manifests(self) -> None:
        self._write_bazel_file("bazel-out/cfg/bin/pkg/data", "data")
        export_path = self._write_manifest(
            "bazel-out/cfg/bin/pkg/test.package_manifest.json",
            {"data": ("aaaa", "bazel-out/cfg/bin/pkg/data")},
        )
        self.export([export_path])
        exported_manifest = self.build_dir / export_path
        os.utime(exported_manifest, (1, 1))

        self.export([export_path])

        self.assertEqual(exported_manifest.stat().st_mtime, 1)


if __name__ == "__main__":
    unittest.main()

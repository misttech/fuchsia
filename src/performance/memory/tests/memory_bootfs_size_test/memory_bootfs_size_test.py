# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Enumerate bootfs files in a format suitable to monitoring"""
import argparse
import dataclasses
import json
import os
import struct
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any, Dict, Iterator, List, Self

ZBI_TYPE_CONTAINER = b"BOOT"
ZBI_CONTAINER_MAGIC = b"\xe6\xf7\x8c\x86"
ZBI_ITEM_MAGIC = b"\x29\x17\x78\xb5"
ZBI_TYPE_BOOTLOADER_FILE = b"BTFL"
ZBI_HEADER_SIZE = 32

parser = argparse.ArgumentParser()
parser.add_argument(
    "--zbi_tool", help="Path to the zbi tool binary.", required=True
)
parser.add_argument("--zbi_image", help="Path to the zbi image.", default=None)
parser.add_argument(
    "--zbi_image_fallback", help="Path to the zbi image.", required=True
)
parser.add_argument("--far_tool", help="Path to the far binary.", required=True)

ARGS, sys.argv = parser.parse_known_args(sys.argv)


@dataclasses.dataclass
class ZbiEntry(object):
    """Describes a file stored in Zircon Boot Image.

    Attributes:
        type: category KERNEL or BOOTFS.
        path: path if the file within the archive.
        size: uncompressed size in bytes.
    """

    type: str
    path: str
    size: int


class ZbiReader(object):
    """Ability to introspect a Zircon Boot Image.

    Extracted files are deleted upon context exit.
    """

    def __init__(self, zbi_tool: str, zbi_image: str):
        self.zbi_tool = zbi_tool
        self.zbi_image = zbi_image
        self.tmp_dir = tempfile.TemporaryDirectory()
        self.work_dir = Path(self.tmp_dir.name)

    def __enter__(self) -> Self:
        data = Path(self.zbi_image).read_bytes()
        idx = 0
        while True:
            idx = data.find(ZBI_TYPE_CONTAINER, idx)
            if idx == -1:
                break
            if (
                idx + ZBI_HEADER_SIZE * 2 <= len(data)
                and data[idx + 8 : idx + 12] == ZBI_CONTAINER_MAGIC
                and data[idx + 24 : idx + 28] == ZBI_ITEM_MAGIC
                and data[idx + 32 : idx + 36] != ZBI_TYPE_BOOTLOADER_FILE
            ):
                if idx > 0:
                    length = struct.unpack_from("<I", data, idx + 4)[0]
                    unwrapped = self.work_dir / "unwrapped.zbi"
                    unwrapped.write_bytes(
                        data[idx : idx + ZBI_HEADER_SIZE + length]
                    )
                    self.zbi_image = str(unwrapped)
                break
            idx += 1
        return self

    def __exit__(self, *args: Any) -> None:
        del args
        self.tmp_dir.cleanup()

    def enumerate_contents(self) -> Iterator[ZbiEntry]:
        """Generate entries for each file in the zbi."""
        zbi_list_path = self.work_dir / "zbi_list.json"
        subprocess.check_call(
            [
                self.zbi_tool,
                "--list",
                "--json-output",
                zbi_list_path,
                self.zbi_image,
            ]
        )
        for entry in json.loads(zbi_list_path.read_text()):
            if entry["type"] not in ("BOOTFS", "KERNEL"):
                continue
            for content in entry["contents"]:
                yield ZbiEntry(
                    type=entry["type"],
                    path=content["name"],
                    size=content["size"],
                )

    def extract(self, file_path: str) -> Path:
        """Extract a ZBI file to a temporary location."""
        subprocess.check_call(
            [
                ARGS.zbi_tool,
                "--output-dir",
                self.work_dir,
                "--extract",
                self.zbi_image,
                "--",
                file_path,
            ]
        )
        output_path = self.work_dir / file_path
        assert output_path.exists()
        return output_path


def test_outdir() -> Path:
    """Persisted test output location."""
    path = Path(os.environ["FUCHSIA_TEST_OUTDIR"]).resolve()
    os.makedirs(path, exist_ok=True)
    return path


def find_zbi() -> str:
    if ARGS.zbi_image and os.path.exists(ARGS.zbi_image):
        return ARGS.zbi_image
    pb_zbis = list(Path(".").glob("**/product_bundle/system_a/fuchsia.zbi"))
    if len(pb_zbis) == 1:
        return str(pb_zbis[0])
    if len(pb_zbis) > 1:
        raise ValueError(f"Multiple product bundle ZBIs found: {pb_zbis}")
    if os.path.exists(ARGS.zbi_image_fallback):
        return ARGS.zbi_image_fallback
    raise ValueError(
        "Zbi not found in: {}, **/product_bundle/system_a/fuchsia.zbi, or {}".format(
            ARGS.zbi_image, ARGS.zbi_image_fallback
        )
    )


@dataclasses.dataclass
class PackageEntry(object):
    """Element of a package manifest."""

    name: str
    hash: str


def parse_package_manifest(content: str) -> List[PackageEntry]:
    """Returns element of a package manifest in the form of:

    name=hash
    name=hash
    ...
    """
    return [
        PackageEntry(*line.split("=", maxsplit=1))
        for line in content.splitlines()
        if line
    ]


class PyHostTestWithLibTests(unittest.TestCase):
    def test_resources_available(self) -> None:
        self.assertTrue(
            os.path.exists(ARGS.zbi_tool), f"Missing {ARGS.zbi_tool}"
        )
        self.assertTrue(
            os.path.exists(ARGS.far_tool), f"Missing {ARGS.far_tool}"
        )

    def test_list_zbi(self) -> None:
        with ZbiReader(ARGS.zbi_tool, find_zbi()) as zbi:
            print(f"Read {zbi.zbi_image}")
            # Mapping from a blob file in bootfs to a readable name.
            #
            # It turns `blob/<hash>` into `blob/[pkg:<package_name>]/<file_path>`
            # `*`` is used as package name for blobs shared by multiple packages.
            # The package's manifest shown as `blob/[pkg=<package_name>].manifest`
            blob_path_rewrite: Dict[str, str] = {}

            # Enumerate the packages stored as blob.
            for package in parse_package_manifest(
                zbi.extract("data/bootfs_packages").read_text()
            ):
                # Identify the blob holding the package's content.
                blob_path_rewrite[
                    f"blob/{package.hash}"
                ] = f"blob/[pkg:{package.name}].manifest"

                # Extract and enumerate the package's content.
                package_root_path = zbi.extract(f"blob/{package.hash}")
                package_root_manifest = subprocess.check_output(
                    [
                        ARGS.far_tool,
                        "cat",
                        f"--archive={package_root_path}",
                        f"--file=meta/contents",
                    ]
                ).decode("utf-8")
                for content in parse_package_manifest(package_root_manifest):
                    key = f"blob/{content.hash}"
                    pkg_desc = "*" if key in blob_path_rewrite else package.name
                    blob_path_rewrite[
                        key
                    ] = f"blob/[pkg:{pkg_desc}]/{content.name}"

            bootfs_file_size = [
                dict(
                    path=blob_path_rewrite.get(zbi_entry.path)
                    or zbi_entry.path,
                    size=zbi_entry.size,
                )
                for zbi_entry in zbi.enumerate_contents()
            ]

            output_path = test_outdir() / "bootfs_file_size.freeform.json"
            print("Publish freeform metrics:", output_path)
            with open(output_path, "wt") as file:
                json.dump(bootfs_file_size, file, indent=4)


if __name__ == "__main__":
    unittest.main()

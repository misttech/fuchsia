#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Wraps a ZBI inside an Android boot or vendor_boot image.

This script works in tandem with the boot shim to allow booting Fuchsia on Android
bootloaders.

The ZBI encoding (`--encoding`) and boot image location (`--location`) are configured
independently:

Supported encodings (`--encoding`):
  * `raw` (default): Uses the input ZBI directly as the payload.
  * `bootloader_file`: Wraps the input ZBI in a `ZBI_TYPE_BOOTLOADER_FILE`
    container (named `--bootloader-file-name`) using `--zbi-tool`.

Supported locations (`--location`):
  * `ramdisk` (default): Places `--boot-shim` in the `boot` image kernel and the
    encoded ZBI payload in the `boot` image `ramdisk`.
  * `vendor_ramdisk`: Places the encoded ZBI payload in a `vendor_boot` image
    `vendor_ramdisk` alongside `--dtb`.
  * `kernel_append`: Embeds the encoded ZBI payload into `--boot-shim`'s
    `linuxboot` embedded payload area via `--linuxboot-embed` and places the
    combined image in the `boot` image kernel with an empty `ramdisk`.
"""

import argparse
import pathlib
import shutil
import subprocess
import sys
import tempfile
from typing import Sequence

DEFAULT_BOOTLOADER_FILE_NAME = "fuchsia.zbi"


def encode_zbi(
    zbi: pathlib.Path,
    out: pathlib.Path,
    zbi_tool: pathlib.Path | None,
    bootloader_file_name: str = DEFAULT_BOOTLOADER_FILE_NAME,
) -> None:
    """Wraps `zbi` in a ZBI container with a single ZBI_TYPE_BOOTLOADER_FILE item.

    The `zbi` host tool (`--type=bootloader_file`) creates the ZBI container and item
    headers, alignment padding, and CRC32 checksum, while expecting the input file to
    begin with the 1-byte name length and filename (`| name_len (1B) | name | payload |`).
    """
    if zbi_tool is None:
        raise ValueError(
            "--zbi-tool is required for encoding 'bootloader_file'"
        )
    name_bytes = bootloader_file_name.encode("utf-8")
    if not (0 < len(name_bytes) <= 255):
        raise ValueError(
            f"Bootloader file name must be 1..255 bytes, got {len(name_bytes)}"
        )

    with tempfile.TemporaryDirectory() as temp_dir:
        named_payload_path = pathlib.Path(temp_dir) / "bootloader_file.bin"
        with named_payload_path.open("wb") as out_f, zbi.open("rb") as in_f:
            out_f.write(bytes([len(name_bytes)]) + name_bytes)
            shutil.copyfileobj(in_f, out_f)

        # Resolve so a bare filename in the cwd (e.g. "zbi") is not looked up on $PATH.
        subprocess.run(
            [
                zbi_tool.resolve(),
                "--output",
                out,
                "--type=bootloader_file",
                named_payload_path,
            ],
            check=True,
        )


def embed_zbi_in_boot_shim(
    linuxboot_embed: pathlib.Path | None,
    boot_shim: pathlib.Path | None,
    zbi: pathlib.Path,
    out: pathlib.Path,
) -> None:
    """Embeds `zbi` into `boot_shim` using `linuxboot-embed.py`."""
    if linuxboot_embed is None:
        raise ValueError(
            "--linuxboot-embed is required for location 'kernel_append'"
        )
    if boot_shim is None:
        raise ValueError("--boot-shim is required for location 'kernel_append'")

    subprocess.run(
        [
            sys.executable,
            linuxboot_embed,
            "--shim",
            boot_shim,
            "--output",
            out,
            zbi,
        ],
        check=True,
    )


def create_android_boot_image(
    mkbootimg: pathlib.Path,
    boot_shim: pathlib.Path | None,
    out: pathlib.Path,
    zbi: pathlib.Path | None = None,
) -> None:
    """Creates an Android `boot` v4 image."""
    if boot_shim is None:
        raise ValueError("--boot-shim is required to create a boot image")

    ramdisk_args: list[str | pathlib.Path] = (
        ["--ramdisk", zbi] if zbi is not None else []
    )
    subprocess.run(
        [
            sys.executable,
            mkbootimg,
            "--kernel",
            boot_shim,
            *ramdisk_args,
            "--header_version",
            "4",
            "--pagesize",
            "4096",
            "--output",
            out,
        ],
        check=True,
    )


def create_android_vendor_boot_image(
    mkbootimg: pathlib.Path,
    dtb: pathlib.Path | None,
    zbi: pathlib.Path,
    out: pathlib.Path,
) -> None:
    """Creates an Android `vendor_boot` v4 image with `zbi` in `vendor_ramdisk`."""
    if dtb is None:
        raise ValueError("--dtb is required to create a vendor_boot image")

    subprocess.run(
        [
            sys.executable,
            mkbootimg,
            "--dtb",
            dtb,
            "--vendor_ramdisk",
            zbi,
            "--header_version",
            "4",
            "--pagesize",
            "4096",
            "--vendor_boot",
            out,
        ],
        check=True,
    )


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--location",
        choices=("ramdisk", "vendor_ramdisk", "kernel_append"),
        default="ramdisk",
        help="Where to place the ZBI payload in the Android boot image (default: ramdisk)",
    )
    parser.add_argument(
        "--encoding",
        choices=("raw", "bootloader_file"),
        default="raw",
        help="How to encode the ZBI payload before packing (default: raw)",
    )
    parser.add_argument(
        "--mkbootimg",
        required=True,
        type=pathlib.Path,
        help="Path to Android `mkbootimg` Python tool",
    )
    parser.add_argument(
        "--zbi-tool",
        type=pathlib.Path,
        help="Path to `zbi` host tool (required for --encoding=bootloader_file)",
    )
    parser.add_argument(
        "--linuxboot-embed",
        type=pathlib.Path,
        help="Path to `linuxboot-embed.py` (required for --location=kernel_append)",
    )
    parser.add_argument(
        "--boot-shim",
        type=pathlib.Path,
        help="Path to boot shim (required for --location=ramdisk and --location=kernel_append)",
    )
    parser.add_argument(
        "--dtb",
        type=pathlib.Path,
        help="Path to DTB (required for --location=vendor_ramdisk)",
    )
    parser.add_argument(
        "--bootloader-file-name",
        default=DEFAULT_BOOTLOADER_FILE_NAME,
        help=f"Filename inside ZBI_TYPE_BOOTLOADER_FILE container (default: {DEFAULT_BOOTLOADER_FILE_NAME})",
    )
    parser.add_argument(
        "-z", "--zbi", required=True, type=pathlib.Path, help="Path to ZBI"
    )
    parser.add_argument(
        "-o",
        "--output",
        required=True,
        type=pathlib.Path,
        help="Path to output file",
    )

    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> None:
    args = parse_args(argv)

    with tempfile.TemporaryDirectory() as temp_dir:
        temp_path = pathlib.Path(temp_dir)

        if args.encoding == "raw":
            zbi = args.zbi
        elif args.encoding == "bootloader_file":
            zbi = temp_path / "bootloader_file_container.zbi"
            encode_zbi(
                zbi=args.zbi,
                out=zbi,
                zbi_tool=args.zbi_tool,
                bootloader_file_name=args.bootloader_file_name,
            )
        else:
            raise ValueError(f"Unsupported encoding: {args.encoding}")

        if args.location == "ramdisk":
            create_android_boot_image(
                mkbootimg=args.mkbootimg,
                boot_shim=args.boot_shim,
                out=args.output,
                zbi=zbi,
            )
        elif args.location == "kernel_append":
            embedded_shim = temp_path / "embedded_shim.bin"
            embed_zbi_in_boot_shim(
                linuxboot_embed=args.linuxboot_embed,
                boot_shim=args.boot_shim,
                zbi=zbi,
                out=embedded_shim,
            )
            create_android_boot_image(
                mkbootimg=args.mkbootimg,
                boot_shim=embedded_shim,
                out=args.output,
            )
        elif args.location == "vendor_ramdisk":
            create_android_vendor_boot_image(
                mkbootimg=args.mkbootimg,
                dtb=args.dtb,
                zbi=zbi,
                out=args.output,
            )
        else:
            raise ValueError(f"Unsupported location: {args.location}")


if __name__ == "__main__":
    main()

#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for zbi_to_android_boot_image.py."""

import pathlib
import tempfile
import unittest
from unittest import mock

import zbi_to_android_boot_image


class ZbiToAndroidBootImageTests(unittest.TestCase):
    def test_encode_zbi(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = pathlib.Path(temp_dir)
            zbi_tool = temp_path / "zbi"
            zbi_path = temp_path / "input.zbi"
            out_path = temp_path / "output.zbi"

            payload = b"test_zbi_payload_123"
            zbi_path.write_bytes(payload)

            captured_input: bytes | None = None

            def fake_run(cmd: list[object], check: bool) -> None:
                nonlocal captured_input
                self.assertTrue(check)
                self.assertEqual(cmd[0], zbi_tool.resolve())
                self.assertEqual(cmd[1], "--output")
                self.assertEqual(cmd[2], out_path)
                self.assertEqual(cmd[3], "--type=bootloader_file")
                named_payload_path = cmd[4]
                assert isinstance(named_payload_path, pathlib.Path)
                captured_input = named_payload_path.read_bytes()

            with mock.patch("subprocess.run", side_effect=fake_run):
                zbi_to_android_boot_image.encode_zbi(
                    zbi=zbi_path,
                    out=out_path,
                    zbi_tool=zbi_tool,
                    bootloader_file_name="fuchsia.zbi",
                )

            expected_input = (
                bytes([len(b"fuchsia.zbi")]) + b"fuchsia.zbi" + payload
            )
            self.assertEqual(captured_input, expected_input)

    def test_encode_zbi_invalid_bootloader_file_name_fails(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            temp_path = pathlib.Path(temp_dir)
            with self.assertRaises(ValueError):
                zbi_to_android_boot_image.encode_zbi(
                    zbi=temp_path / "in.zbi",
                    out=temp_path / "out.zbi",
                    zbi_tool=temp_path / "zbi",
                    bootloader_file_name="",
                )
            with self.assertRaises(ValueError):
                zbi_to_android_boot_image.encode_zbi(
                    zbi=temp_path / "in.zbi",
                    out=temp_path / "out.zbi",
                    zbi_tool=temp_path / "zbi",
                    bootloader_file_name="a" * 256,
                )

    @mock.patch("zbi_to_android_boot_image.create_android_vendor_boot_image")
    @mock.patch("zbi_to_android_boot_image.create_android_boot_image")
    @mock.patch("zbi_to_android_boot_image.embed_zbi_in_boot_shim")
    @mock.patch("zbi_to_android_boot_image.encode_zbi")
    def test_main_ramdisk_defaults(
        self,
        mock_encode_zbi: mock.MagicMock,
        mock_embed_zbi: mock.MagicMock,
        mock_create_boot: mock.MagicMock,
        mock_create_vendor_boot: mock.MagicMock,
    ) -> None:
        zbi_to_android_boot_image.main(
            [
                "--mkbootimg",
                "mkbootimg",
                "--boot-shim",
                "shim.bin",
                "-z",
                "fuchsia.zbi",
                "-o",
                "boot.img",
            ]
        )

        mock_encode_zbi.assert_not_called()
        mock_embed_zbi.assert_not_called()
        mock_create_boot.assert_called_once_with(
            mkbootimg=pathlib.Path("mkbootimg"),
            boot_shim=pathlib.Path("shim.bin"),
            out=pathlib.Path("boot.img"),
            zbi=pathlib.Path("fuchsia.zbi"),
        )
        mock_create_vendor_boot.assert_not_called()

    @mock.patch("zbi_to_android_boot_image.create_android_vendor_boot_image")
    @mock.patch("zbi_to_android_boot_image.create_android_boot_image")
    @mock.patch("zbi_to_android_boot_image.embed_zbi_in_boot_shim")
    @mock.patch("zbi_to_android_boot_image.encode_zbi")
    def test_main_kernel_append_bootloader_file(
        self,
        mock_encode_zbi: mock.MagicMock,
        mock_embed_zbi: mock.MagicMock,
        mock_create_boot: mock.MagicMock,
        mock_create_vendor_boot: mock.MagicMock,
    ) -> None:
        zbi_to_android_boot_image.main(
            [
                "--location",
                "kernel_append",
                "--encoding",
                "bootloader_file",
                "--mkbootimg",
                "mkbootimg",
                "--zbi-tool",
                "zbi",
                "--linuxboot-embed",
                "linuxboot-embed.py",
                "--boot-shim",
                "shim.bin",
                "-z",
                "fuchsia.zbi",
                "-o",
                "boot.img",
            ]
        )

        mock_encode_zbi.assert_called_once_with(
            zbi=pathlib.Path("fuchsia.zbi"),
            out=mock.ANY,
            zbi_tool=pathlib.Path("zbi"),
            bootloader_file_name="fuchsia.zbi",
        )
        encoded_zbi = mock_encode_zbi.call_args.kwargs["out"]

        mock_embed_zbi.assert_called_once_with(
            linuxboot_embed=pathlib.Path("linuxboot-embed.py"),
            boot_shim=pathlib.Path("shim.bin"),
            zbi=encoded_zbi,
            out=mock.ANY,
        )
        embedded_shim = mock_embed_zbi.call_args.kwargs["out"]

        mock_create_boot.assert_called_once_with(
            mkbootimg=pathlib.Path("mkbootimg"),
            boot_shim=embedded_shim,
            out=pathlib.Path("boot.img"),
        )
        mock_create_vendor_boot.assert_not_called()

    @mock.patch("zbi_to_android_boot_image.create_android_vendor_boot_image")
    @mock.patch("zbi_to_android_boot_image.create_android_boot_image")
    @mock.patch("zbi_to_android_boot_image.embed_zbi_in_boot_shim")
    @mock.patch("zbi_to_android_boot_image.encode_zbi")
    def test_main_vendor_ramdisk_bootloader_file(
        self,
        mock_encode_zbi: mock.MagicMock,
        mock_embed_zbi: mock.MagicMock,
        mock_create_boot: mock.MagicMock,
        mock_create_vendor_boot: mock.MagicMock,
    ) -> None:
        zbi_to_android_boot_image.main(
            [
                "--location",
                "vendor_ramdisk",
                "--encoding",
                "bootloader_file",
                "--mkbootimg",
                "mkbootimg",
                "--zbi-tool",
                "zbi",
                "--dtb",
                "board.dtb",
                "-z",
                "recovery.zbi",
                "-o",
                "vendor_boot.img",
            ]
        )

        mock_encode_zbi.assert_called_once_with(
            zbi=pathlib.Path("recovery.zbi"),
            out=mock.ANY,
            zbi_tool=pathlib.Path("zbi"),
            bootloader_file_name="fuchsia.zbi",
        )
        encoded_zbi = mock_encode_zbi.call_args.kwargs["out"]

        mock_embed_zbi.assert_not_called()
        mock_create_boot.assert_not_called()
        mock_create_vendor_boot.assert_called_once_with(
            mkbootimg=pathlib.Path("mkbootimg"),
            dtb=pathlib.Path("board.dtb"),
            zbi=encoded_zbi,
            out=pathlib.Path("vendor_boot.img"),
        )


if __name__ == "__main__":
    unittest.main()

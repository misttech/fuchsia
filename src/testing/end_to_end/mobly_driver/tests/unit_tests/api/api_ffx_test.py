# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for Mobly driver's api_ffx.py."""

import ipaddress
import subprocess
import unittest
from typing import Any
from unittest.mock import patch

from mobly_driver.api import api_ffx
from parameterized import parameterized


class FfxClientTest(unittest.TestCase):
    """Tests for api_ffx.FfxClient"""

    def setUp(self) -> None:
        super().setUp()
        self.client = api_ffx.FfxClient("some_ffx_path")

    @parameterized.expand(
        [
            (
                "No default nodes",
                b'[{"nodename": "dut", "is_default": false}]',
                ["dut"],
                [],
            ),
            (
                "1 default node",
                b'[{"nodename": "dut", "is_default": true}]',
                ["dut"],
                ["dut"],
            ),
        ]
    )
    @patch("subprocess.check_output", autospec=True)
    def test_target_list_success(
        self,
        unused_name: str,
        target_list_output: bytes,
        want_all_nodes: list[str],
        want_default_nodes: list[str],
        mock_check_output: Any,
    ) -> None:
        """Test case for target_list() returning expected results"""
        mock_check_output.return_value = target_list_output
        res = self.client.target_list()
        self.assertEqual(res.all_nodes, want_all_nodes)
        self.assertEqual(res.default_nodes, want_default_nodes)

    @parameterized.expand(
        [
            (
                "timeout",
                subprocess.TimeoutExpired(cmd="", timeout=-1),
            ),
            (
                "failure",
                subprocess.CalledProcessError(returncode=1, cmd=[], stderr=""),
            ),
        ]
    )
    @patch("subprocess.check_output", autospec=True)
    def test_target_list_command_failure_raises_exception(
        self, unused_name: str, mock_exception: Any, mock_check_output: Any
    ) -> None:
        """Test case for exception being raised from subprocess failure"""
        mock_check_output.side_effect = mock_exception
        with self.assertRaises(api_ffx.CommandException):
            self.client.target_list()

    @patch(
        "subprocess.check_output",
        autospec=True,
        return_value=b'[{"nodename": "dut", "is_default": false}]',
    )
    def test_target_list_with_ssh_auth_sock(
        self, mock_check_output: Any
    ) -> None:
        """Test case for ssh_auth_sock being included in ffx command"""
        client = api_ffx.FfxClient("some_ffx_path", ssh_auth_sock="/tmp/sock")
        client.target_list()
        check_output_args = mock_check_output.call_args.args[0]
        self.assertIn("-c", check_output_args)
        self.assertIn("ssh.auth-sock=/tmp/sock", check_output_args)

    @patch(
        "subprocess.check_output",
        autospec=True,
        return_value=b'[{"nodename": "dut", "is_default": false}]',
    )
    def test_target_list_with_identities_only(
        self, mock_check_output: Any
    ) -> None:
        """Test case for identities_only being included in ffx command"""
        client = api_ffx.FfxClient("some_ffx_path", identities_only=True)
        client.target_list()
        check_output_args = mock_check_output.call_args.args[0]
        self.assertIn("-c", check_output_args)
        self.assertIn("ssh.identities-only=true", check_output_args)

    @parameterized.expand(
        [
            ("Invalid JSON str", b""),
            ("Empty device JSON str", b"[{}]"),
        ]
    )
    @patch("subprocess.check_output", autospec=True)
    def test_target_list_invalid_output_raises_exception(
        self,
        unused_name: str,
        target_list_output: bytes,
        mock_check_output: Any,
    ) -> None:
        """Test case for exception being raised from invalid discovery output"""
        mock_check_output.return_value = target_list_output
        with self.assertRaises(api_ffx.OutputFormatException):
            self.client.target_list()

    @patch("subprocess.check_output", autospec=True)
    def test_get_target_serial(self, mock_check_output: Any) -> None:
        """Test case for get_target_serial() returning expected results"""
        mock_check_output.return_value = b"0123456789ABCDEF\n"

        self.assertEqual(
            self.client.get_target_serial(
                target_name="fuchsia-emulator",
            ),
            "0123456789ABCDEF",
        )

        # `target_name` must be a positional filter. Passing it via the
        # global `-t` flag only selects ffx's default target and does not
        # filter `target list` output, so with more than one device attached
        # every device's serial would be returned.
        mock_check_output.assert_called_once_with(
            [
                "some_ffx_path",
                "target",
                "list",
                "--format",
                "serials",
                "fuchsia-emulator",
            ],
            timeout=15,
        )
        self.assertNotIn("-t", mock_check_output.call_args.args[0])

    @patch("subprocess.check_output", return_value=b"", autospec=True)
    def test_get_target_serial_no_serial_returns_none(
        self, mock_check_output: Any
    ) -> None:
        """Test case for get_target_serial() returning None when the target
        reports no serial number"""
        self.assertIsNone(
            self.client.get_target_serial(
                target_name="fuchsia-emulator",
            )
        )
        mock_check_output.assert_called()

    @parameterized.expand(
        [
            (
                "timeout",
                subprocess.TimeoutExpired(cmd="", timeout=-1),
            ),
            (
                # ffx exits 2 when a supplied nodename cannot be resolved.
                "failure",
                subprocess.CalledProcessError(returncode=2, cmd=[], stderr=""),
            ),
        ]
    )
    @patch("subprocess.check_output", autospec=True)
    def test_get_target_serial_failure_raises_exception(
        self, unused_name: str, mock_exception: Any, mock_check_output: Any
    ) -> None:
        """Test case for get_target_serial() raising exceptions for
        subprocess failure"""
        mock_check_output.side_effect = mock_exception
        with self.assertRaises(api_ffx.CommandException):
            self.client.get_target_serial(
                target_name="fuchsia-emulator",
            )
        mock_check_output.assert_called()

    @patch(
        "subprocess.check_output",
        autospec=True,
    )
    def test_get_target_ssh_address(self, mock_check_output: Any) -> None:
        """Test case for get_target_ssh_address() returning expected results"""
        ssh_ip = "fe80::4fce:3102:ef13:888c%qemu"
        ssh_port = 8022
        mock_check_output.return_value = f"[{ssh_ip}]:{ssh_port}".encode()

        expected_target_ssh_address = api_ffx.TargetSshAddress(
            ip=ipaddress.IPv6Address(ssh_ip), port=ssh_port
        )

        self.assertEqual(
            self.client.get_target_ssh_address(
                target_name="fuchsia-emulator",
            ),
            expected_target_ssh_address,
        )

        mock_check_output.assert_called()

    @parameterized.expand(
        [
            (
                "timeout",
                subprocess.TimeoutExpired(cmd="", timeout=-1),
            ),
            (
                "failure",
                subprocess.CalledProcessError(returncode=1, cmd=[], stderr=""),
            ),
        ]
    )
    @patch("subprocess.check_output", autospec=True)
    def test_get_target_ssh_address_failure_raises_exception(
        self, unused_name: str, mock_exception: Any, mock_check_output: Any
    ) -> None:
        """Test case for get_target_ssh_address() raising exceptions for
        subprocess failure"""
        mock_check_output.side_effect = mock_exception
        with self.assertRaises(api_ffx.CommandException):
            self.client.get_target_ssh_address(
                target_name="fuchsia-emulator",
            )
        mock_check_output.assert_called()

    @patch(
        "subprocess.check_output",
        return_value=b"some invalid output",
        autospec=True,
    )
    def test_get_target_ssh_address_invalid_output_raises_exception(
        self, mock_check_output: Any
    ) -> None:
        """Test case for get_target_ssh_address raising exception for invalid
        output"""
        with self.assertRaises(api_ffx.OutputFormatException):
            self.client.get_target_ssh_address(
                target_name="fuchsia-emulator",
            )
        mock_check_output.assert_called()

    @patch(
        "subprocess.check_output",
        autospec=True,
    )
    def test_get_target_usb_address(self, mock_check_output: Any) -> None:
        """Test case for get_target_usb_address() returning expected results"""
        mock_check_output.return_value = b"usb:cid:42\n"

        expected_target_usb_address = api_ffx.TargetUsbAddress(target_id=42)

        self.assertEqual(
            self.client.get_target_usb_address(
                target_name="fuchsia-emulator",
            ),
            expected_target_usb_address,
        )
        self.assertEqual(str(expected_target_usb_address), "usb:cid:42")

        mock_check_output.assert_called_once_with(
            [
                "some_ffx_path",
                "target",
                "list",
                "--allow-addrs",
                "usb",
                "--format",
                "addresses-with-scope",
                "fuchsia-emulator",
            ],
            timeout=15,
        )

    @parameterized.expand(
        [
            (
                "timeout",
                subprocess.TimeoutExpired(cmd="", timeout=-1),
            ),
            (
                "failure",
                subprocess.CalledProcessError(returncode=1, cmd=[], stderr=""),
            ),
        ]
    )
    @patch("subprocess.check_output", autospec=True)
    def test_get_target_usb_address_failure_raises_exception(
        self, unused_name: str, mock_exception: Any, mock_check_output: Any
    ) -> None:
        """Test case for get_target_usb_address() raising exceptions for
        subprocess failure"""
        mock_check_output.side_effect = mock_exception
        with self.assertRaises(api_ffx.CommandException):
            self.client.get_target_usb_address(
                target_name="fuchsia-emulator",
            )
        mock_check_output.assert_called()

    @parameterized.expand(
        [
            ("invalid_prefix", b"some invalid output"),
            ("non_integer_cid", b"usb:cid:abc"),
            ("negative_cid", b"usb:cid:-1"),
        ]
    )
    @patch("subprocess.check_output", autospec=True)
    def test_get_target_usb_address_invalid_output_raises_exception(
        self,
        unused_name: str,
        invalid_output: bytes,
        mock_check_output: Any,
    ) -> None:
        """Test case for get_target_usb_address raising exception for invalid
        output"""
        mock_check_output.return_value = invalid_output
        with self.assertRaises(api_ffx.OutputFormatException):
            self.client.get_target_usb_address(
                target_name="fuchsia-emulator",
            )
        mock_check_output.assert_called()

    @parameterized.expand(
        [
            (
                "::1",
                api_ffx._REMOTE_TARGET_SSH_PORT,
                True,
            ),
            (
                "fe80::6f01:a7e5:3e79:ceec",
                api_ffx._REMOTE_TARGET_SSH_PORT,
                False,
            ),
            (
                "::1",
                80,
                False,
            ),
            (
                "127.0.0.1",
                api_ffx._REMOTE_TARGET_SSH_PORT,
                True,
            ),
            (
                "192.168.1.1",
                api_ffx._REMOTE_TARGET_SSH_PORT,
                False,
            ),
            (
                "127.0.0.1",
                80,
                False,
            ),
        ]
    )
    def test_target_ssh_address(
        self, ip: str, port: int, is_remote: bool
    ) -> None:
        """Test case for TargetSshAddress dataclass"""
        target_ssh_address = api_ffx.TargetSshAddress(
            ip=ipaddress.ip_address(ip), port=port
        )
        self.assertEqual(target_ssh_address.is_remote(), is_remote)

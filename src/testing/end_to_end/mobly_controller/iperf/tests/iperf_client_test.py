# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import logging
import os
import subprocess
import unittest
from unittest import mock

from iperf import iperf_client
from iperf.iperf_client import (
    IPerfClientBase,
    IPerfClientOverSsh,
)
from libs.types import ControllerConfig

ARGS = 0
KWARGS = 1
MOCK_LOGFILE_PATH = "/path/to/foo"


class IPerfClientModuleTest(unittest.TestCase):
    """Tests the iperf.iperf_client module functions."""

    @mock.patch("iperf.iperf_client.SSHProvider")
    def test_create_can_create_client_over_ssh(
        self, _mock_ssh_provider: mock.Mock
    ) -> None:
        cfg: ControllerConfig = {
            "ssh_config": {
                "user": "root",
                "host": "192.168.42.11",
                "identity_file": "/dev/null",
            },
            "test_interface": "wlan0",
            "sync_date": False,
        }
        clients = iperf_client.create([cfg])
        self.assertEqual(len(clients), 1)
        self.assertIsInstance(clients[0], IPerfClientOverSsh)
        self.assertEqual(clients[0].test_interface, "wlan0")

    def test_create_raises_value_error_without_ssh_config(self) -> None:
        with self.assertRaises(ValueError):
            iperf_client.create([{}])


class IPerfClientBaseTest(unittest.TestCase):
    """Tests iperf.iperf_client.IPerfClientBase."""

    @mock.patch("os.makedirs")
    def test_get_full_file_path_creates_parent_directory(
        self, mock_makedirs: mock.Mock
    ) -> None:
        with mock.patch.object(
            logging,
            "log_path",
            "/tmp/unit_test_garbage",
            create=True,
        ):
            full_file_path = IPerfClientBase._get_full_file_path("0")

        self.assertTrue(
            mock_makedirs.called, "Did not attempt to create a directory."
        )
        self.assertEqual(
            os.path.dirname(full_file_path),
            mock_makedirs.call_args[ARGS][0],
            "The parent directory of the full file path was not created.",
        )


class IPerfClientOverSshTest(unittest.TestCase):
    """Tests iperf.iperf_client.IPerfClientOverSsh."""

    @mock.patch("builtins.open")
    def test_start_writes_output_to_full_file_path(
        self,
        mock_open: mock.Mock,
    ) -> None:
        mock_ssh_provider = mock.Mock()
        mock_ssh_provider.run.return_value = subprocess.CompletedProcess(
            args=["iperf3"],
            returncode=0,
            stdout=b"iperf test output",
        )

        client = IPerfClientOverSsh(mock_ssh_provider, sync_date=False)

        file_path = "/path/to/foo"
        with mock.patch.object(
            client, "_get_full_file_path", return_value=file_path
        ):
            client.start("127.0.0.1", "IPERF_ARGS", "TAG")
        mock_open.assert_called_with(file_path, "wb")
        mock_open().__enter__().write.assert_called_with(b"iperf test output")

    def test_ssh_client_timeout_subprocess(self) -> None:
        mock_ssh = mock.Mock()
        mock_ssh.run.side_effect = subprocess.TimeoutExpired(
            cmd="iperf3", timeout=10
        )
        client = IPerfClientOverSsh(mock_ssh, sync_date=False)
        with mock.patch.object(
            client, "_get_full_file_path", return_value=MOCK_LOGFILE_PATH
        ):
            with self.assertRaises(TimeoutError) as cm:
                client.start("192.168.1.1", "-t 10", "tag1", timeout=10)
            self.assertIn("Command execution timed out", str(cm.exception))
            self.assertIsInstance(
                cm.exception.__cause__, subprocess.TimeoutExpired
            )

    def test_ssh_client_timeout_error(self) -> None:
        mock_ssh = mock.Mock()
        mock_ssh.run.side_effect = TimeoutError("custom timeout")
        client = IPerfClientOverSsh(mock_ssh, sync_date=False)
        with mock.patch.object(
            client, "_get_full_file_path", return_value=MOCK_LOGFILE_PATH
        ):
            with self.assertRaises(TimeoutError) as cm:
                client.start("192.168.1.1", "-t 10", "tag1", timeout=10)
            self.assertEqual(str(cm.exception), "custom timeout")
            self.assertIsNone(cm.exception.__cause__)

    @mock.patch("builtins.open", new_callable=mock.mock_open)
    def test_ssh_client_non_zero_exit_writes_output(
        self, mock_file: mock.Mock
    ) -> None:
        mock_ssh = mock.Mock()
        mock_ssh.run.return_value = subprocess.CompletedProcess(
            args=["iperf3"],
            returncode=1,
            stdout=b'{"error": "the server is busy running a test"}',
        )
        client = IPerfClientOverSsh(mock_ssh, sync_date=False)
        with mock.patch.object(
            client, "_get_full_file_path", return_value=MOCK_LOGFILE_PATH
        ):
            out_path = client.start("192.168.1.1", "-t 10", "tag1", timeout=10)
            self.assertEqual(out_path, MOCK_LOGFILE_PATH)
            mock_ssh.run.assert_called_once_with(
                "iperf3 -c 192.168.1.1 -t 10",
                timeout_sec=10,
            )
            mock_file().write.assert_called_once_with(
                b'{"error": "the server is busy running a test"}'
            )

    @mock.patch("iperf.iperf_client.logging.warning")
    @mock.patch("builtins.open", new_callable=mock.mock_open)
    def test_ssh_client_called_process_error_writes_output(
        self, mock_file: mock.Mock, mock_warning: mock.Mock
    ) -> None:
        mock_ssh = mock.Mock()
        mock_ssh.run.side_effect = subprocess.CalledProcessError(
            returncode=1,
            cmd="iperf3",
            output=b'{"error": "the server is busy running a test"}',
        )
        client = IPerfClientOverSsh(mock_ssh, sync_date=False)
        with mock.patch.object(
            client, "_get_full_file_path", return_value=MOCK_LOGFILE_PATH
        ):
            out_path = client.start("192.168.1.1", "-t 10", "tag1", timeout=10)
            self.assertEqual(out_path, MOCK_LOGFILE_PATH)
            mock_ssh.run.assert_called_once_with(
                "iperf3 -c 192.168.1.1 -t 10",
                timeout_sec=10,
            )
            mock_warning.assert_called_once()
            mock_file().write.assert_called_once_with(
                b'{"error": "the server is busy running a test"}'
            )

    def test_ssh_client_unexpected_exception_propagates(self) -> None:
        mock_ssh = mock.Mock()
        mock_ssh.run.side_effect = RuntimeError("transport failure")
        client = IPerfClientOverSsh(mock_ssh, sync_date=False)
        with mock.patch.object(
            client, "_get_full_file_path", return_value=MOCK_LOGFILE_PATH
        ):
            with self.assertRaises(RuntimeError) as cm:
                client.start("192.168.1.1", "-t 10", "tag1", timeout=10)
            self.assertIn("transport failure", str(cm.exception))

    @mock.patch("builtins.open", new_callable=mock.mock_open)
    def test_ssh_client_none_stdout_writes_empty_bytes(
        self, mock_file: mock.Mock
    ) -> None:
        mock_ssh = mock.Mock()
        mock_ssh.run.return_value = subprocess.CompletedProcess(
            args=["iperf3"],
            returncode=0,
            stdout=None,
        )
        client = IPerfClientOverSsh(mock_ssh, sync_date=False)
        with mock.patch.object(
            client, "_get_full_file_path", return_value=MOCK_LOGFILE_PATH
        ):
            client.start("192.168.1.1", "-t 10", "tag1", timeout=10)
            mock_file().write.assert_called_once_with(b"")

    @mock.patch("builtins.open", new_callable=mock.mock_open)
    def test_ssh_client_none_called_process_error_writes_empty_bytes(
        self, mock_file: mock.Mock
    ) -> None:
        mock_ssh = mock.Mock()
        mock_ssh.run.side_effect = subprocess.CalledProcessError(
            returncode=1,
            cmd="iperf3",
            output=None,
        )
        client = IPerfClientOverSsh(mock_ssh, sync_date=False)
        with mock.patch.object(
            client, "_get_full_file_path", return_value=MOCK_LOGFILE_PATH
        ):
            client.start("192.168.1.1", "-t 10", "tag1", timeout=10)
            mock_file().write.assert_called_once_with(b"")


if __name__ == "__main__":
    unittest.main()

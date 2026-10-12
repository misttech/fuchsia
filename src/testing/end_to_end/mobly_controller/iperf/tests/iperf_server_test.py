# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

import json
import os
import unittest
from unittest import mock

from iperf import iperf_server
from iperf.iperf_server import (
    IPerfResult,
    IPerfServer,
    IPerfServerOverSsh,
    _get_port_from_ss_output,
)
from libs.ssh import connection, settings
from libs.types import ControllerConfig
from mobly import logger

MOCK_LOGFILE_PATH = "/tmp/mock_iperf_log.log"


class IPerfServerModuleTest(unittest.TestCase):
    """Tests the iperf_server module factory and port detection functions."""

    def test_create_creates_local_iperf_server_with_int(self) -> None:
        servers = iperf_server.create([5201])  # type: ignore[list-item]
        self.assertEqual(len(servers), 1)
        self.assertIsInstance(servers[0], IPerfServer)
        self.assertEqual(servers[0].port, 5201)

    def test_create_creates_local_iperf_server_with_str(self) -> None:
        servers = iperf_server.create(["5201"])  # type: ignore[list-item]
        self.assertEqual(len(servers), 1)
        self.assertIsInstance(servers[0], IPerfServer)
        self.assertEqual(servers[0].port, 5201)

    def test_create_cannot_create_local_iperf_server_with_bad_str(self) -> None:
        with self.assertRaises(ValueError):
            iperf_server.create(["not_a_port"])  # type: ignore[list-item]

    @mock.patch("libs.ssh.connection.SshConnection")
    @mock.patch("antlion.utils.get_interface_based_on_ip", return_value="eth1")
    @mock.patch(
        "libs.commands.command.LinuxCommand.available", return_value=False
    )
    def test_create_creates_server_over_ssh_with_ssh_config(
        self, _mock_cmd: mock.Mock, _mock_net: mock.Mock, _mock_ssh: mock.Mock
    ) -> None:
        cfg: ControllerConfig = {
            "ssh_config": {
                "user": "root",
                "host": "192.168.1.1",
                "identity_file": "/dev/null",
            },
            "port": 5201,
            "test_interface": "lan",
        }
        servers = iperf_server.create([cfg])
        self.assertEqual(len(servers), 1)
        server = servers[0]
        self.assertIsInstance(server, IPerfServerOverSsh)
        assert isinstance(server, IPerfServerOverSsh)
        self.assertEqual(server.port, 5201)
        self.assertEqual(server.test_interface, "lan")

    @mock.patch("libs.ssh.connection.SshConnection")
    @mock.patch("antlion.utils.get_interface_based_on_ip", return_value="eth1")
    @mock.patch(
        "libs.commands.command.LinuxCommand.available", return_value=False
    )
    def test_create_creates_server_over_ssh_with_default_port(
        self, _mock_cmd: mock.Mock, _mock_net: mock.Mock, _mock_ssh: mock.Mock
    ) -> None:
        cfg: ControllerConfig = {
            "ssh_config": {
                "user": "root",
                "host": "192.168.1.1",
                "identity_file": "/dev/null",
            },
            "test_interface": "lan",
        }
        servers = iperf_server.create([cfg])
        self.assertEqual(len(servers), 1)
        server = servers[0]
        self.assertIsInstance(server, IPerfServerOverSsh)
        assert isinstance(server, IPerfServerOverSsh)
        self.assertEqual(server.port, 5201)
        self.assertEqual(server.test_interface, "lan")

    def test_create_raises_value_error_on_invalid_config(self) -> None:
        with self.assertRaises(ValueError):
            iperf_server.create([{"invalid_key": "val"}])

    def test_get_port_from_ss_output_returns_correct_port_ipv4(self) -> None:
        ss_output = (
            "tcp LISTEN  0 5 127.0.0.1:5201  *:*"
            ' users:(("iperf3",pid=1234,fd=3))\n'
        )
        self.assertEqual(_get_port_from_ss_output(ss_output, 1234), "5201")

    def test_get_port_from_ss_output_returns_correct_port_ipv6(self) -> None:
        ss_output = (
            "tcp LISTEN  0 5 [::]:5201  *:*"
            ' users:(("iperf3",pid=1234,fd=3))\n'
        )
        self.assertEqual(_get_port_from_ss_output(ss_output, 1234), "5201")

    def test_get_port_from_ss_output_raises_when_not_found(self) -> None:
        ss_output = 'tcp LISTEN  0 5 127.0.0.1:5201  *:* users:(("iperf3",pid=9999,fd=3))\n'
        with self.assertRaises(ProcessLookupError):
            _get_port_from_ss_output(ss_output, 1234)

    def test_destroy_calls_stop_and_close(self) -> None:
        mock_server = mock.create_autospec(IPerfServerOverSsh)
        iperf_server.destroy([mock_server])
        mock_server.stop.assert_called_once()
        mock_server.close_ssh.assert_called_once()


class IPerfResultTest(unittest.TestCase):
    """Tests IPerfResult parsing."""

    def test_parse_valid_json(self) -> None:
        json_data = (
            '{"end": {"sum_received": {"bits_per_second": 8000000},'
            ' "sum_sent": {"bits_per_second": 8000000},'
            ' "sum": {"bits_per_second": 8000000}},'
            ' "intervals": [{"sum": {"bits_per_second": 8000000}}]}'
        )
        result = IPerfResult(json_data, reporting_speed_units="Mbits")
        self.assertIsNotNone(result.avg_rate)
        self.assertIsNotNone(result.avg_receive_rate)
        self.assertIsNotNone(result.avg_send_rate)
        expected_rate = 8000000 / (1024 * 1024)
        self.assertAlmostEqual(result.avg_rate or 0.0, expected_rate, places=2)
        self.assertAlmostEqual(
            result.avg_receive_rate or 0.0, expected_rate, places=2
        )
        self.assertAlmostEqual(
            result.avg_send_rate or 0.0, expected_rate, places=2
        )

    def test_std_deviation_calculation(self) -> None:
        """Verifies sample standard deviation (N-1) calculation using math.fsum."""
        mb_to_bps = 1024 * 1024 * 8
        mock_result = {
            "end": {"sum": {"bits_per_second": 10 * mb_to_bps}},
            "intervals": [
                {"sum": {"bits_per_second": 10 * mb_to_bps}},
                {"sum": {"bits_per_second": 20 * mb_to_bps}},
                {"sum": {"bits_per_second": 30 * mb_to_bps}},
                {
                    "sum": {"bits_per_second": 999 * mb_to_bps}
                },  # ignored (last interval)
            ],
        }
        res = IPerfResult(json.dumps(mock_result))
        std_dev = res.get_std_deviation(iperf_ignored_interval=0)
        self.assertIsNotNone(std_dev)
        assert std_dev is not None
        self.assertAlmostEqual(std_dev, 10.0, places=4)


class IPerfServerLocalTest(unittest.TestCase):
    """Tests local IPerfServer execution."""

    def setUp(self) -> None:
        patcher = mock.patch.object(
            iperf_server, "_get_port_from_ss_output", return_value="5201"
        )
        patcher.start()
        self.addCleanup(patcher.stop)

    @mock.patch("builtins.open")
    @mock.patch("subprocess.Popen")
    @mock.patch("libs.proc.job.run")
    def test_start_and_stop(
        self, mock_job: mock.Mock, mock_popen: mock.Mock, _mock_open: mock.Mock
    ) -> None:
        mock_proc = mock.Mock()
        mock_proc.pid = 1234
        mock_popen.return_value = mock_proc
        mock_job.return_value.stdout = b"fake ss output"

        server = IPerfServer(5201)
        with mock.patch.object(
            server, "_get_full_file_path", return_value=MOCK_LOGFILE_PATH
        ):
            server.start()
            self.assertTrue(server.started)
            mock_job.assert_called_with("ss -l -p -n | grep iperf")

            log_path = server.stop()
            self.assertFalse(server.started)
            self.assertEqual(log_path, MOCK_LOGFILE_PATH)
            mock_proc.terminate.assert_called_once()

    @mock.patch("builtins.open")
    @mock.patch("subprocess.Popen")
    @mock.patch("libs.proc.job.run")
    def test_start_retries_when_grep_initially_finds_no_match(
        self, mock_job: mock.Mock, mock_popen: mock.Mock, _mock_open: mock.Mock
    ) -> None:
        mock_proc = mock.Mock()
        mock_proc.pid = 1234
        mock_popen.return_value = mock_proc

        first_res = mock.Mock()
        first_res.stdout = b""
        second_res = mock.Mock()
        second_res.stdout = (
            b'tcp LISTEN 0 5 *:5201 *:* users:(("iperf3",pid=1234,fd=3))'
        )
        mock_job.side_effect = [first_res, second_res]

        server = IPerfServer(5201)
        with (
            mock.patch.object(
                iperf_server,
                "_get_port_from_ss_output",
                side_effect=[ProcessLookupError("not found"), "5201"],
            ),
            mock.patch.object(
                server, "_get_full_file_path", return_value=MOCK_LOGFILE_PATH
            ),
        ):
            server.start()
            self.assertTrue(server.started)
            self.assertEqual(server.port, 5201)
            self.assertEqual(mock_job.call_count, 2)


class IPerfServerOverSshTest(unittest.TestCase):
    """Tests remote IPerfServerOverSsh execution on both Raspi and OpenWrt-One."""

    def _create_ssh_settings(self, user: str = "root") -> settings.SshSettings:
        return settings.from_config(
            {
                "host": "192.168.42.11",
                "user": user,
                "identity_file": "/dev/null",
            }
        )

    @mock.patch("antlion.utils.get_interface_based_on_ip", return_value="eth1")
    def test_raspberry_pi_setup_with_nmcli_and_journalctl(
        self, _mock_net: mock.Mock
    ) -> None:
        """Verifies Raspberry Pi behavior: nmcli and journalctl are present and used."""
        ssh_cfg = self._create_ssh_settings(user="pi")
        mock_ssh = mock.create_autospec(connection.SshConnection)
        mock_ssh.run.return_value.stdout = b""
        with mock.patch(
            "libs.commands.command.LinuxCommand.available", return_value=True
        ):
            server = IPerfServerOverSsh(
                ssh_settings=ssh_cfg,
                port=5201,
                test_interface="eth0",
                use_killall=False,
                ssh_session=mock_ssh,
            )
            self.assertIsNotNone(server._nmcli)
            self.assertIsNotNone(server._journalctl)
            assert server._journalctl is not None

            with mock.patch.object(
                server._journalctl, "logs", return_value="systemd log content"
            ):
                self.assertEqual(
                    server.get_systemd_journal(), "systemd log content"
                )

    def test_openwrt_one_setup_without_nmcli_or_journalctl(self) -> None:
        """Verifies OpenWrt-One behavior: nmcli and journalctl are absent, handled gracefully."""
        ssh_cfg = self._create_ssh_settings(user="root")
        mock_ssh = mock.create_autospec(connection.SshConnection)
        with mock.patch(
            "libs.commands.command.LinuxCommand.available", return_value=False
        ):
            server = IPerfServerOverSsh(
                ssh_settings=ssh_cfg,
                port=5201,
                test_interface="lan",
                use_killall=True,
                ssh_session=mock_ssh,
            )
            self.assertIsNone(server._nmcli)
            self.assertIsNone(server._journalctl)
            self.assertIsNone(server.get_systemd_journal())

    @mock.patch("builtins.open")
    def test_openwrt_one_start_stop_with_killall(
        self, _mock_open: mock.Mock
    ) -> None:
        """Verifies OpenWrt-One start and stop lifecycle using killall iperf3."""
        ssh_cfg = self._create_ssh_settings(user="root")
        mock_ssh = mock.create_autospec(connection.SshConnection)
        mock_job = mock.Mock()
        mock_job.stdout = b"5678"
        mock_ssh.run_async.return_value = mock_job
        mock_ssh.run.return_value.stdout = b'{"end": {}}'

        with mock.patch(
            "libs.commands.command.LinuxCommand.available", return_value=False
        ):
            server = IPerfServerOverSsh(
                ssh_settings=ssh_cfg,
                port=5201,
                test_interface="lan",
                use_killall=True,
                ssh_session=mock_ssh,
            )
            with mock.patch.object(
                server, "_get_full_file_path", return_value=MOCK_LOGFILE_PATH
            ):
                server.start()
                self.assertTrue(server.started)
                mock_ssh.run.assert_any_call(
                    "killall iperf3", ignore_status=True
                )

                log = server.stop()
                self.assertFalse(server.started)
                self.assertEqual(log, MOCK_LOGFILE_PATH)
                mock_ssh.run.assert_any_call(
                    ["killall", "iperf3"], ignore_status=True
                )
                mock_ssh.run.assert_any_call(
                    ["cat", "/tmp/iperf_server_port5201.log"],
                    ignore_status=True,
                )
                mock_ssh.run.assert_any_call(
                    ["rm", "-f", "/tmp/iperf_server_port5201.log"],
                    ignore_status=True,
                )

    @mock.patch("antlion.utils.get_interface_based_on_ip", return_value="eth1")
    @mock.patch("antlion.utils.renew_linux_ip_address")
    def test_renew_test_interface_ip_address(
        self, mock_renew: mock.Mock, _mock_net: mock.Mock
    ) -> None:
        ssh_cfg = self._create_ssh_settings(user="pi")
        mock_ssh = mock.create_autospec(connection.SshConnection)
        mock_ssh.run.return_value.stdout = b""

        with mock.patch(
            "libs.commands.command.LinuxCommand.available", return_value=False
        ):
            # Raspberry Pi (eth0, even with use_killall=True) calls renew_linux_ip_address
            server_raspi = IPerfServerOverSsh(
                ssh_settings=ssh_cfg,
                port=5201,
                test_interface="eth0",
                use_killall=True,
                ssh_session=mock_ssh,
            )
            server_raspi.renew_test_interface_ip_address()
            mock_renew.assert_called_once_with(mock_ssh, "eth0")

            # OpenWrt (test_interface="br-lan") is a no-op
            mock_renew.reset_mock()
            server_openwrt = IPerfServerOverSsh(
                ssh_settings=ssh_cfg,
                port=5201,
                test_interface="br-lan",
                use_killall=True,
                ssh_session=mock_ssh,
            )
            server_openwrt.renew_test_interface_ip_address()
            mock_renew.assert_not_called()

    def test_start_exec_and_pid_parsing(self) -> None:
        """Verifies start() uses exec and correctly parses numeric vs non-numeric PID."""
        ssh_cfg = self._create_ssh_settings(user="root")
        mock_ssh = mock.create_autospec(connection.SshConnection)
        mock_job = mock.Mock()
        mock_ssh.run_async.return_value = mock_job

        with mock.patch(
            "libs.commands.command.LinuxCommand.available", return_value=False
        ):
            server = IPerfServerOverSsh(
                ssh_settings=ssh_cfg,
                port=5201,
                test_interface="lan",
                use_killall=True,
                ssh_session=mock_ssh,
            )

            # Valid PID
            mock_job.stdout = b"12345\n"
            server.start()
            self.assertEqual(server._iperf_pid, "12345")
            mock_ssh.run_async.assert_called_with(
                "exec iperf3 -s -J -p 5201  > /tmp/iperf_server_port5201.log"
            )

            # Reset for non-numeric PID test
            server._iperf_pid = None
            mock_job.stdout = b"error: command not found\n"
            server.start()
            self.assertIsNone(server._iperf_pid)

    def test_close_ssh_preserves_session_and_runners(self) -> None:
        """Verifies close_ssh closes the session without resetting _ssh_session or runners."""
        ssh_cfg = self._create_ssh_settings(user="root")
        mock_ssh = mock.create_autospec(connection.SshConnection)

        with mock.patch(
            "libs.commands.command.LinuxCommand.available", return_value=True
        ), mock.patch(
            "antlion.utils.get_interface_based_on_ip", return_value="eth1"
        ):
            server = IPerfServerOverSsh(
                ssh_settings=ssh_cfg,
                port=5201,
                test_interface="eth0",
                ssh_session=mock_ssh,
            )
            old_journal = server._journalctl
            old_ss = server._ss

            server.close_ssh()
            mock_ssh.close.assert_called_once()
            self.assertIs(server._ssh_session, mock_ssh)
            self.assertIs(server._get_ssh(), mock_ssh)
            self.assertIs(server._journalctl, old_journal)
            self.assertIs(server._ss, old_ss)

    def test_start_after_close_ssh_succeeds(self) -> None:
        """Verifies start() succeeds without error after close_ssh()."""
        ssh_cfg = self._create_ssh_settings(user="root")
        mock_ssh = mock.create_autospec(connection.SshConnection)
        mock_job = mock.Mock()
        mock_job.stdout = b"12345\n"
        mock_ssh.run_async.return_value = mock_job
        mock_ssh.run.return_value.stdout = b""

        with mock.patch(
            "libs.commands.command.LinuxCommand.available", return_value=False
        ):
            server = IPerfServerOverSsh(
                ssh_settings=ssh_cfg,
                port=5201,
                test_interface="lan",
                use_killall=True,
                ssh_session=mock_ssh,
            )
            server.close_ssh()
            mock_ssh.close.assert_called_once()

            # Calling start() after close_ssh() must not raise AssertionError in _cleanup_iperf_port.
            server.start()
            self.assertTrue(server.started)
            mock_ssh.run.assert_any_call("killall iperf3", ignore_status=True)
            mock_ssh.run_async.assert_called_once()

    def test_get_systemd_journal_preserves_journalctl(self) -> None:
        """Verifies get_systemd_journal queries journalctl directly without resetting state."""
        ssh_cfg = self._create_ssh_settings(user="root")
        mock_ssh = mock.create_autospec(connection.SshConnection)

        with mock.patch(
            "libs.commands.command.LinuxCommand.available", return_value=True
        ), mock.patch(
            "antlion.utils.get_interface_based_on_ip", return_value="eth1"
        ):
            server = IPerfServerOverSsh(
                ssh_settings=ssh_cfg,
                port=5201,
                test_interface="eth0",
                ssh_session=mock_ssh,
            )
            mock_journal = mock.Mock()
            mock_journal.logs.return_value = "systemd journal log output"
            server._journalctl = mock_journal

            logs = server.get_systemd_journal()
            self.assertEqual(logs, "systemd journal log output")
            mock_journal.logs.assert_called_once()
            self.assertIs(server._journalctl, mock_journal)

    @mock.patch("builtins.open", new_callable=mock.mock_open)
    @mock.patch(
        "iperf.iperf_server.mobly_utils.get_current_epoch_time",
        return_value=1234567890000,
    )
    def test_download_logs_writes_file_when_journal_available(
        self, _mock_time: mock.Mock, mock_open: mock.Mock
    ) -> None:
        """Verifies download_logs writes systemd journal to disk when journal is available."""
        ssh_cfg = self._create_ssh_settings(user="root")
        mock_ssh = mock.create_autospec(connection.SshConnection)

        with mock.patch(
            "libs.commands.command.LinuxCommand.available", return_value=True
        ), mock.patch(
            "antlion.utils.get_interface_based_on_ip", return_value="eth1"
        ):
            server = IPerfServerOverSsh(
                ssh_settings=ssh_cfg,
                port=5201,
                test_interface="eth0",
                ssh_session=mock_ssh,
            )
            mock_journal = mock.Mock()
            mock_journal.logs.return_value = "systemd journal log output"
            server._journalctl = mock_journal

            with mock.patch.object(server.log, "info") as mock_log_info:
                server.download_logs("/tmp/logs")
                expected_timestamp = logger.normalize_log_line_timestamp(
                    logger.epoch_to_log_line_timestamp(1234567890000)
                )
                expected_path = os.path.join(
                    "/tmp/logs", f"iperf_systemd_{expected_timestamp}.log"
                )
                mock_open.assert_called_once_with(
                    expected_path, "a", encoding="utf-8"
                )
                handle = mock_open()
                handle.write.assert_called_once_with(
                    "systemd journal log output"
                )
                mock_log_info.assert_called_once_with(
                    f"Wrote systemd journal to {expected_path}"
                )

    @mock.patch("builtins.open", new_callable=mock.mock_open)
    def test_download_logs_skips_when_journal_unavailable(
        self, mock_open: mock.Mock
    ) -> None:
        """Verifies download_logs skips writing to disk when journalctl is unavailable."""
        ssh_cfg = self._create_ssh_settings(user="root")
        mock_ssh = mock.create_autospec(connection.SshConnection)

        with mock.patch(
            "libs.commands.command.LinuxCommand.available", return_value=False
        ):
            server = IPerfServerOverSsh(
                ssh_settings=ssh_cfg,
                port=5201,
                test_interface="lan",
                use_killall=True,
                ssh_session=mock_ssh,
            )
            self.assertIsNone(server.get_systemd_journal())

            with mock.patch.object(server.log, "debug") as mock_log_debug:
                server.download_logs("/tmp/logs")
                mock_open.assert_not_called()
                mock_log_debug.assert_called_once_with(
                    "Systemd journal not available on this device; skipping."
                )


if __name__ == "__main__":
    unittest.main()

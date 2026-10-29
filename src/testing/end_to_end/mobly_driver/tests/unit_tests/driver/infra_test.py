# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for mobly_driver/driver/infra.py."""

import os
import tempfile
import unittest
from datetime import timedelta
from typing import Any
from unittest.mock import call, mock_open, patch

from mobly_driver.driver import common, infra

_HONEYDEW_CONFIG: dict[str, Any] = {
    "transports": {
        "ffx": {
            "path": "/ffx/path",
            "subtools_search_path": "subtools/search/path",
        }
    }
}


class InfraMoblyDriverTest(unittest.TestCase):
    """Infra Driver tests"""

    @patch("yaml.dump", return_value="yaml_str")
    @patch("mobly_driver.driver.common.read_json_from_file")
    @patch("mobly_driver.driver.common.read_yaml_from_file")
    @patch("mobly_driver.api.api_mobly.new_testbed_config")
    def test_generate_test_config_with_params_success(
        self,
        mock_new_config: Any,
        mock_read_yaml: Any,
        mock_read_json: Any,
        *unused_args: Any,
    ) -> None:
        """Test case for successful config generation"""
        mock_read_yaml.return_value = {"existing_param": "value"}
        mock_read_json.return_value = [
            {"type": "FuchsiaDevice", "nodename": "dut"}
        ]
        driver = infra.InfraDriver(
            tb_json_path="tb/json/path",
            honeydew_config=_HONEYDEW_CONFIG,
            params_path="params/path",
            output_path="",
        )
        ret = driver.generate_test_config()

        mock_new_config.assert_called_once_with(
            testbed_name="InfraTestbed",
            output_path="",
            honeydew_config=_HONEYDEW_CONFIG,
            mobly_controllers=[{"type": "FuchsiaDevice", "nodename": "dut"}],
            test_params_dict={
                "existing_param": "value",
                "ffx-subtools-search-path": "subtools/search/path",
            },
            botanist_honeydew_map={"nodename": "name"},
            ssh_path=None,
        )
        mock_read_yaml.assert_called_once_with("params/path")
        mock_read_json.assert_called_once_with("tb/json/path")
        self.assertEqual(ret, "yaml_str")

    @patch("yaml.dump", return_value="yaml_str")
    @patch("mobly_driver.driver.common.read_json_from_file")
    @patch("mobly_driver.driver.common.read_yaml_from_file")
    @patch("mobly_driver.api.api_mobly.new_testbed_config")
    def test_generate_test_config_without_params_success(
        self,
        mock_new_config: Any,
        mock_read_yaml: Any,
        mock_read_json: Any,
        *unused_args: Any,
    ) -> None:
        """Test case for successful config without params generation"""
        mock_read_json.return_value = [
            {"type": "FuchsiaDevice", "nodename": "dut"}
        ]
        driver = infra.InfraDriver(
            tb_json_path="tb/json/path",
            honeydew_config=_HONEYDEW_CONFIG,
            output_path="",
        )
        ret = driver.generate_test_config()

        mock_new_config.assert_called_once_with(
            testbed_name="InfraTestbed",
            output_path="",
            honeydew_config=_HONEYDEW_CONFIG,
            mobly_controllers=[{"type": "FuchsiaDevice", "nodename": "dut"}],
            test_params_dict={
                "ffx-subtools-search-path": "subtools/search/path",
            },
            botanist_honeydew_map={"nodename": "name"},
            ssh_path=None,
        )
        mock_read_yaml.assert_not_called()
        mock_read_json.assert_called_once_with("tb/json/path")
        self.assertEqual(ret, "yaml_str")

    @patch(
        "mobly_driver.driver.common.read_json_from_file",
        side_effect=common.InvalidFormatException,
    )
    def test_generate_test_config_invalid_json_raises_exception(
        self, *unused_args: Any
    ) -> None:
        """Test case for exception being raised on invalid JSON content"""
        driver = infra.InfraDriver(
            tb_json_path="tb/json/path",
            honeydew_config=_HONEYDEW_CONFIG,
            output_path="",
        )
        with self.assertRaises(common.InvalidFormatException):
            driver.generate_test_config()

    @patch(
        "mobly_driver.driver.common.read_yaml_from_file",
        side_effect=common.InvalidFormatException,
    )
    @patch("mobly_driver.driver.common.read_json_from_file")
    def test_generate_test_config_invalid_yaml_raises_exception(
        self, *unused_args: Any
    ) -> None:
        """Test case for exception being raised on invalid YAML content"""
        driver = infra.InfraDriver(
            tb_json_path="tb/json/path",
            honeydew_config=_HONEYDEW_CONFIG,
            params_path="params/path",
            output_path="",
        )
        with self.assertRaises(common.InvalidFormatException):
            driver.generate_test_config()

    @patch(
        "mobly_driver.driver.common.read_json_from_file", side_effect=OSError
    )
    def test_generate_test_config_invalid_tb_path_raises_exception(
        self, *unused_args: Any
    ) -> None:
        """Test case for exception being raised on invalid testbed JSON path"""
        driver = infra.InfraDriver(
            tb_json_path="/does/not/exist",
            honeydew_config=_HONEYDEW_CONFIG,
            output_path="",
        )
        with self.assertRaises(common.DriverException):
            driver.generate_test_config()

    @patch(
        "mobly_driver.driver.common.read_yaml_from_file", side_effect=OSError
    )
    def test_generate_test_config_invalid_params_path_raises_exception(
        self, *unused_args: Any
    ) -> None:
        """Test case for exception being raised on invalid params YAML path"""
        driver = infra.InfraDriver(
            tb_json_path="/does/not/exist",
            honeydew_config=_HONEYDEW_CONFIG,
            params_path="params/path",
            output_path="",
        )
        with self.assertRaises(common.DriverException):
            driver.generate_test_config()

    @patch("mobly_driver.api.api_mobly.get_result_path")
    @patch(
        "mobly_driver.api.api_mobly.get_latest_test_output_dir_symlink_path",
        return_value="path/to/remove",
    )
    @patch(
        "mobly_driver.api.api_infra.TESTPARSER_RESULT_HEADER",
        "---MOCK_HEADER---",
    )
    @patch("builtins.open", new_callable=mock_open, read_data="test_result")
    @patch("os.remove")
    @patch("builtins.print")
    def test_teardown_success(
        self, mock_print: Any, mock_rm: Any, *unused_args: Any
    ) -> None:
        """Test case for teardown"""
        driver = infra.InfraDriver(
            tb_json_path="",
            honeydew_config=_HONEYDEW_CONFIG,
            output_path="",
        )
        driver.teardown()

        self.assertIn(call("---MOCK_HEADER---"), mock_print.call_args_list)
        self.assertIn(call("test_result"), mock_print.call_args_list)
        mock_rm.assert_called_once_with("path/to/remove")

    @patch("mobly_driver.api.api_mobly.get_result_path")
    @patch(
        "mobly_driver.api.api_mobly.get_latest_test_output_dir_symlink_path",
        return_value="path/to/remove",
    )
    @patch(
        "mobly_driver.api.api_infra.TESTPARSER_RESULT_HEADER",
        "---MOCK_HEADER---",
    )
    @patch("builtins.open", new_callable=mock_open, read_data="test_result")
    @patch("os.remove")
    @patch("builtins.print")
    def test_teardown_with_timeout(
        self, mock_print: Any, mock_rm: Any, *unused_args: Any
    ) -> None:
        """Test case for teardown when test timed out"""
        driver = infra.InfraDriver(
            tb_json_path="",
            honeydew_config=_HONEYDEW_CONFIG,
            output_path="",
        )
        driver.teardown(timed_out=True, test_timeout=timedelta(seconds=260))

        self.assertIn(call("---MOCK_HEADER---"), mock_print.call_args_list)
        self.assertIn(call("test_result"), mock_print.call_args_list)
        self.assertIn(
            call(
                "---\nDetails: Test timed out after 0:04:20.\nType: MoblyDriverTimeout\n...\n"
            ),
            mock_print.call_args_list,
        )
        mock_rm.assert_called_once_with("path/to/remove")

    @patch("mobly_driver.api.api_mobly.get_result_path")
    @patch("mobly_driver.api.api_mobly.get_latest_test_output_dir_symlink_path")
    @patch("mobly_driver.api.api_infra.TESTPARSER_RESULT_HEADER")
    @patch("builtins.open", side_effect=OSError)
    @patch("os.remove", side_effect=OSError)
    @patch("builtins.print")
    def test_teardown_success_without_test_results(
        self, mock_print: Any, mock_rm: Any, *unused_args: Any
    ) -> None:
        """Test case for teardown succeeding despite missing results"""
        driver = infra.InfraDriver(
            tb_json_path="",
            honeydew_config=_HONEYDEW_CONFIG,
            output_path="",
        )
        driver.teardown()

        mock_print.assert_not_called()
        mock_rm.assert_called_once()

    @patch("mobly_driver.api.api_mobly.get_result_path")
    @patch("mobly_driver.api.api_mobly.get_latest_test_output_dir_symlink_path")
    @patch(
        "mobly_driver.api.api_infra.TESTPARSER_RESULT_HEADER",
        "---MOCK_HEADER---",
    )
    @patch("builtins.open", side_effect=OSError)
    @patch("os.remove", side_effect=OSError)
    @patch("builtins.print")
    def test_teardown_with_timeout_without_test_results(
        self, mock_print: Any, mock_rm: Any, *unused_args: Any
    ) -> None:
        """Test case for teardown emitting timeout record even when results file is missing"""
        driver = infra.InfraDriver(
            tb_json_path="",
            honeydew_config=_HONEYDEW_CONFIG,
            output_path="",
        )
        driver.teardown(timed_out=True, test_timeout=timedelta(seconds=260))

        self.assertEqual(
            mock_print.call_args_list,
            [
                call("---MOCK_HEADER---"),
                call(
                    "---\nDetails: Test timed out after 0:04:20.\nType: MoblyDriverTimeout\n...\n"
                ),
            ],
        )
        mock_rm.assert_called_once()

    @patch("builtins.print")
    def test_teardown_renames_timestamp_directory_to_artifacts(
        self, mock_print: Any
    ) -> None:
        """Test case verifying timestamped Mobly subdir is renamed to artifacts."""
        with tempfile.TemporaryDirectory() as tmp_dir:
            sibling_file = os.path.join(tmp_dir, "stdout-and-stderr.txt")
            with open(sibling_file, "w") as f:
                f.write("runner output")

            testbed_dir = os.path.join(tmp_dir, "InfraTestbed")
            timestamp_dir = os.path.join(testbed_dir, "07-08-2026_07-05-38-773")
            nested_case_dir = os.path.join(
                timestamp_dir, "BootBenchmark", "test_e2e_boot"
            )
            os.makedirs(nested_case_dir)

            with open(
                os.path.join(timestamp_dir, "test_summary.yaml"), "w"
            ) as f:
                f.write("summary_content")
            with open(os.path.join(timestamp_dir, "test_log.DEBUG"), "w") as f:
                f.write("debug_log")
            with open(
                os.path.join(nested_case_dir, "warm_kcounters.txt"), "w"
            ) as f:
                f.write("kcounters_data")

            symlink_path = os.path.join(testbed_dir, "latest")
            os.symlink(timestamp_dir, symlink_path)

            driver = infra.InfraDriver(
                tb_json_path="",
                honeydew_config=_HONEYDEW_CONFIG,
                output_path=tmp_dir,
            )
            driver.teardown()

            artifacts_dir = os.path.join(testbed_dir, "artifacts")
            self.assertFalse(os.path.lexists(symlink_path))
            self.assertFalse(os.path.exists(timestamp_dir))
            self.assertTrue(os.path.isfile(sibling_file))
            self.assertTrue(
                os.path.isfile(os.path.join(artifacts_dir, "test_summary.yaml"))
            )
            self.assertTrue(
                os.path.isfile(os.path.join(artifacts_dir, "test_log.DEBUG"))
            )
            moved_artifact = os.path.join(
                artifacts_dir,
                "BootBenchmark",
                "test_e2e_boot",
                "warm_kcounters.txt",
            )
            self.assertTrue(os.path.isfile(moved_artifact))
            with open(moved_artifact, "r") as f:
                self.assertEqual(f.read(), "kcounters_data")
            self.assertIn(call("summary_content"), mock_print.call_args_list)

    @patch("logging.exception")
    @patch("builtins.print")
    def test_teardown_relative_latest_symlink_and_logs_on_error(
        self, unused_mock_print: Any, mock_log_exception: Any
    ) -> None:
        """Test case verifying relative latest symlink and error logging on collision."""
        with tempfile.TemporaryDirectory() as tmp_dir:
            testbed_dir = os.path.join(tmp_dir, "InfraTestbed")
            ts_dir_1 = os.path.join(testbed_dir, "07-08-2026_07-05-38-773")
            os.makedirs(ts_dir_1)
            with open(os.path.join(ts_dir_1, "test_log.INFO"), "w") as f:
                f.write("run_1_log")

            symlink_path = os.path.join(testbed_dir, "latest")
            os.symlink("07-08-2026_07-05-38-773", symlink_path)

            driver = infra.InfraDriver(
                tb_json_path="",
                honeydew_config=_HONEYDEW_CONFIG,
                output_path=tmp_dir,
            )
            driver.teardown()

            artifacts_dir = os.path.join(testbed_dir, "artifacts")
            self.assertFalse(os.path.lexists(symlink_path))
            self.assertFalse(os.path.exists(ts_dir_1))
            self.assertTrue(
                os.path.isfile(os.path.join(artifacts_dir, "test_log.INFO"))
            )
            mock_log_exception.assert_not_called()

            # If `artifacts` already exists and is non-empty, os.rename raises
            # OSError which is caught and logged via logging.exception.
            ts_dir_2 = os.path.join(testbed_dir, "07-08-2026_07-06-12-104")
            os.makedirs(ts_dir_2)
            os.symlink("07-08-2026_07-06-12-104", symlink_path)
            driver.teardown()

            mock_log_exception.assert_called_once()
            self.assertTrue(os.path.isdir(ts_dir_2))

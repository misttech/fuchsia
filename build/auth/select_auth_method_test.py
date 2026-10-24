#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for select_auth_method.py."""

import pathlib
import subprocess
import tempfile
import unittest
from unittest import mock

import gcloud
import select_auth_method


class SelectAuthMethodTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.check_loas_script = (
            pathlib.Path(self.temp_dir.name) / "check_loas_restrictions.sh"
        )
        self.cred_helper = pathlib.Path(self.temp_dir.name) / "credhelper"

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def test_machine_auth_on_bot(self) -> None:
        # LUCI/Swarming environment variables present
        env = {"BUILDBUCKET_ID": "123"}
        method = select_auth_method.select_auth_method(
            env=env,
            check_loas_script=self.check_loas_script,
            cred_helper=self.cred_helper,
        )
        self.assertEqual(method, "machine")

    def test_error_when_all_auth_methods_missing(self) -> None:
        # No corporate LOAS, no local gcloud
        env = {"PATH": ""}
        with self.assertRaises(ValueError) as cm:
            select_auth_method.select_auth_method(
                env=env,
                check_loas_script=self.check_loas_script,
                cred_helper=self.cred_helper,
            )
        self.assertIn(
            "We could not find a valid corporate LOAS credential",
            str(cm.exception),
        )

    @mock.patch.object(select_auth_method, "is_executable", return_value=True)
    def test_loas_unrestricted_with_helper(
        self, mock_is_exec: mock.Mock
    ) -> None:
        env = {"HOME": self.temp_dir.name}
        with mock.patch.object(
            subprocess, "check_output", return_value="unrestricted\n"
        ):
            method = select_auth_method.select_auth_method(
                env=env,
                check_loas_script=self.check_loas_script,
                cred_helper=self.cred_helper,
            )
            self.assertEqual(method, "loas")

    @mock.patch.object(
        subprocess, "check_output", return_value="unrestricted\n"
    )
    def test_loas_unrestricted_without_helper_falls_back_to_oauth(
        self, mock_sub: mock.Mock
    ) -> None:
        # check_loas_script is executable, but cred_helper is not. Fallback to oauth.
        env = {"HOME": self.temp_dir.name}
        adc_file = pathlib.Path(self.temp_dir.name) / gcloud.ADC_SUBPATH
        adc_file.parent.mkdir(parents=True, exist_ok=True)
        adc_file.touch()

        def mock_is_executable(path: pathlib.Path) -> bool:
            return path == self.check_loas_script

        with mock.patch.object(
            select_auth_method, "is_executable", side_effect=mock_is_executable
        ):
            method = select_auth_method.select_auth_method(
                env=env,
                check_loas_script=self.check_loas_script,
                cred_helper=self.cred_helper,
            )
            self.assertEqual(method, "oauth")

    def test_oauth_fallback_when_no_loas_but_has_local_creds(self) -> None:
        # check_loas_restrictions.sh fails (returns ""), but local ADC credentials exist.
        env = {"HOME": self.temp_dir.name}
        adc_file = pathlib.Path(self.temp_dir.name) / gcloud.ADC_SUBPATH
        adc_file.parent.mkdir(parents=True, exist_ok=True)
        adc_file.touch()

        with mock.patch.object(
            subprocess,
            "check_output",
            side_effect=subprocess.CalledProcessError(1, "cmd"),
        ):
            method = select_auth_method.select_auth_method(
                env=env,
                check_loas_script=self.check_loas_script,
                cred_helper=self.cred_helper,
            )
            self.assertEqual(method, "oauth")

    def test_is_user_based_property(self) -> None:
        self.assertTrue(select_auth_method.AuthType.LOAS.is_user_based)
        self.assertTrue(select_auth_method.AuthType.OAUTH.is_user_based)
        self.assertFalse(select_auth_method.AuthType.MACHINE.is_user_based)


if __name__ == "__main__":
    unittest.main()

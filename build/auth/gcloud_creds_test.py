#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for gcloud_creds.py."""

# Add our active parent directory to sys.path to enable flat, direct imports of sister modules
import pathlib
import sys

_SCRIPT_DIR = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(_SCRIPT_DIR))

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from typing import Any
from unittest import mock

import apt
import gcloud
import gcloud_creds


class IsolateGcloudCredsTestBase(unittest.TestCase):
    """Shared base class for isolating credentials tests.

    De-duplicates temporary directories, mock ADC file creation, and CLI runner helpers.
    """

    def setUp(self) -> None:
        self.temp_dir_obj = tempfile.TemporaryDirectory()
        self.temp_dir = pathlib.Path(self.temp_dir_obj.name)
        self.out_dir = self.temp_dir / "out"
        self.out_dir.mkdir()

    def tearDown(self) -> None:
        self.temp_dir_obj.cleanup()

    def _create_mock_adc(
        self,
        filename: str = "global_adc.json",
        content_dict: dict[str, Any] | None = None,
    ) -> pathlib.Path:
        """Helper to create a mock ADC file with given JSON contents."""
        path = self.temp_dir / filename
        content = {
            "fake_id": "fake-value",
            "type": "authorized_user",
            "quota_project_id": "some-other-project",
        }
        if content_dict:
            content.update(content_dict)
        path.write_text(json.dumps(content))
        return path

    def _run_main_cli(
        self, args: list[str], adc_path: pathlib.Path | None = None
    ) -> tuple[int, str, str]:
        """Helper to execute main() as a CLI, capturing exit code, stdout, and stderr."""
        import contextlib
        import io

        environ = {}
        if adc_path:
            environ["GOOGLE_APPLICATION_CREDENTIALS"] = str(adc_path)

        with mock.patch.dict(os.environ, environ):
            stdout = io.StringIO()
            stderr = io.StringIO()
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(
                stderr
            ):
                exit_code = gcloud_creds.main(args)
            return exit_code, stdout.getvalue(), stderr.getvalue()


class IsolateCredentialsTest(IsolateGcloudCredsTestBase):
    """Tests the primary credentials isolation function 'isolate_credentials()'."""

    def test_isolate_credentials_authorized_user(self) -> None:
        # Create a mock standard user default credentials file
        adc_src = self._create_mock_adc(
            content_dict={"fake_extra_field": "fake-extra-value"}
        )

        # Run isolation
        local_adc = gcloud_creds.isolate_credentials(
            out_dir=self.out_dir,
            quota_project="fake-project",
            source_credentials_path=adc_src,
        )

        self.assertIsNotNone(local_adc)
        assert local_adc is not None
        self.assertTrue(local_adc.is_file())
        self.assertEqual(
            local_adc, self.out_dir / gcloud_creds.ISOLATED_ADC_FILENAME
        )

        # Read the isolated copy back and verify changes
        isolated_content = json.loads(local_adc.read_text())

        self.assertEqual(isolated_content["fake_id"], "fake-value")
        self.assertEqual(
            isolated_content["fake_extra_field"], "fake-extra-value"
        )
        self.assertEqual(isolated_content["type"], "authorized_user")
        self.assertEqual(
            isolated_content["quota_project_id"],
            "fake-project",
        )

    def test_isolate_credentials_with_custom_quota_project(self) -> None:
        adc_src = self._create_mock_adc()

        # Run isolation with a custom overridden quota project
        local_adc = gcloud_creds.isolate_credentials(
            out_dir=self.out_dir,
            quota_project="custom-override-project",
            source_credentials_path=adc_src,
        )

        self.assertIsNotNone(local_adc)
        assert local_adc is not None
        isolated_content = json.loads(local_adc.read_text())

        self.assertEqual(isolated_content["fake_id"], "fake-value")
        self.assertEqual(isolated_content["type"], "authorized_user")
        self.assertEqual(
            isolated_content["quota_project_id"], "custom-override-project"
        )

    def test_isolate_credentials_ignores_service_account(self) -> None:
        # Create a mock service account key file
        adc_src = self._create_mock_adc(
            filename="service_account.json",
            content_dict={
                "client_email": "mock-service-account@google.com",
                "type": "service_account",
            },
        )

        # Run isolation
        local_adc = gcloud_creds.isolate_credentials(
            out_dir=self.out_dir,
            quota_project="fake-project",
            source_credentials_path=adc_src,
        )

        # Should not create any isolated copy or modify anything
        self.assertIsNone(local_adc)
        self.assertFalse(
            (self.out_dir / gcloud_creds.ISOLATED_ADC_FILENAME).exists()
        )

    def test_isolate_credentials_missing_source_raises_gcloud_creds_error(
        self,
    ) -> None:
        # Resolve to non-existent file
        adc_src = self.temp_dir / "does_not_exist.json"

        # Verify that it raises GcloudCredsError
        with self.assertRaises(gcloud_creds.GcloudCredsError):
            gcloud_creds.isolate_credentials(
                out_dir=self.out_dir,
                quota_project="fake-project",
                source_credentials_path=adc_src,
            )


class IsolateCredentialsSafeTest(IsolateGcloudCredsTestBase):
    """Tests the exception-safe version 'isolate_credentials_safe()'. Packs direct exceptions."""

    def test_isolate_credentials_safe_success(self) -> None:
        adc_src = self._create_mock_adc()
        local_adc = gcloud_creds.isolate_credentials_safe(
            out_dir=self.out_dir,
            quota_project="fake-project",
            source_credentials_path=adc_src,
        )
        self.assertIsNotNone(local_adc)
        assert local_adc is not None
        self.assertTrue(local_adc.is_file())
        self.assertEqual(
            local_adc, self.out_dir / gcloud_creds.ISOLATED_ADC_FILENAME
        )

    def test_isolate_credentials_missing_source_safe_returns_none(self) -> None:
        # Resolve to non-existent file
        adc_src = self.temp_dir / "does_not_exist.json"

        local_adc = gcloud_creds.isolate_credentials_safe(
            out_dir=self.out_dir,
            quota_project="fake-project",
            source_credentials_path=adc_src,
        )
        self.assertIsNone(local_adc)


class MainCliTest(IsolateGcloudCredsTestBase):
    """Tests the CLI entrypoint 'main()' function directly."""

    def test_main_cli_no_args_displays_help(self) -> None:
        exit_code, stdout, _ = self._run_main_cli([])
        self.assertEqual(exit_code, 0)
        self.assertIn("usage:", stdout)
        self.assertIn("{isolate,login,ensure}", stdout)

    def test_main_cli_isolate_subcommand_success(self) -> None:
        adc_src = self._create_mock_adc()
        exit_code, stdout, _ = self._run_main_cli(
            [
                "isolate",
                "--out-dir",
                str(self.out_dir),
                "--quota-project",
                "cli-project-sub",
            ],
            adc_src,
        )

        self.assertEqual(exit_code, 0)
        local_adc_path = self.out_dir / gcloud_creds.ISOLATED_ADC_FILENAME
        self.assertEqual(stdout.strip(), str(local_adc_path))
        self.assertTrue(local_adc_path.is_file())

        isolated_content = json.loads(local_adc_path.read_text())
        self.assertEqual(
            isolated_content["quota_project_id"], "cli-project-sub"
        )

    @mock.patch.object(shutil, "which", return_value="/bin/gcloud")
    @mock.patch.object(subprocess, "run")
    def test_run_gcloud_login_when_gcloud_exists(
        self, mock_run: mock.Mock, mock_which: mock.Mock
    ) -> None:
        gcloud_creds.run_gcloud_login()
        mock_run.assert_called_once_with(
            ["/bin/gcloud", "auth", "application-default", "login"],
            stdin=mock.ANY,
            stdout=mock.ANY,
            stderr=mock.ANY,
            check=True,
        )

    @mock.patch.object(gcloud, "path")
    @mock.patch.object(apt, "exists", return_value=True)
    @mock.patch.object(apt, "install")
    def test_run_gcloud_login_missing_apt_install_accepts(
        self,
        mock_install: mock.Mock,
        mock_exists: mock.Mock,
        mock_path: mock.Mock,
    ) -> None:
        mock_path.side_effect = [None, pathlib.Path("/usr/bin/gcloud")]

        gcloud_creds.ensure_gcloud_installed()

        mock_install.assert_called_once_with(
            gcloud_creds.GOOGLE_CLOUD_CLI_PACKAGE, interactive=True
        )

    @mock.patch.object(gcloud, "path", return_value=None)
    @mock.patch.object(apt, "exists", return_value=True)
    @mock.patch.object(apt, "install")
    def test_run_gcloud_login_missing_apt_install_declines(
        self,
        mock_install: mock.Mock,
        mock_exists: mock.Mock,
        mock_path: mock.Mock,
    ) -> None:
        mock_install.side_effect = RuntimeError("Installation declined.")

        with self.assertRaises(gcloud_creds.GcloudCredsError) as ctx:
            gcloud_creds.ensure_gcloud_installed()
        self.assertIn("Installation declined.", str(ctx.exception))

    @mock.patch.object(gcloud, "path", return_value=None)
    @mock.patch.object(apt, "exists", return_value=False)
    def test_run_gcloud_login_missing_no_apt_falls_back_to_documentation(
        self, mock_exists: mock.Mock, mock_path: mock.Mock
    ) -> None:
        with self.assertRaises(gcloud_creds.GcloudCredsError) as ctx:
            gcloud_creds.ensure_gcloud_installed()
        self.assertIn(
            "https://cloud.google.com/sdk/docs/install", str(ctx.exception)
        )

    @mock.patch.object(shutil, "which", return_value="/bin/gcloud")
    @mock.patch.object(subprocess, "run")
    def test_main_cli_login_success(
        self, mock_run: mock.Mock, mock_which: mock.Mock
    ) -> None:
        exit_code, _, _ = self._run_main_cli(["login"])
        self.assertEqual(exit_code, 0)
        mock_run.assert_called_once_with(
            ["/bin/gcloud", "auth", "application-default", "login"],
            stdin=mock.ANY,
            stdout=mock.ANY,
            stderr=mock.ANY,
            check=True,
        )

    def test_main_cli_ensure_when_already_exists(self) -> None:
        adc_src = self._create_mock_adc()
        exit_code, stdout, _ = self._run_main_cli(
            [
                "ensure",
                "--out-dir",
                str(self.out_dir),
                "--quota-project",
                "ensure-project",
            ],
            adc_src,
        )

        self.assertEqual(exit_code, 0)
        local_adc_path = self.out_dir / gcloud_creds.ISOLATED_ADC_FILENAME
        self.assertEqual(stdout.strip(), str(local_adc_path))

    @mock.patch.object(gcloud_creds, "run_gcloud_login")
    def test_main_cli_ensure_when_missing_with_interactive(
        self, mock_login: mock.Mock
    ) -> None:
        # Simulate missing global credentials file during resolve
        with mock.patch.object(
            gcloud, "resolve_global_adc_path"
        ) as mock_resolve:
            mock_resolve.return_value = self.temp_dir / "missing.json"

            # Create the file after login is mock-called
            def side_effect() -> None:
                self._create_mock_adc(filename="missing.json")

            mock_login.side_effect = side_effect

            exit_code, stdout, _ = self._run_main_cli(
                [
                    "ensure",
                    "--out-dir",
                    str(self.out_dir),
                    "--quota-project",
                    "ensure-project-interactive",
                    "--interactive",
                ]
            )

            self.assertEqual(exit_code, 0)
            mock_login.assert_called_once()
            local_adc_path = self.out_dir / gcloud_creds.ISOLATED_ADC_FILENAME
            self.assertEqual(stdout.strip(), str(local_adc_path))

    def test_main_cli_ensure_when_missing_non_interactive_returns_one(
        self,
    ) -> None:
        with mock.patch.object(
            gcloud, "resolve_global_adc_path"
        ) as mock_resolve:
            mock_resolve.return_value = self.temp_dir / "missing.json"

            exit_code, _, stderr = self._run_main_cli(
                [
                    "ensure",
                    "--out-dir",
                    str(self.out_dir),
                    "--quota-project",
                    "ensure-project",
                ]
            )

            self.assertEqual(exit_code, 1)
            self.assertIn(
                "No credentials found and --interactive is not set.", stderr
            )


if __name__ == "__main__":
    unittest.main()

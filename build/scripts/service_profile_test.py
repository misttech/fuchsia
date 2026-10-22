#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import io
import json
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

import service_profile


class DummyProfile:
    def __init__(
        self,
        endpoint: str,
        threads: int,
        name: str = "default",
        auth_type: str = "none",
    ) -> None:
        self.endpoint = endpoint
        self.threads = threads
        self.name = name
        self.auth_type = auth_type

    @classmethod
    def from_dict(cls, data: service_profile.JSONObject) -> "DummyProfile":
        return cls(
            endpoint=str(data.get("endpoint", "")),
            threads=int(str(data.get("threads", 0))),
            name=str(data.get("profile_name", "default")),
            auth_type=str(data.get("auth_type", "none")),
        )

    def to_env_dict(self) -> dict[str, str]:
        return {"MY_ENDPOINT": self.endpoint}


class TestServiceProfileManager(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = Path(self.temp_dir.name)

        # Setup template profiles directory
        self.profiles_relpath = Path("build/myservice/profiles")
        self.profiles_dir = self.fuchsia_dir / self.profiles_relpath
        self.profiles_dir.mkdir(parents=True, exist_ok=True)

        self.default_json = self.profiles_dir / "default.json"
        self.default_json.write_text(
            json.dumps(
                {"endpoint": "https://default.example.com", "threads": 4}
            )
        )

        self.custom_json = self.profiles_dir / "custom.json"
        self.custom_json.write_text(
            json.dumps({"endpoint": "https://custom.example.com", "threads": 8})
        )

        self.config_relpath = Path(".fx/config/myservice_profile.json")

        self.spec = service_profile.ServiceProfileSpec(
            service_name="myservice",
            env_profile_var="MYSERVICE_PROFILE",
            profiles_relpath=self.profiles_relpath,
            config_relpath=self.config_relpath,
        )
        self.manager = service_profile.ServiceProfileManager(
            DummyProfile, self.spec
        )

    def tearDown(self) -> None:
        self.temp_dir.cleanup()

    def test_resolve_paths_fallback_to_default_template(self) -> None:
        paths = self.manager.resolve_paths(self.fuchsia_dir, {})
        self.assertEqual(paths.active_name, "default")
        self.assertEqual(paths.active_json, self.default_json)
        self.assertEqual(paths.active_inputs, [self.default_json])
        self.assertEqual(
            paths.available_json, [self.custom_json, self.default_json]
        )

    def test_select_creates_single_combined_json(self) -> None:
        self.manager.select(self.fuchsia_dir, "custom")

        config_path = self.fuchsia_dir / self.config_relpath
        self.assertTrue(config_path.is_file())

        saved = json.loads(config_path.read_text())
        self.assertEqual(saved["profile_name"], "custom")
        self.assertEqual(saved["endpoint"], "https://custom.example.com")
        self.assertEqual(saved["threads"], 8)

        # Confirm path resolution now points to the single on-disk file
        paths = self.manager.resolve_paths(self.fuchsia_dir, {})
        self.assertEqual(paths.active_name, "custom")
        self.assertEqual(paths.active_json, config_path)
        self.assertEqual(paths.active_inputs, [config_path])

    def test_select_nonexistent_profile_raises_error(self) -> None:
        with self.assertRaises(FileNotFoundError):
            self.manager.select(self.fuchsia_dir, "does_not_exist")

    def test_env_override_has_highest_precedence(self) -> None:
        # Select 'custom' on disk
        self.manager.select(self.fuchsia_dir, "custom")

        # But set environment variable to 'default'
        paths = self.manager.resolve_paths(
            self.fuchsia_dir, {"MYSERVICE_PROFILE": "default"}
        )
        self.assertEqual(paths.active_name, "default")
        self.assertEqual(paths.active_json, self.default_json)
        self.assertEqual(paths.active_inputs, [self.default_json])

    def test_env_override_nonexistent_profile_raises_error(self) -> None:
        with self.assertRaises(FileNotFoundError) as ctx:
            self.manager.resolve_paths(
                self.fuchsia_dir, {"MYSERVICE_PROFILE": "nonexistent"}
            )
        self.assertIn("nonexistent", str(ctx.exception))

    def test_load_active_profile(self) -> None:
        # Load fallback default
        prof = self.manager.load(self.fuchsia_dir, {})
        self.assertEqual(prof.endpoint, "https://default.example.com")
        self.assertEqual(prof.name, "default")

        # Select custom and load
        self.manager.select(self.fuchsia_dir, "custom")
        prof = self.manager.load(self.fuchsia_dir, {})
        self.assertEqual(prof.endpoint, "https://custom.example.com")
        self.assertEqual(prof.name, "custom")

    def test_load_explicit_named_profile(self) -> None:
        prof = self.manager.load(self.fuchsia_dir, {}, name="custom")
        self.assertEqual(prof.endpoint, "https://custom.example.com")

    def test_main_argparser_structure(self) -> None:
        parser = self.manager._main_argparser(
            default_fuchsia_dir=self.fuchsia_dir
        )
        args = parser.parse_args(["list", "-v"])
        self.assertEqual(args.subcommand, "list")
        self.assertTrue(args.verbose)
        self.assertEqual(args.fuchsia_dir, self.fuchsia_dir)

    def test_cli_list_and_active(self) -> None:
        # Test 'list' subcommand
        buf = io.StringIO()
        with redirect_stdout(buf):
            ret = self.manager.main(
                [f"--fuchsia-dir={self.fuchsia_dir}", "list"], {}
            )
        self.assertEqual(ret, 0)
        output = buf.getvalue().splitlines()
        self.assertIn("custom", output)
        self.assertIn("default", output)

        # Test 'active' subcommand before selection
        buf = io.StringIO()
        with redirect_stdout(buf):
            ret = self.manager.main(
                [f"--fuchsia-dir={self.fuchsia_dir}", "active"], {}
            )
        self.assertEqual(ret, 0)
        self.assertEqual(buf.getvalue().strip(), "default")

        # Test 'select' subcommand
        buf = io.StringIO()
        with redirect_stdout(buf):
            ret = self.manager.main(
                [f"--fuchsia-dir={self.fuchsia_dir}", "select", "custom"], {}
            )
        self.assertEqual(ret, 0)
        self.assertIn("MYSERVICE profile set to: custom", buf.getvalue())

        # Test 'active' after selection
        buf = io.StringIO()
        with redirect_stdout(buf):
            ret = self.manager.main(
                [f"--fuchsia-dir={self.fuchsia_dir}", "active"], {}
            )
        self.assertEqual(ret, 0)
        self.assertEqual(buf.getvalue().strip(), "custom")

    def test_cli_run_subcommand(self) -> None:
        with mock.patch("os.execvpe") as mock_exec:
            self.manager.main(
                [
                    f"--fuchsia-dir={self.fuchsia_dir}",
                    "run",
                    "--",
                    "echo",
                    "hi",
                ],
                {"EXISTING": "1"},
            )
            mock_exec.assert_called_once()
            cmd, args, env = mock_exec.call_args[0]
            self.assertEqual(cmd, "echo")
            self.assertEqual(args, ["echo", "hi"])
            self.assertEqual(env["MY_ENDPOINT"], "https://default.example.com")
            self.assertEqual(env["EXISTING"], "1")

    def test_cli_auth_type_subcommand(self) -> None:
        buf = io.StringIO()
        with redirect_stdout(buf):
            ret = self.manager.main(
                [f"--fuchsia-dir={self.fuchsia_dir}", "auth-type"], {}
            )
        self.assertEqual(ret, 0)
        self.assertEqual(buf.getvalue().strip(), "none")


if __name__ == "__main__":
    unittest.main()

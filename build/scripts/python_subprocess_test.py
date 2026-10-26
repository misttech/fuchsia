# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for python_subprocess.py."""

import os
import pathlib
import subprocess
import unittest
from unittest import mock

from build.scripts import python_subprocess


class PythonSubprocessTest(unittest.TestCase):
    """Unit tests for python_subprocess module functions."""

    def setUp(self) -> None:
        self.source_dir = pathlib.Path("/test/fuchsia")

    def test_prepend_env_path_sets_when_missing(self) -> None:
        env: dict[str, str] = {}
        python_subprocess._prepend_env_path(env, "PATH", "/bin")
        self.assertEqual(env["PATH"], "/bin")

    def test_prepend_env_path_prepends_when_absent(self) -> None:
        env = {"PATH": "/usr/bin"}
        python_subprocess._prepend_env_path(env, "PATH", "/bin")
        self.assertEqual(env["PATH"], f"/bin{os.pathsep}/usr/bin")

    def test_prepend_env_path_moves_existing_to_front(self) -> None:
        env = {"PATH": f"/usr/bin{os.pathsep}/bin{os.pathsep}/opt/bin"}
        python_subprocess._prepend_env_path(env, "PATH", "/bin")
        self.assertEqual(
            env["PATH"], f"/bin{os.pathsep}/usr/bin{os.pathsep}/opt/bin"
        )

    def test_curate_python_env_default(self) -> None:
        base_env = {"USER": "developer"}
        env = python_subprocess._curate_python_env(
            source_dir=self.source_dir,
            base_env=base_env,
        )
        self.assertEqual(env["USER"], "developer")
        self.assertEqual(env["PYTHONPATH"], str(self.source_dir))

    def test_curate_python_env_empty_source_dir_raises(self) -> None:
        with self.assertRaises(ValueError):
            python_subprocess._curate_python_env(
                source_dir=pathlib.Path(""),
            )

    def test_curate_python_env_prepends_source_dir(self) -> None:
        base_env = {"PYTHONPATH": "/custom/path"}
        env = python_subprocess._curate_python_env(
            source_dir=self.source_dir,
            base_env=base_env,
        )
        expected = f"{self.source_dir}{os.pathsep}/custom/path"
        self.assertEqual(env["PYTHONPATH"], expected)

    def test_curate_python_env_moves_existing_source_dir_to_front(self) -> None:
        base_env = {"PYTHONPATH": f"/other/path{os.pathsep}{self.source_dir}"}
        env = python_subprocess._curate_python_env(
            source_dir=self.source_dir,
            base_env=base_env,
        )
        expected = f"{self.source_dir}{os.pathsep}/other/path"
        self.assertEqual(env["PYTHONPATH"], expected)

    def test_curate_python_env_inherits_ambient_pycacheprefix(self) -> None:
        base_env = {"PYTHONPYCACHEPREFIX": "/custom/cache"}
        env = python_subprocess._curate_python_env(
            source_dir=self.source_dir,
            base_env=base_env,
        )
        self.assertEqual(env["PYTHONPYCACHEPREFIX"], "/custom/cache")

    def test_prepare_cmd_and_env_prepends_binary_and_flag(self) -> None:
        cmd, env = python_subprocess._prepare_cmd_and_env(
            cmd=["script.py", "arg1"],
            source_dir=self.source_dir,
            base_env={},
        )
        self.assertEqual(
            cmd, [str(python_subprocess.PYTHON_BIN), "-S", "script.py", "arg1"]
        )
        self.assertEqual(env["PYTHONPATH"], str(self.source_dir))

    def test_prepare_cmd_and_env_retains_existing_binary(self) -> None:
        input_cmd = [str(python_subprocess.PYTHON_BIN), "-S", "-u", "script.py"]
        cmd, env = python_subprocess._prepare_cmd_and_env(
            cmd=input_cmd,
            source_dir=self.source_dir,
            base_env={},
        )
        self.assertEqual(cmd, input_cmd)

    def test_prepare_cmd_and_env_inserts_s_flag_when_binary_present(
        self,
    ) -> None:
        input_cmd = [str(python_subprocess.PYTHON_BIN), "script.py"]
        cmd, env = python_subprocess._prepare_cmd_and_env(
            cmd=input_cmd,
            source_dir=self.source_dir,
            base_env={},
        )
        self.assertEqual(
            cmd, [str(python_subprocess.PYTHON_BIN), "-S", "script.py"]
        )

    def test_prepare_cmd_and_env_pops_text_kwargs(self) -> None:
        popen_kwargs = {
            "text": True,
            "universal_newlines": True,
            "timeout": 5.0,
        }
        cmd, env = python_subprocess._prepare_cmd_and_env(
            cmd=["script.py"],
            source_dir=self.source_dir,
            base_env={},
            popen_kwargs=popen_kwargs,
        )
        self.assertNotIn("text", popen_kwargs)
        self.assertNotIn("universal_newlines", popen_kwargs)
        self.assertEqual(popen_kwargs["timeout"], 5.0)

    def test_prepare_cmd_and_env_merges_popen_env(self) -> None:
        popen_kwargs = {"env": {"EXTRA": "VALUE"}}
        cmd, env = python_subprocess._prepare_cmd_and_env(
            cmd=["script.py"],
            source_dir=self.source_dir,
            base_env={},
            popen_kwargs=popen_kwargs,
        )
        self.assertEqual(env["EXTRA"], "VALUE")
        self.assertEqual(env["PYTHONPATH"], str(self.source_dir))

    def test_prepare_cmd_and_env_ignores_none_env(self) -> None:
        popen_kwargs = {"env": None}
        cmd, env = python_subprocess._prepare_cmd_and_env(
            cmd=["script.py"],
            source_dir=self.source_dir,
            base_env={"USER": "test"},
            popen_kwargs=popen_kwargs,
        )
        self.assertEqual(env["USER"], "test")
        self.assertEqual(env["PYTHONPATH"], str(self.source_dir))

    @mock.patch.object(
        subprocess,
        "run",
        return_value=subprocess.CompletedProcess(
            args=["cmd"], returncode=0, stdout="  result\n  "
        ),
    )
    def test_check_output_returns_stripped_stdout(
        self, mock_run: mock.Mock
    ) -> None:
        output = python_subprocess.check_output(
            cmd=["helper.py"],
            source_dir=self.source_dir,
            base_env={"USER": "test"},
            timeout=10.0,
            stderr=subprocess.PIPE,
        )
        self.assertEqual(output, "result")
        mock_run.assert_called_once_with(
            [str(python_subprocess.PYTHON_BIN), "-S", "helper.py"],
            text=True,
            timeout=10.0,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=True,
            env={
                "USER": "test",
                "PYTHONPATH": str(self.source_dir),
            },
        )

    @mock.patch.object(
        subprocess,
        "run",
        return_value=subprocess.CompletedProcess(
            args=["cmd"], returncode=0, stdout="out"
        ),
    )
    def test_run_invokes_run(self, mock_run: mock.Mock) -> None:
        proc = python_subprocess.run(
            cmd=["helper.py", "--check"],
            source_dir=self.source_dir,
            base_env={"USER": "test"},
            check=True,
        )
        self.assertEqual(proc.stdout, "out")
        mock_run.assert_called_once_with(
            [str(python_subprocess.PYTHON_BIN), "-S", "helper.py", "--check"],
            text=True,
            check=True,
            env={
                "USER": "test",
                "PYTHONPATH": str(self.source_dir),
            },
        )


if __name__ == "__main__":
    unittest.main()

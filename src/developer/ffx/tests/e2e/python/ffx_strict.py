#!/usr/bin/env fuchsia-vendored-python
# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Simple FFX host tool E2E test."""

import inspect
import json
import logging
import os
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path
from typing import Any, List, Optional, Text, Tuple

import ffxtestcase
from mobly import asserts, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)


def parse_json_messages(output: str) -> list[dict[str, Any]]:
    decoder = json.JSONDecoder()
    messages = []
    position = 0
    while position < len(output):
        (message, read) = decoder.raw_decode(output[position:])
        messages.append(message)
        position = position + read
    return messages


class FfxStrictTest(ffxtestcase.FfxTestCase):
    """FFX host tool E2E test For Strict."""

    async def setup_class(self) -> None:
        await super().setup_class()
        self.dut_ssh_address = self.dut.ffx.get_target_address()
        self.dut_name = self.dut.ffx.get_target_name()
        self.ssh_private_key: Optional[str] = None

    # Return list of ["key=val"]
    def _get_configs(self, keys: List[str]) -> List[str]:
        outputs = []
        for key in keys:
            output = json.loads(
                self.run_ffx(["config", "get", "-s", "first", key])
            )
            asserts.assert_true(
                isinstance(output, Text),
                f"Value for {key} is not a string: {output}",
            )
            output = output.strip().replace('"', "")
            outputs.append(f"{key}={output}")
        return outputs

    # Look up and store the user's private key
    def _get_ssh_private_key(self) -> None:
        if self.ssh_private_key:
            return
        ssh_keys = self.dut.ffx.config.ssh_private_keys
        if isinstance(ssh_keys, list) and ssh_keys and ssh_keys[0]:
            self.ssh_private_key = ssh_keys[0]
            return
        elif isinstance(ssh_keys, str) and ssh_keys:
            self.ssh_private_key = ssh_keys
            return
        ssh_priv_output = json.loads(
            self.run_ffx(["config", "get", "-s", "first", "ssh.priv"])
        )
        ssh_priv = ""
        if isinstance(ssh_priv_output, List):
            ssh_priv = ssh_priv_output[0].strip().replace('"', "")
        elif isinstance(ssh_priv_output, Text):
            ssh_priv = ssh_priv_output.strip().replace('"', "")

        self.ssh_private_key = ssh_priv

    # Build the default configs passed to strict invocations of ffx
    def _build_strict_configs(self, extra_configs: List[str]) -> List[str]:
        environ = os.environ
        configs: List[str] = []
        # Get output directory
        out_dir = os.environ.get("TEST_UNDECLARED_OUTPUTS_DIR")
        caller_frame = inspect.currentframe()
        while caller_frame and caller_frame.f_code.co_name.startswith("_"):
            caller_frame = caller_frame.f_back
        if out_dir and caller_frame:
            out_dir = os.path.join(
                out_dir, f"{caller_frame.f_code.co_name}.log"
            )
        if not out_dir:
            out_dir = "/dev/null"
        _LOGGER.info(f"Setting ffx config log dir to {out_dir}")

        # Get other required configs:
        #
        # ffx.subtool-search-paths: Subtool path, for externally-compiled subtools
        # test.output_path: Output path for certain tests
        # ssh.priv: path to private key for ssh connection to target
        # fastboot.devices_file.path: path for discovering Pontis fastboot devices
        # log.dir: directory to write logs
        # shared_data: directory for data shared among programs
        ffx_path = self.dut.ffx.config.binary_path
        subtool_path = Path(ffx_path).resolve().parent
        configs.append(f"ffx.subtool-search-paths={subtool_path}")
        configs.append(f"test.output_path={out_dir}")
        self._get_ssh_private_key()
        configs.append(f"ssh.priv={self.ssh_private_key}")
        configs.append(
            f"fastboot.devices_file.path={environ['HOME']}/.fastboot/devices"
        )
        configs.append(f"log.dir={environ['FUCHSIA_TEST_OUTDIR']}/ffx_logs")
        if self.dut.ffx.config.shared_data:
            configs.append(f"shared_data={self.dut.ffx.config.shared_data}")
        else:
            configs.append(f"shared_data={environ['FUCHSIA_TEST_OUTDIR']}/ffx")
        configs.extend(extra_configs)
        return configs

    def _format_strict_config_args(self, configs: List[str]) -> List[str]:
        # Return as list of args: ["-c", "key1=val1", "-c", "key2=val2", ...]
        args: List[str] = []
        for config in configs:
            args.extend(["-c", config])
        return args

    # Build the default config arguments passed to strict invocations of ffx
    def _build_strict_config_args(self, extra_configs: List[str]) -> List[str]:
        return self._format_strict_config_args(
            self._build_strict_configs(extra_configs)
        )

    # Run ffx --strict <cmd> with the specified configs, and
    # optionally with a target
    def _run_strict_ffx_with_configs(
        self, cmd: List[str], config_args: List[str], target: Optional[str]
    ) -> Any:
        all_args = [
            "--strict",
            "--machine",
            "json",
            "-o",
            "/dev/null",
            *config_args,
        ]
        if target is not None:
            all_args += ["-t", target]
        all_args += cmd
        return json.loads(self.run_ffx(all_args))

    def _run_strict_ffx_unchecked_with_format(
        self,
        cmd: List[str],
        target: Optional[str],
        format: str,
        config_args: Optional[List[str]] = None,
    ) -> Tuple[int, str, str]:
        if config_args is None:
            config_args = self._build_strict_config_args([])
        all_args = [
            "--strict",
            "--machine",
            format,
            "-o",
            "/dev/null",
            *config_args,
        ]
        if target is not None:
            all_args += ["-t", target]
        all_args += cmd
        (code, stdout, stderr) = self.run_ffx_unchecked(all_args)
        return (code, stdout.strip().replace("\n", ""), stderr)

    def _run_strict_ffx_unchecked(
        self,
        cmd: List[str],
        target: Optional[str] = None,
        config_args: Optional[List[str]] = None,
    ) -> Tuple[int, str, str]:
        return self._run_strict_ffx_unchecked_with_format(
            cmd, target, "json", config_args=config_args
        )

    # Run ffx --strict <cmd> with the default configs, and
    # optionally with a target
    def _run_strict_ffx(
        self, cmd: List[str], target: Optional[str] = None
    ) -> Any:
        return self._run_strict_ffx_with_configs(
            cmd, self._build_strict_config_args([]), target
        )

    def test_strict_errors_with_target_name(self) -> None:
        """Test `ffx --strict target echo` fails when attempt discovery."""
        with asserts.assert_raises(subprocess.CalledProcessError):
            self._run_strict_ffx(
                [
                    "target",
                    "echo",
                    "From a Test",
                ],
                self.dut_name,
            )

    def test_strict_can_check_for_no_target(self) -> None:
        """Test `ffx --strict target echo` requires a target."""
        (code, stdout, stderr) = self._run_strict_ffx_unchecked(
            ["target", "echo"], None
        )
        message = json.loads(stdout)
        asserts.assert_equal(stderr, "")
        asserts.assert_equal(message["type"], "user")
        asserts.assert_equal(message["code"], 1)
        asserts.assert_equal(
            message["message"],
            "Command line flags unsatisfactory for strict mode:\n\tffx strict requires that the target be explicitly specified. Specify `--target <target>`.",
        )

    def test_strict_can_accept_no_target(self) -> None:
        """Test `ffx --strict product download` doesn't require a target."""
        with asserts.assert_raises(subprocess.CalledProcessError):
            try:
                self._run_strict_ffx(
                    ["product", "download", "http://0.0.0.0:12345", "foo"], None
                )
            except subprocess.CalledProcessError as e:
                asserts.assert_false(
                    b"ffx strict requires that the target be explicitly specified"
                    in e.stderr,
                    "The command should not require a target",
                )
                raise

    def test_strict_may_require_shared_data(self) -> None:
        """Test `ffx --strict` will error out if $SHARED_DATA is required but not supplied"""
        strict_configs = [
            c
            for c in self._build_strict_configs([])
            if not c.startswith("shared_data=")
        ]
        (_code, stdout, _stderr) = self._run_strict_ffx_unchecked(
            ["repository", "server", "list"],
            target=f"{self.dut_ssh_address}",
            config_args=self._format_strict_config_args(strict_configs),
        )

        # We can't actually load the output into json, because `ffx repository server list`
        # has technically not been ported to ffx-strict, and it can produce multiple error
        # lines, which is not valid JSON. But it is the only built-in plugin that requires
        # SHARED_DATA, so it is the tool we are using here.
        asserts.assert_true(
            "SHARED_DATA must be specified in strict mode" in stdout,
            "Expected a message about SHARED_DATA in strict",
        )

    def test_strict_uses_shared_data(self) -> None:
        """Test `ffx --strict` will use the value passed in for $SHARED_DATA"""
        strict_configs = [
            "shared_data=foo" if c.startswith("shared_data=") else c
            for c in self._build_strict_configs([])
        ]
        (_code, stdout, _stderr) = self._run_strict_ffx_unchecked_with_format(
            ["config", "get", "monitor.pid_file"],
            None,
            "raw",
            config_args=self._format_strict_config_args(strict_configs),
        )
        asserts.assert_true(
            "foo/monitor" in stdout,
            "Expected SHARED_DATA to be correctly set in strict",
            stdout,
        )

    def test_target_list_strict_fails(self) -> None:
        """Test `ffx --strict target list` correctly reports RCS=N."""
        emu_config = (
            [f"emu.instance_dir={self.dut.ffx.config.emu_instance_dir}"]
            if self.dut.ffx.config.emu_instance_dir
            else self._get_configs(["emu.instance_dir"])
        )
        # Ensure that we cannot find the ssh.priv file
        new_configs = [
            c + "NONEXISTENT" if c.startswith("ssh.priv=") else c
            for c in self._build_strict_configs(emu_config)
        ]
        output = self._run_strict_ffx_with_configs(
            [
                "target",
                "list",
                self.dut_name,
            ],
            self._format_strict_config_args(new_configs),
            None,
        )
        asserts.assert_equal(output[0]["rcs_state"], "N")

    def test_target_list_strict_omitted_ssh_priv(self) -> None:
        """Test `ffx --strict target list` reports an error if ssh.priv is omitted."""
        emu_config = (
            [f"emu.instance_dir={self.dut.ffx.config.emu_instance_dir}"]
            if self.dut.ffx.config.emu_instance_dir
            else self._get_configs(["emu.instance_dir"])
        )
        # Filter out ssh.priv completely
        new_config_args = self._format_strict_config_args(
            [
                c
                for c in self._build_strict_configs(emu_config)
                if not c.startswith("ssh.priv=")
            ]
        )
        (code, stdout, _stderr) = self._run_strict_ffx_unchecked(
            ["target", "list", self.dut_name],
            config_args=new_config_args,
        )
        asserts.assert_equal(code, 1)
        message = json.loads(stdout)
        asserts.assert_equal(message["type"], "user")
        asserts.assert_equal(message["code"], 1)
        asserts.assert_equal(
            message["message"],
            "Required argument `-c ssh.priv=` not specified. Unable to verify RCS state.",
        )

        # But with --no-probe, omitting ssh.priv succeeds
        (code, stdout, _stderr) = self._run_strict_ffx_unchecked(
            ["target", "list", "--no-probe", self.dut_name],
            config_args=new_config_args,
        )
        asserts.assert_equal(code, 0)

    def test_target_wait_strict(self) -> None:
        """Test `ffx --strict target wait`."""
        output = self._run_strict_ffx(
            [
                "target",
                "wait",
            ],
            f"{self.dut_ssh_address}",
        )
        asserts.assert_equal(output, {"ok": {}})

    def test_target_wait_down_strict(self) -> None:
        """Test `ffx --strict target wait --down`."""
        (code, stdout, stderr) = self._run_strict_ffx_unchecked(
            [
                "target",
                "wait",
                "--timeout",
                "1",
                "--down",
            ],
            f"{self.dut_ssh_address}",
        )
        # the raw json decoder doesnt like whitespace or newlines
        asserts.assert_equal(stderr, "")
        messages = parse_json_messages(stdout)
        # We'll grab the last message to parse
        message = messages[-1]
        asserts.assert_equal(message["type"], "user")
        asserts.assert_equal(message["code"], 1)
        asserts.assert_equal(code, 1)

    def test_target_snapshot_destination_annotations(self) -> None:
        """Test `ffx --strict target snapshot` with incorrect dir."""
        (_code, stdout, _stderr) = self._run_strict_ffx_unchecked(
            [
                "target",
                "snapshot",
                "--dump-annotations",
            ],
            f"{self.dut_ssh_address}",
        )
        messages = parse_json_messages(stdout)
        message = messages[-1]
        asserts.assert_true(
            message.get("annotations") is not None,
            f"JSON message doesn't contain 'annotations': {message}",
        )
        asserts.assert_true(
            "board" in message["annotations"]["annotations"],
            f"message check failed: {message}",
        )

    def test_target_snapshot_destination_not_directory(self) -> None:
        """Test `ffx --strict target snapshot` with incorrect dir."""
        with tempfile.NamedTemporaryFile() as tmp:
            tmp.write(b"blah blah blah")
            (_code, stdout, _stderr) = self._run_strict_ffx_unchecked(
                [
                    "target",
                    "snapshot",
                    "-d",
                    tmp.name,
                ],
                f"{self.dut_ssh_address}",
            )
            messages = parse_json_messages(stdout)
            message = messages[-1]
            asserts.assert_true(
                message.get("user_error") is not None,
                f"JSON message doesn't contain 'user_error': {message}",
            )
            asserts.assert_true(
                "not a directory" in message["user_error"]["message"],
                f"message check failed: {message}",
            )
            asserts.assert_true(
                tmp.name in message["user_error"]["message"],
                f"message check failed: {message}",
            )

    def test_target_snapshot_e2e(self) -> None:
        """Test `ffx --strict target snapshot` with incorrect dir."""
        with tempfile.TemporaryDirectory() as tmpdir:
            (_code, stdout, _stderr) = self._run_strict_ffx_unchecked(
                [
                    "target",
                    "snapshot",
                    "-d",
                    tmpdir,
                ],
                f"{self.dut_ssh_address}",
            )
            messages = parse_json_messages(stdout)
            message = messages[-1]
            # Example expected output:
            # {"snapshot":{"output_file":"/tmp/snapshots/20250218_150248/snapshot.zip"}}
            asserts.assert_true(
                message.get("snapshot") is not None,
                f"JSON message doesn't contain 'snapshot': {message}",
            )
            asserts.assert_true(
                "zip" in message["snapshot"]["output_file"],
                f"message check failed: {message}",
            )
            output_file = message["snapshot"]["output_file"]
            output_file_size = os.path.getsize(output_file)
            asserts.assert_true(output_file_size > 0, "output file is empty")
            # Example unzip of snapshot contents:
            # Archive:  snapshot.zip
            #   inflating: annotations.json
            #   inflating: build.kernel-boot-options.txt
            #   inflating: inspect.json
            #   inflating: log.kernel.txt
            #   inflating: log.system.txt
            #   inflating: metadata.json
            with zipfile.ZipFile(output_file, "r") as output_file_zip:
                info = output_file_zip.infolist()
                checks = [
                    "annotations.json",
                    "build.kernel-boot-options.txt",
                    "inspect.json",
                    "log.kernel.txt",
                    "log.system.txt",
                    "metadata.json",
                ]
                for check in checks:
                    asserts.assert_true(
                        any(map(lambda x: x.filename == check, info)),
                        f"Expected `{check}` in snapshot: {info}",
                    )

    def test_target_ssh_strict(self) -> None:
        """Test `ffx --strict target ssh`."""
        (_code, stdout, _stderr) = self._run_strict_ffx_unchecked_with_format(
            [
                "target",
                "ssh",
                "echo",
                "foo",
            ],
            f"{self.dut_ssh_address}",
            "raw",
        )
        asserts.assert_equal(stdout, "foo")

    def test_strict_example_script(self) -> None:
        """Runs the strict.py example script and checks its execution."""
        self._get_ssh_private_key()
        asserts.assert_is_not_none(self.ssh_private_key)
        # Standard assert is required to narrow type from Optional[str] to str for Mypy.
        assert self.ssh_private_key is not None

        # Locate the test data directory.
        ffx_path = self.dut.ffx.config.binary_path
        test_data_dir = Path(ffx_path).parent / "test_data" / "strict_example"
        if not test_data_dir.exists():
            # Fallback to searching under the build directory when ffx_path points
            # to a symlink directory like host-tools instead of the concrete toolchain dir.
            build_dir = Path(ffx_path).parent.parent
            for path in build_dir.iterdir():
                if (
                    path.name.startswith("host_")
                    and (path / "test_data" / "strict_example").exists()
                ):
                    test_data_dir = path / "test_data" / "strict_example"
                    break

        asserts.assert_true(
            test_data_dir.exists(),
            f"Could not locate strict_example test data directory under {Path(ffx_path).parent}",
        )
        strict_py = test_data_dir / "strict.py"

        # Prepare environment variables for strict.py
        environ = dict(os.environ)
        environ["FUCHSIA_NODENAME"] = str(self.dut_ssh_address)
        environ["FUCHSIA_SSH_KEY"] = self.ssh_private_key
        # Add ffx binary directory to PATH so the script's `subprocess.run(["ffx", ...])` works.
        environ["PATH"] = os.pathsep.join(
            filter(None, [str(Path(ffx_path).parent), environ.get("PATH")])
        )

        # Run the strict.py script in a temporary directory to avoid writing log files
        # to a potentially read-only test data directory.
        with tempfile.TemporaryDirectory() as tmp_dir:
            _LOGGER.info(
                "Running strict.py example script in %s: %s", tmp_dir, strict_py
            )
            result = subprocess.run(
                [sys.executable, str(strict_py)],
                capture_output=True,
                text=True,
                env=environ,
                cwd=tmp_dir,
            )

        _LOGGER.info("strict.py stdout:\n%s", result.stdout)
        _LOGGER.info("strict.py stderr:\n%s", result.stderr)

        asserts.assert_equal(
            result.returncode,
            0,
            f"strict.py failed with exit code {result.returncode}.\nStderr:\n{result.stderr}",
        )


if __name__ == "__main__":
    test_runner.main()

# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""ADB Random Command Stress Test."""

import asyncio
import hashlib
import logging
import os
import random
import subprocess
import tempfile
import uuid
from collections.abc import Callable, Coroutine
from typing import Any

import fuchsia_base_test
from honeydew.device_classes import android_device
from honeydew.transports.adb import errors as adb_errors
from mobly import asserts, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)


class AdbRandomStressTest(fuchsia_base_test.FuchsiaBaseTest):
    """Mobly test for running random ADB commands from an extended pool.

    Picks randomly from an extended pool of operations spanning connectivity,
    transport state, shell variants, and file transfer modes, with weighted
    selection favouring non-disruptive commands while ensuring reproducible
    execution via a seedable RNG.

    Non-disruptive commands (shell execution, push/pull transfers, and state
    queries that run over the active ADB connection) are weighted higher than
    disruptive operations that reset the transport or reboot the device
    (reboot, host ADB server restart, and root/unroot adbd restarts). This
    ensures the test execution budget is not dominated by ~60s device reboots
    and that most iterations exercise back-to-back data and command streams
    interspersed with occasional transport resets.

    Required Mobly Test Params:
        num_iterations (int): Number of random commands to run.

    Optional Mobly Test Params:
        random_seed (int): Seed for the random number generator for test
            reproducibility.
    """

    dut: android_device.AndroidDevice

    async def pre_run(self) -> None:
        test_arg_tuple_list: list[tuple[int]] = []
        num_iterations = int(self.user_params.get("num_iterations", 20))
        for iteration in range(1, num_iterations + 1):
            test_arg_tuple_list.append((iteration,))

        self.generate_tests(
            test_logic=self._test_logic,
            name_func=self._name_func,
            arg_sets=test_arg_tuple_list,
        )

    async def setup_class(self) -> None:
        await super().setup_class()
        assert isinstance(self.dut, android_device.AndroidDevice)
        # Ensure ADB is supported and enabled on this device.
        _ = self.dut.adb

        # TODO(b/564946370): Remove this unbind once AndroidDevice.adb's
        # root/unroot/use_adb_root match Honeydew ADB transport.
        for attr in ("root", "unroot", "use_adb_root"):
            self.dut.adb.__dict__.pop(attr, None)

        # Seed the random generator for reproducibility.
        seed = self.user_params.get("random_seed")
        if seed is None:
            seed = random.randrange(1000000)
        else:
            seed = int(seed)
        _LOGGER.info("Using random seed: %d", seed)
        random.seed(seed)

        # Determine expected device serial number for validation.
        assert (
            hasattr(self.dut.adb, "_serial_number")
            and self.dut.adb._serial_number
        ), "DUT ADB transport missing valid _serial_number"
        self._expected_serial: str = self.dut.adb._serial_number
        _LOGGER.info("Expected device serial: %s", self._expected_serial)

        # Build weighted list of candidate operations: (method, weight).
        self._operations: list[
            tuple[Callable[[], Coroutine[Any, Any, None]], int]
        ] = []

        # Category 1: Connectivity and transport state.
        self._operations.extend(
            [
                (self._cmd_wait_for_device, 10),
                (self._cmd_get_state, 10),
                (self._cmd_get_serialno, 10),
                (self._cmd_devices_l, 10),
            ]
        )
        if self.dut.adb._adb_server is not None:
            self._operations.append((self._cmd_server_restart, 1))

        # Category 2: Shell variants beyond echo.
        self._operations.extend(
            [
                (self._cmd_shell_echo, 10),
                (self._cmd_shell_exit_code, 10),
                (self._cmd_shell_stdin_pipe, 10),
            ]
        )

        self._operations.append((self._cmd_shell_large_stdout, 5))

        # Probe getprop support.
        try:
            self.dut.adb.run(["shell", "getprop", "sys.boot_completed"])
            self._operations.append((self._cmd_shell_getprop, 10))
            _LOGGER.info("ADB shell getprop is supported")
        except Exception as e:
            _LOGGER.warning("ADB getprop probe failed: %s", e)

        # Probe dumpsys usb support.
        try:
            self.dut.adb.run(["shell", "dumpsys", "usb"])
            self._operations.append((self._cmd_shell_dumpsys_usb, 5))
            _LOGGER.info("ADB shell dumpsys usb is supported")
        except Exception as e:
            _LOGGER.warning("ADB dumpsys usb probe failed: %s", e)

        # Category 3: File transfer variants.
        self._operations.extend(
            [
                (self._cmd_push_pull, 10),
                (self._cmd_push_pull_zero_length, 10),
                (self._cmd_push_pull_large, 5),
                (self._cmd_push_pull_unicode, 10),
                (self._cmd_push_pull_directory, 5),
            ]
        )

        # Probe push compression algorithms.
        self._supported_compressions: list[str] = []
        with tempfile.TemporaryDirectory() as probe_tmp:
            probe_file = os.path.join(probe_tmp, "probe.txt")
            with open(probe_file, "w", encoding="utf-8") as f:
                f.write("probe compression payload\n")
            probe_remote = "/tmp/adb_probe_comp.txt"
            for algo in ("brotli", "lz4", "zstd"):
                try:
                    self.dut.adb.run(
                        ["push", "-z", algo, probe_file, probe_remote]
                    )
                    self._supported_compressions.append(algo)
                    _LOGGER.info("ADB push compression '%s' is supported", algo)
                except Exception as e:
                    _LOGGER.debug(
                        "ADB push compression '%s' not supported: %s", algo, e
                    )
                finally:
                    try:
                        self.dut.adb.run(["shell", "rm", "-f", probe_remote])
                    except Exception:
                        pass

        if self._supported_compressions:
            self._operations.append((self._cmd_push_pull_compressed, 5))

        # Category 4: Lifecycle operations (weighted lower).
        self._operations.extend(
            [
                (self._cmd_root_unroot_toggle, 3),
                (self._cmd_su_root_unroot_toggle, 3),
                (self._cmd_reboot, 1),
            ]
        )

        # Probe logcat support.
        try:
            self.dut.adb.run(["logcat", "-d"])
            self._operations.append((self._cmd_logcat, 5))
            _LOGGER.info("ADB logcat is supported")
        except Exception as e:
            _LOGGER.warning(
                "ADB logcat is not supported: %s. Skipping in random test.", e
            )

    async def _test_logic(self, iteration: int) -> None:
        """Selects and runs a weighted random ADB command."""
        methods = [op[0] for op in self._operations]
        weights = [op[1] for op in self._operations]
        cmd_method = random.choices(methods, weights=weights, k=1)[0]
        _LOGGER.info(
            "Iteration %d: Running random command: %s",
            iteration,
            cmd_method.__name__,
        )
        await cmd_method()

    def _name_func(self, iteration: int) -> str:
        return f"test_adb_random_cmd_{iteration}"

    # --- Individual Command Implementations ---

    async def _cmd_shell_echo(self) -> None:
        """Runs a shell echo command and asserts expected stdout."""
        _LOGGER.info("Running: adb shell echo")
        output = self.dut.adb.run(["shell", "echo", "hello"])
        asserts.assert_in(
            "hello",
            output,
            msg=f"Unexpected output from adb shell echo: {output}",
        )

    async def _cmd_reboot(self) -> None:
        """Reboots the device via ADB and waits for recovery."""
        _LOGGER.info("Running: adb reboot")
        self.dut.ffx.notify_intentional_disconnect()
        self.dut.adb.run(["reboot"])
        _LOGGER.info("Waiting for device to go offline...")
        await asyncio.to_thread(self.dut.wait_for_offline)
        _LOGGER.info("Waiting for device to go online...")
        await self.dut.wait_for_online()
        await self.dut.on_device_boot()
        _LOGGER.info("Device is back online after reboot.")

    async def _cmd_root_unroot_toggle(self) -> None:
        """Toggles between root and shell user via adb root/unroot."""
        _LOGGER.info("Running: adb root/unroot toggle")
        if self.dut.adb.is_su_root:
            self.dut.adb.su_unroot()
        output = self.dut.adb.run(["shell", "id"])
        if "uid=0(root)" in output:
            await self._unroot()
        else:
            await self._root()

    async def _root(self) -> None:
        _LOGGER.info("Switching to root")
        self.dut.adb.root()
        output = self.dut.adb.run(["shell", "id"])
        asserts.assert_in(
            "uid=0(root)",
            output,
            msg=f"Expected root user (uid=0) but got: {output}",
        )

    async def _unroot(self) -> None:
        _LOGGER.info("Switching to unroot")
        self.dut.adb.unroot()
        output = self.dut.adb.run(["shell", "id"])
        asserts.assert_in(
            "uid=2000(shell)",
            output,
            msg=f"Expected shell user (uid=2000) but got: {output}",
        )

    async def _cmd_su_root_unroot_toggle(self) -> None:
        """Toggles between su_root and su_unroot."""
        _LOGGER.info("Running: adb su_root/su_unroot toggle")
        if self.dut.adb.is_root:
            await self._unroot()
        if self.dut.adb.is_su_root:
            await self._su_unroot()
        else:
            await self._su_root()

    async def _su_root(self) -> None:
        _LOGGER.info("Switching to su_root")
        self.dut.adb.su_root()
        output = self.dut.adb.run(["shell", "id"])
        asserts.assert_in(
            "uid=0(root)",
            output,
            msg=f"Expected root user (uid=0) via su_root but got: {output}",
        )

    async def _su_unroot(self) -> None:
        _LOGGER.info("Switching to su_unroot")
        self.dut.adb.su_unroot()
        output = self.dut.adb.run(["shell", "id"])
        asserts.assert_in(
            "uid=2000(shell)",
            output,
            msg=(
                f"Expected shell user (uid=2000) after su_unroot but got: "
                f"{output}"
            ),
        )

    async def _cmd_push_pull(self) -> None:
        """Pushes a file to device, pulls it back, and verifies content."""
        _LOGGER.info("Running: adb push/pull")
        unique_token = uuid.uuid4().hex[:8]
        content = (
            f"Random content {random.randint(1000, 9999)} - {unique_token}\n"
        )
        remote_file = f"/tmp/adb_test_rand_{unique_token}.txt"

        with tempfile.TemporaryDirectory() as tmpdir:
            local_file = os.path.join(
                tmpdir, f"adb_test_rand_{unique_token}.txt"
            )
            with open(local_file, "w", encoding="utf-8") as f:
                f.write(content)

            try:
                self.dut.adb.run(["push", local_file, remote_file])

                # To test pull, ensure pulling into a clean state by removing
                # the local file first.
                os.remove(local_file)

                self.dut.adb.run(["pull", remote_file, local_file])
                with open(local_file, "r", encoding="utf-8") as f_in:
                    pulled_content = f_in.read()

                asserts.assert_equal(
                    pulled_content,
                    content,
                    msg=f"Content mismatch: {pulled_content} != {content}",
                )
            finally:
                # Clean up the remote file; local files in tmpdir are
                # automatically removed on exiting the with block.
                try:
                    self.dut.adb.run(["shell", "rm", "-f", remote_file])
                except Exception as e:
                    _LOGGER.warning(
                        "Failed to clean up remote file %s: %s",
                        remote_file,
                        e,
                    )

    async def _cmd_logcat(self) -> None:
        """Runs adb logcat -d and verifies non-empty output."""
        _LOGGER.info("Running: adb logcat -d")
        output = self.dut.adb.run(["logcat", "-d"])
        asserts.assert_is_not_none(output, msg="Logcat output was None")
        _LOGGER.info(
            "Logcat returned %d chars of output (preview: %s...)",
            len(output),
            output[:100],
        )

    # --- Additional Extended Operations ---

    async def _cmd_wait_for_device(self) -> None:
        """Runs adb wait-for-device and confirms device state."""
        _LOGGER.info("Running: adb wait-for-device")
        self.dut.adb.run(["wait-for-device"])
        state = self.dut.adb.run(["get-state"]).strip()
        asserts.assert_equal(
            state, "device", msg=f"Unexpected device state: {state}"
        )

    async def _cmd_get_state(self) -> None:
        """Verifies adb get-state output matches 'device'."""
        _LOGGER.info("Running: adb get-state")
        state = self.dut.adb.run(["get-state"]).strip()
        asserts.assert_equal(
            state, "device", msg=f"Device state is not 'device': {state}"
        )

    async def _cmd_get_serialno(self) -> None:
        """Verifies adb get-serialno matches expected serial."""
        _LOGGER.info("Running: adb get-serialno")
        serial = self.dut.adb.run(["get-serialno"]).strip()
        asserts.assert_equal(
            serial,
            self._expected_serial,
            msg=(
                f"Serial mismatch: expected {self._expected_serial}, "
                f"got {serial}"
            ),
        )

    async def _cmd_devices_l(self) -> None:
        """Runs adb devices -l and verifies target enumeration."""
        _LOGGER.info("Running: adb devices -l")
        output = self.dut.adb.run(
            ["devices", "-l"],
            include_serial=False,
        )
        if self._expected_serial:
            asserts.assert_in(
                self._expected_serial,
                output,
                msg=(
                    f"Device serial {self._expected_serial} not in "
                    "devices listing"
                ),
            )
        asserts.assert_not_in("offline", output)
        asserts.assert_not_in("unauthorized", output)

    async def _cmd_server_restart(self) -> None:
        """Restarts the host-side isolated ADB server and re-attaches."""
        _LOGGER.info("Running low-weight: isolated ADB server restart")
        adb_server = self.dut.adb._adb_server
        assert adb_server is not None
        adb_server.reset_restart_count()
        adb_server.restart()
        self.dut.adb.run(["wait-for-device"])
        state = self.dut.adb.run(["get-state"]).strip()
        asserts.assert_equal(state, "device")

    async def _cmd_shell_large_stdout(self) -> None:
        """Streams large stdout to verify buffered pipe transport handling."""
        _LOGGER.info("Running: adb shell large stdout streaming")
        output = self.dut.adb.run(["shell", "seq", "1", "1000"])
        lines = [line.strip() for line in output.strip().splitlines() if line]
        asserts.assert_equal(
            len(lines),
            1000,
            msg=f"Expected 1000 lines from large stdout, got {len(lines)}",
        )
        asserts.assert_equal(lines[-1], "1000")

    async def _cmd_shell_exit_code(self) -> None:
        """Verifies non-zero exit code propagation under stress."""
        _LOGGER.info("Running: adb shell non-zero exit code propagation")
        exit_code = random.choice([7, 42, 101])
        try:
            self.dut.adb.run(["shell", "sh", "-c", f'"exit {exit_code}"'])
            asserts.fail(
                f"Expected shell exit {exit_code} to raise AdbCommandError"
            )
        except adb_errors.AdbCommandError as e:
            asserts.assert_in(
                f"returncode = {exit_code}",
                str(e),
                msg=f"Exit code {exit_code} not reflected in returncode: {e}",
            )

    async def _cmd_shell_stdin_pipe(self) -> None:
        """Pipes stdin to non-interactive shell command and verifies output."""
        _LOGGER.info("Running: adb shell stdin pipe to cat")
        payload = f"stdin_stress_payload_{uuid.uuid4().hex}\n"
        cmd = self.dut.adb._build_adb_cmd(["shell", "cat"], include_serial=True)
        env = self.dut.adb._get_command_env()
        proc = subprocess.run(
            cmd,
            input=payload,
            capture_output=True,
            text=True,
            env=env,
        )
        asserts.assert_equal(
            proc.returncode,
            0,
            msg=(
                f"stdin cat command failed with rc={proc.returncode}: "
                f"{proc.stderr}"
            ),
        )
        asserts.assert_in(
            payload.strip(),
            proc.stdout,
            msg=f"stdin pipe content mismatch: {proc.stdout}",
        )

    async def _cmd_shell_getprop(self) -> None:
        """Runs adb shell getprop and verifies property output."""
        _LOGGER.info("Running: adb shell getprop")
        output = self.dut.adb.run(
            ["shell", "getprop", "sys.boot_completed"]
        ).strip()
        asserts.assert_in(
            output,
            ("1", "0"),
            msg=f"Unexpected sys.boot_completed property value: {output}",
        )

    async def _cmd_shell_dumpsys_usb(self) -> None:
        """Runs adb shell dumpsys usb to mirror developer options queries."""
        _LOGGER.info("Running: adb shell dumpsys usb")
        output = self.dut.adb.run(["shell", "dumpsys", "usb"])
        asserts.assert_true(
            len(output) > 0, msg="dumpsys usb returned empty output"
        )
        asserts.assert_in(
            "USB", output, msg="dumpsys usb output missing expected USB header"
        )

    async def _cmd_push_pull_zero_length(self) -> None:
        """Pushes and pulls a 0-byte file to verify boundary transfer."""
        _LOGGER.info("Running: adb push/pull zero-length file")
        token = uuid.uuid4().hex[:8]
        remote_file = f"/tmp/adb_zero_{token}.dat"

        with tempfile.TemporaryDirectory() as tmpdir:
            local_file = os.path.join(tmpdir, f"zero_{token}.dat")
            with open(local_file, "wb") as f:
                pass

            try:
                self.dut.adb.run(["push", local_file, remote_file])
                os.remove(local_file)
                self.dut.adb.run(["pull", remote_file, local_file])
                asserts.assert_equal(
                    os.path.getsize(local_file),
                    0,
                    msg="Pulled zero-length file has non-zero size",
                )
            finally:
                try:
                    self.dut.adb.run(["shell", "rm", "-f", remote_file])
                except Exception:
                    pass

    async def _cmd_push_pull_large(self) -> None:
        """Pushes and pulls a 512KB payload and verifies SHA256 checksum."""
        _LOGGER.info("Running: adb push/pull 512KB payload")
        token = uuid.uuid4().hex[:8]
        remote_file = f"/tmp/adb_large_{token}.bin"
        payload = os.urandom(512 * 1024)
        expected_hash = hashlib.sha256(payload).hexdigest()

        with tempfile.TemporaryDirectory() as tmpdir:
            local_file = os.path.join(tmpdir, f"large_{token}.bin")
            with open(local_file, "wb") as f:
                f.write(payload)

            try:
                self.dut.adb.run(["push", local_file, remote_file])
                os.remove(local_file)
                self.dut.adb.run(["pull", remote_file, local_file])
                with open(local_file, "rb") as f_in:
                    pulled_hash = hashlib.sha256(f_in.read()).hexdigest()
                asserts.assert_equal(
                    pulled_hash,
                    expected_hash,
                    msg="SHA256 checksum mismatch on large payload transfer",
                )
            finally:
                try:
                    self.dut.adb.run(["shell", "rm", "-f", remote_file])
                except Exception:
                    pass

    async def _cmd_push_pull_unicode(self) -> None:
        """Pushes and pulls a file with Unicode filename and content."""
        _LOGGER.info("Running: adb push/pull Unicode filename and content")
        token = uuid.uuid4().hex[:6]
        filename = f"adb_föö_bår_🚀_{token}.txt"
        remote_file = f"/tmp/{filename}"
        content = f"Unicode payload: Grüß Gott, 世界, 🚀, token={token}\n"

        with tempfile.TemporaryDirectory() as tmpdir:
            local_file = os.path.join(tmpdir, filename)
            with open(local_file, "w", encoding="utf-8") as f:
                f.write(content)

            try:
                self.dut.adb.run(["push", local_file, remote_file])
                os.remove(local_file)
                self.dut.adb.run(["pull", remote_file, local_file])
                with open(local_file, "r", encoding="utf-8") as f_in:
                    pulled_content = f_in.read()
                asserts.assert_equal(
                    pulled_content,
                    content,
                    msg="Unicode content mismatch across push/pull",
                )
            finally:
                try:
                    self.dut.adb.run(["shell", "rm", "-f", remote_file])
                except Exception:
                    pass

    async def _cmd_push_pull_directory(self) -> None:
        """Pushes and pulls an entire directory tree and verifies contents."""
        _LOGGER.info("Running: adb push/pull directory tree")
        token = uuid.uuid4().hex[:8]
        remote_dir = f"/tmp/adb_dir_{token}"

        with (
            tempfile.TemporaryDirectory() as src_dir,
            tempfile.TemporaryDirectory() as dst_dir,
        ):
            sub_dir = os.path.join(src_dir, "nested")
            os.makedirs(sub_dir, exist_ok=True)
            files_data = {
                "file1.txt": f"Content 1 - {token}\n",
                "file2.txt": f"Content 2 - {token}\n",
                os.path.join("nested", "file3.txt"): f"Nested 3 - {token}\n",
            }
            for rel_path, data in files_data.items():
                with open(
                    os.path.join(src_dir, rel_path), "w", encoding="utf-8"
                ) as f:
                    f.write(data)

            try:
                self.dut.adb.run(["push", src_dir, remote_dir])
                self.dut.adb.run(["pull", remote_dir, dst_dir])
                pulled_root = os.path.join(
                    dst_dir, os.path.basename(remote_dir)
                )
                for rel_path, data in files_data.items():
                    pulled_path = os.path.join(pulled_root, rel_path)
                    asserts.assert_true(
                        os.path.exists(pulled_path),
                        msg=f"Expected pulled file {pulled_path} missing",
                    )
                    with open(pulled_path, "r", encoding="utf-8") as f_in:
                        asserts.assert_equal(
                            f_in.read(),
                            data,
                            msg=f"Directory file mismatch in {rel_path}",
                        )
            finally:
                try:
                    self.dut.adb.run(["shell", "rm", "-rf", remote_dir])
                except Exception:
                    pass

    async def _cmd_push_pull_compressed(self) -> None:
        """Pushes with supported compression algorithm and verifies content."""
        if not self._supported_compressions:
            return
        algo = random.choice(self._supported_compressions)
        _LOGGER.info("Running: adb push -z %s", algo)
        token = uuid.uuid4().hex[:8]
        remote_file = f"/tmp/adb_comp_{algo}_{token}.txt"
        content = (f"Compressible payload {algo} {token}\n") * 500

        with tempfile.TemporaryDirectory() as tmpdir:
            local_file = os.path.join(tmpdir, f"comp_{token}.txt")
            with open(local_file, "w", encoding="utf-8") as f:
                f.write(content)

            try:
                self.dut.adb.run(["push", "-z", algo, local_file, remote_file])
                os.remove(local_file)
                self.dut.adb.run(["pull", remote_file, local_file])
                with open(local_file, "r", encoding="utf-8") as f_in:
                    pulled = f_in.read()
                asserts.assert_equal(
                    pulled,
                    content,
                    msg=f"Compressed transfer ({algo}) content mismatch",
                )
            finally:
                try:
                    self.dut.adb.run(["shell", "rm", "-f", remote_file])
                except Exception:
                    pass


if __name__ == "__main__":
    test_runner.main()

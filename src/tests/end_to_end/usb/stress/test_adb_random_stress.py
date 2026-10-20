# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""ADB Random Command Stress Test."""

import asyncio
import logging
import os
import random
import tempfile

import fuchsia_base_test
from honeydew.device_classes import android_device
from mobly import test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)


class AdbRandomStressTest(fuchsia_base_test.FuchsiaBaseTest):
    """Mobly test for running random ADB commands in a loop.

    Required Mobly Test Params:
        num_iterations (int): Number of random commands to run.

    Optional Mobly Test Params:
        random_seed (int): Seed for the random number generator for test reproducibility.
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
        # Ensure ADB is supported and enabled on this device before starting the test
        # (raises NotSupportedError or NotEnabledError otherwise).
        _ = self.dut.adb
        # TODO(b/564946370): Remove this unbind once AndroidDevice.adb's
        # temporary su_root monkey-patch is removed.
        for attr in ("root", "unroot", "use_adb_root"):
            self.dut.adb.__dict__.pop(attr, None)

        # Seed the random generator for reproducibility.
        # Allow seeding via user_params.
        seed = self.user_params.get("random_seed")
        if seed is None:
            # Generate a random seed
            seed = random.randrange(1000000)
        else:
            seed = int(seed)
        _LOGGER.info(f"Using random seed: {seed}")
        random.seed(seed)

        # Define all candidate command methods
        self._commands = [
            self._cmd_shell_echo,
            self._cmd_reboot,
            self._cmd_root_unroot_toggle,
            self._cmd_su_root_unroot_toggle,
            self._cmd_push_pull,
        ]

        # Check if logcat is supported
        try:
            self.dut.adb.run(["logcat", "-d"])
            self._commands.append(self._cmd_logcat)
            _LOGGER.info("ADB logcat is supported")
        except Exception as e:
            _LOGGER.warning(
                f"ADB logcat is not supported: {e}. Skipping in random test."
            )

    async def _test_logic(self, iteration: int) -> None:
        """Selects and runs a random ADB command."""
        cmd_method = random.choice(self._commands)
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
        _LOGGER.info("Running: adb shell echo")
        output = self.dut.adb.run(["shell", "echo", "hello"])
        if "hello" not in output:
            raise Exception(f"Unexpected output: {output}")

    async def _cmd_reboot(self) -> None:
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
        if "uid=0(root)" not in output:
            _LOGGER.warning(
                f"adb root did not switch to root user. output: {output}"
            )

    async def _unroot(self) -> None:
        _LOGGER.info("Switching to unroot")
        self.dut.adb.unroot()
        output = self.dut.adb.run(["shell", "id"])
        if "uid=2000(shell)" not in output:
            raise Exception(f"Expected shell user (uid=2000) but got: {output}")

    async def _cmd_su_root_unroot_toggle(self) -> None:
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
        if "uid=0(root)" not in output:
            raise Exception(
                f"Expected root user (uid=0) via su_root but got: {output}"
            )

    async def _su_unroot(self) -> None:
        _LOGGER.info("Switching to su_unroot")
        self.dut.adb.su_unroot()
        output = self.dut.adb.run(["shell", "id"])
        if "uid=2000(shell)" not in output:
            raise Exception(
                f"Expected shell user (uid=2000) after su_unroot but got: {output}"
            )

    async def _cmd_push_pull(self) -> None:
        _LOGGER.info("Running: adb push/pull")
        content = f"Random content {random.randint(1000, 9999)}"
        remote_file = "/tmp/adb_test_rand.txt"

        with tempfile.TemporaryDirectory() as tmpdir:
            local_file = os.path.join(tmpdir, "adb_test_rand.txt")
            with open(local_file, "w") as f:
                f.write(content)

            try:
                self.dut.adb.run(["push", local_file, remote_file])

                # To test pull, we want to make sure we are pulling to a clean state.
                # We can remove the local file first.
                os.remove(local_file)

                self.dut.adb.run(["pull", remote_file, local_file])
                with open(local_file, "r") as f_in:
                    pulled_content = f_in.read()

                if pulled_content != content:
                    raise Exception(
                        f"Content mismatch: {pulled_content} != {content}"
                    )
            finally:
                # We only need to clean up remote file here.
                # Local files in tmpdir are automatically cleaned up when exiting the 'with' block.
                try:
                    self.dut.adb.run(["shell", "rm", "-f", remote_file])
                except Exception as e:
                    _LOGGER.warning(f"Failed to clean up remote file: {e}")

    async def _cmd_logcat(self) -> None:
        _LOGGER.info("Running: adb logcat -d")
        output = self.dut.adb.run(["logcat", "-d"])
        # We just verify it returns something (or at least doesn't error)
        _LOGGER.info(
            f"Logcat returned {len(output)} chars of output (preview: {output[:100]}...)"
        )


if __name__ == "__main__":
    test_runner.main()

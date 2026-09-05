# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Host USB driver controller for loading and unloading kernel test drivers."""

import logging
import os
import shutil
import subprocess
from typing import Optional

_LOGGER: logging.Logger = logging.getLogger(__name__)


class UsbTestController:
    """Manages the host usbtest driver via DMC in infra or sudo modprobe locally."""

    def __init__(
        self,
        vendor: int = 0x18D1,
        product: int = 0xA022,
        alt: int = 0,
    ) -> None:
        self.vendor = vendor
        self.product = product
        self.alt = alt
        self._dmc_path: Optional[str] = shutil.which("dmc") or os.environ.get(
            "DMC_PATH"
        )

    def load_driver(self) -> None:
        """Load the usbtest driver with target VID/PID and alt setting."""
        if os.path.exists("/sys/module/usbtest"):
            _LOGGER.info("usbtest kernel module is already loaded.")
            return

        if self._dmc_path and os.path.exists(self._dmc_path):
            cmd = [
                self._dmc_path,
                "load-usbtest-driver",
                "-vendor",
                f"{self.vendor:#06x}",
                "-product",
                f"{self.product:#06x}",
                "-alt",
                str(self.alt),
            ]
            _LOGGER.info(f"Loading usbtest driver via DMC: {' '.join(cmd)}")
            try:
                subprocess.run(cmd, check=True, capture_output=True, text=True)
            except subprocess.CalledProcessError as e:
                _LOGGER.warning(
                    f"DMC load-usbtest-driver failed: {e.stderr or e}"
                )
                raise
        else:
            error_msg = (
                "\n========================================================================\n"
                "ERROR: Host kernel module 'usbtest' is not loaded!\n"
                "Automated non-interactive execution cannot prompt for 'sudo'.\n\n"
                "Please run tests using the desk test runner script:\n"
                "  ./src/tests/end_to_end/usb/functional/zero_function/run_zero_test_at_desk.sh --serial-socket <path>\n\n"
                "Or load the kernel driver manually on your host before testing:\n"
                "  sudo modprobe usbtest vendor=0x18d1 product=0xa022 alt=0\n"
                "========================================================================\n"
            )
            _LOGGER.error(error_msg)
            raise RuntimeError("usbtest kernel module is not loaded on host.")

    def unload_driver(self) -> None:
        """Unload the usbtest driver to restore host state."""
        if self._dmc_path and os.path.exists(self._dmc_path):
            cmd = [self._dmc_path, "unload-usbtest-driver"]
            _LOGGER.info(f"Unloading usbtest driver via DMC: {' '.join(cmd)}")
            try:
                subprocess.run(cmd, check=False, capture_output=True, text=True)
            except Exception as e:
                _LOGGER.debug(f"DMC unload-usbtest-driver failed: {e}")
        else:
            _LOGGER.info(
                "DMC is not present; skipping host kernel module unloading in test harness."
            )

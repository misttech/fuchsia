# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for Audio affordance."""

import asyncio
import logging
import os
import time
from datetime import timedelta

from fuchsia_base_test import fuchsia_base_test
from mobly import test_runner

from honeydew.affordances.virtual_audio.types import WaitForQuietResult
from honeydew.fuchsia_device import fuchsia_device

_AUDIO_FILE_INPUT = "audio_runtime_deps/sine_wave.wav"
_LOGGER: logging.Logger = logging.getLogger(__name__)


class AudioAffordanceTests(fuchsia_base_test.FuchsiaBaseTest):
    """Audio affordance tests"""

    def setup_class(self) -> None:
        """setup_class is called once before running tests.

        It does the following things:
            * Assigns `device` variable with FuchsiaDevice object
        """
        super().setup_class()
        self.device: fuchsia_device.FuchsiaDevice = self.fuchsia_devices[0]

    def setup_test(self) -> None:
        super().setup_test()

    def teardown_test(self) -> None:
        super().teardown_test()

    def test_audio(self) -> None:
        responseAudio = self.device.virtual_audio.capture()
        inputResponse = self.device.virtual_audio.inject(_AUDIO_FILE_INPUT)

        asyncio.run(inputResponse.wait_until_injection_is_done())

        time.sleep(5)

        data = asyncio.run(responseAudio.stop_and_extract_response())
        output_path = os.path.join(
            os.getenv("FUCHSIA_TEST_OUTDIR") or "", "response.wav"
        )
        _LOGGER.info("Got %d bytes", len(data))
        _LOGGER.info("Writing to %s", output_path)

        with open(
            output_path,
            "wb",
        ) as f:
            f.write(data)

    def test_triggered_capture(self) -> None:
        quiet_result = self.device.virtual_audio.wait_for_quiet(
            requested_quiet_period=timedelta(seconds=2),
            optional_maximum_time_to_wait_for_quiet=timedelta(seconds=60),
        )
        assert quiet_result == WaitForQuietResult.SUCCESS

        self.device.virtual_audio.queue_triggered_capture(
            maximum_time_to_capture_audio=timedelta(seconds=5),
            maximum_time_to_wait_for_sound=timedelta(seconds=5),
            optional_quiet_time_before_stopping_capture=timedelta(seconds=1),
        )
        inputResponse = self.device.virtual_audio.inject(_AUDIO_FILE_INPUT)
        asyncio.run(inputResponse.wait_until_injection_is_done())

        capture_result = self.device.virtual_audio.wait_for_triggered_capture()
        data = b""
        if capture_result.audio_data is not None:
            _LOGGER.info("Retrieved audio data from the device")
            data = capture_result.audio_data
        else:
            _LOGGER.warning(
                "No audio data retrieved from the device. This may be OK depending on if this device is expected to play audio."
            )

        output_path = os.path.join(
            os.getenv("FUCHSIA_TEST_OUTDIR") or "", "response.wav"
        )
        _LOGGER.info("Got %d bytes", len(data))
        _LOGGER.info("Writing to %s", output_path)

        with open(
            output_path,
            "wb",
        ) as f:
            f.write(data)


if __name__ == "__main__":
    test_runner.main()

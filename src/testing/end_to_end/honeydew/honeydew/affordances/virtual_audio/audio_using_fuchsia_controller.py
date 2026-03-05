# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Audio recording affordance."""
import json
import logging
import os
import time
from datetime import timedelta

import fidl_fuchsia_test_audio as fta
import fuchsia_async_extension
from fidl import AsyncSocket
from fuchsia_controller_py import Socket

from honeydew import errors
from honeydew.affordances.virtual_audio import audio
from honeydew.affordances.virtual_audio import errors as virtual_audio_errors
from honeydew.affordances.virtual_audio import types
from honeydew.transports.ffx import ffx
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)
from honeydew.typing.custom_types import FidlEndpoint

_LOGGER: logging.Logger = logging.getLogger(__name__)

_INJECTION_ENDPOINT = FidlEndpoint(
    "/core/audio_recording", "fuchsia.test.audio.Injection"
)

_CAPTURE_ENDPOINT = FidlEndpoint(
    "/core/audio_recording", "fuchsia.test.audio.Capture"
)

_AUDIO_RECORDING_COMPONENT: str = "core/audio_recording"


class AsyncVirtualAudioUsingFuchsiaController(audio.AsyncVirtualAudio):
    """Async audio affordance implementation using Fuchsia Controller.

    Args:
        device_name: Device name.
        fuchsia_controller: Fuchsia Controller transport.
        ffx_transport: FFX transport.
    """

    def __init__(
        self,
        device_name: str,
        fuchsia_controller: fc_transport.FuchsiaController,
        ffx_transport: ffx.FFX,
    ) -> None:
        self._device_name: str = device_name
        self.fuchsia_controller: fc_transport.FuchsiaController = (
            fuchsia_controller
        )
        self._ffx_transport: ffx.FFX = ffx_transport

        self.verify_supported()
        self._init_clients()

    def verify_supported(self) -> None:
        """Verify that virtual audio is supported for this device.

        Raises:
            NotSupportedError: Virtual audio is not supported by Fuchsia device.
        """
        # check if the device have component to support virtual audio.
        output = self._ffx_transport.run(
            ["--machine", "json", "component", "list"]
        )
        component_list = json.loads(output)
        instances = component_list.get("instances", [])

        if not any(
            instance.get("moniker") == _AUDIO_RECORDING_COMPONENT
            for instance in instances
        ):
            raise errors.NotSupportedError(
                f"{_AUDIO_RECORDING_COMPONENT} is not available in device {self._device_name}"
            )

    def _init_clients(self) -> None:
        self._injection_client = fta.InjectionClient(
            self.fuchsia_controller.connect_device_proxy(_INJECTION_ENDPOINT)
        )
        self._capture_client = fta.CaptureClient(
            self.fuchsia_controller.connect_device_proxy(_CAPTURE_ENDPOINT)
        )

    async def inject(self, wav_file: str) -> types.AsyncAudioInputWaiter:
        """Inject wav_file audio query.
        Args:
            wav_file: Audio .wav file

        Return:
            AsyncAudioInputWaiter: object. This object is used to wait until the input injection is done.

        Raises:
            VirtualAudioError: On failure
            ValueError : Failed when input audio file is missing.
        """

        if not wav_file or not os.path.exists(wav_file):
            raise ValueError(f"Input audio file cannot be found - {wav_file}.")

        audio_to_inject: bytes
        with open(wav_file, "rb") as f:
            audio_to_inject = f.read()

        val = await self._injection_client.clear_input_audio(index=0)
        if val.err is not None:
            raise virtual_audio_errors.VirtualAudioError(
                f"Failed to clear audio: {val.err}"
            )

        (sender, server_end) = Socket.create()
        self._injection_client.write_input_audio(
            index=0,
            audio_writer=server_end.take(),
        )

        start = time.monotonic()
        _LOGGER.info("Writing audio data...")
        sender.write(audio_to_inject)
        sender.close()
        _LOGGER.info(
            "...Done sending %d bytes in %fs",
            len(audio_to_inject),
            time.monotonic() - start,
        )

        size = await self._injection_client.get_input_audio_size(index=0)
        if size.err is not None or size.response is None:
            raise virtual_audio_errors.VirtualAudioError(
                f"Failed to get audio size: {size.err}"
            )

        if size.response.byte_count != len(audio_to_inject):
            raise virtual_audio_errors.VirtualAudioError(
                f"Expected to have written {size.response.byte_count} bytes, found {len(audio_to_inject)} at audio injection index 0"
            )

        _LOGGER.info("Saying it!")
        if (
            err := (
                await self._injection_client.start_input_injection(index=0)
            ).err
        ) is not None:
            raise virtual_audio_errors.VirtualAudioError(
                f"Failed to start audio {err}"
            )

        return types.AsyncAudioInputWaiter(self._injection_client)

    async def capture(self) -> types.AsyncAudioResponse:
        """Start to capture the audio response.

        Return:
            AsyncAudioResponse: object. This object is used to stop and extract the captured audio.

        Raises:
            VirtualAudioError:  On failure
        """

        _LOGGER.info("Start audio capture...")
        if (
            err := (await self._capture_client.start_output_capture()).err
        ) is not None:
            raise virtual_audio_errors.VirtualAudioError(
                f"Failed to start output capture audio {err}"
            )

        return types.AsyncAudioResponse(self._capture_client)

    async def wait_for_quiet(
        self,
        requested_quiet_period: timedelta,
        optional_maximum_time_to_wait_for_quiet: timedelta | None = None,
    ) -> types.WaitForQuietResult:
        requested_quiet_period_ms = int(
            requested_quiet_period.total_seconds() * 1000
        )
        if optional_maximum_time_to_wait_for_quiet is not None:
            maximum_wait_time_ms = int(
                optional_maximum_time_to_wait_for_quiet.total_seconds() * 1000
            )
        else:
            maximum_wait_time_ms = None
        _LOGGER.info(
            "Waiting for %dms ms of quiet before proceeding",
            requested_quiet_period_ms,
        )
        res = await self._capture_client.wait_for_quiet(
            requested_quiet_period_ms=requested_quiet_period_ms,
            maximum_wait_time_ms=maximum_wait_time_ms,
        )
        if res.err is not None:
            raise virtual_audio_errors.VirtualAudioError(
                f"Failed to wait for quiet: {res.err}"
            )
        if res.response is None:
            raise virtual_audio_errors.VirtualAudioError(
                "Missing response value"
            )

        if res.response.result == fta.WaitForQuietResult.SUCCESS:
            _LOGGER.info("Quiet observed, proceeding.")
            return types.WaitForQuietResult.SUCCESS
        elif (
            res.response.result
            == fta.WaitForQuietResult.QUIET_PERIOD_NOT_OBSERVED
        ):
            _LOGGER.info("Quiet period was not observed.")
            return types.WaitForQuietResult.DID_NOT_OBSERVE_QUIET_PERIOD
        else:
            raise virtual_audio_errors.VirtualAudioError(
                f"Unknown response value: {res.response}. Possible version mismatch between host and target."
            )

    async def queue_triggered_capture(
        self,
        maximum_time_to_wait_for_sound: timedelta,
        maximum_time_to_capture_audio: timedelta,
        optional_quiet_time_before_stopping_capture: timedelta | None,
    ) -> None:
        maximum_time_to_wait_for_sound_ms = int(
            maximum_time_to_wait_for_sound.total_seconds() * 1000
        )
        maximum_capture_duration_ms = int(
            maximum_time_to_capture_audio.total_seconds() * 1000
        )
        if optional_quiet_time_before_stopping_capture is not None:
            optional_quiet_before_stopping_ms = int(
                optional_quiet_time_before_stopping_capture.total_seconds()
                * 1000
            )
        else:
            optional_quiet_before_stopping_ms = None

        capture_res = await self._capture_client.queue_triggered_capture(
            maximum_time_to_wait_for_sound_ms=maximum_time_to_wait_for_sound_ms,
            maximum_capture_duration_ms=maximum_capture_duration_ms,
            optional_quiet_before_stopping_ms=optional_quiet_before_stopping_ms,
        )

        if capture_res.err is not None:
            raise virtual_audio_errors.VirtualAudioError(
                f"Failed to queue triggered capture: {capture_res.err}"
            )

    async def wait_for_triggered_capture(self) -> types.TriggeredCaptureResult:
        capture_res = await self._capture_client.wait_for_triggered_capture()

        if capture_res.err is not None:
            raise virtual_audio_errors.VirtualAudioError(
                f"Failed to get the result of a triggered capture: {capture_res.err}"
            )

        if capture_res.response is None or capture_res.response.result is None:
            raise virtual_audio_errors.VirtualAudioError(
                "Missing response value"
            )

        if (
            capture_res.response.result
            == fta.QueuedCaptureResult.FAILED_NO_SOUND_TIMEOUT
        ):
            return types.TriggeredCaptureResult(
                status=types.TriggeredCaptureStatus.FAILED_TO_START_RECORDING,
                audio_data=None,
            )

        status: types.TriggeredCaptureStatus
        if capture_res.response.result == fta.QueuedCaptureResult.CAPTURED:
            _LOGGER.info("Successfully captured audio")
            status = types.TriggeredCaptureStatus.SUCCESS
        elif (
            capture_res.response.result
            == fta.QueuedCaptureResult.CAPTURED_TO_TIME_LIMIT
        ):
            _LOGGER.info("Successfully captured audio (to duration limit)")
            status = types.TriggeredCaptureStatus.SUCCESS_RECORDED_TO_LIMIT
        else:
            raise virtual_audio_errors.VirtualAudioError(
                f"Unexpected response value: {capture_res.response}. Possible version mismatch between host and target."
            )

        receiver = await self._capture_client.get_output_audio()
        if receiver.err is not None or receiver.response is None:
            raise virtual_audio_errors.VirtualAudioError(
                f"Failed to get captured audio from device: {receiver.err}"
            )

        _LOGGER.info("Reading the stored audio data")
        sock = AsyncSocket(Socket(receiver.response.audio_reader))
        data = await sock.read_all()
        _LOGGER.info("Audio recording contains %d bytes", len(data))

        return types.TriggeredCaptureResult(
            status=status, audio_data=bytes(data)
        )


class VirtualAudioUsingFuchsiaController(audio.VirtualAudio):
    """Audio affordance implementation using Fuchsia Controller.

    Connecting to the protocols this connects to on startup will
    inject the virtual audio device which does the following things:

    - Input audio will only come from the virtual device. Actual microphones are disabled.
    - Output audio will only go to the virtual device. Actual speakers are disabled.

    TODO(https://fxbug.dev/417759272): There is currently no way to disable this
    behavior other than rebooting the device.

    Args:
        device_name: Device name.
        fuchsia_controller: Fuchsia Controller transport.
        ffx_transport: FFX transport.
    """

    def __init__(
        self,
        device_name: str,
        fuchsia_controller: fc_transport.FuchsiaController,
        ffx_transport: ffx.FFX,
    ) -> None:
        self._inner = AsyncVirtualAudioUsingFuchsiaController(
            device_name=device_name,
            fuchsia_controller=fuchsia_controller,
            ffx_transport=ffx_transport,
        )

    def verify_supported(self) -> None:
        """Verify that virtual audio is supported for this device.

        Raises:
            NotSupportedError: Virtual audio is not supported by Fuchsia device.
        """
        self._inner.verify_supported()

    def inject(self, wav_file: str) -> types.AudioInputWaiter:
        """Inject wav_file audio query.
        Args:
            wav_file: Audio .wav file

        Return:
            AudioInputWaiter: object. This object is used to wait until the input injection is done.

        Raises:
            VirtualAudioError: On failure
            ValueError : Failed when input audio file is missing.
        """
        return types.AudioInputWaiter(
            fuchsia_async_extension.get_loop()
            .run_until_complete(self._inner.inject(wav_file))
            ._injection_client
        )

    def capture(self) -> types.AudioResponse:
        """Start to capture the audio response.

        Return:
            AudioResponse: object. This object is used to stop and extract the captured audio.

        Raises:
            VirtualAudioError:  On failure
        """
        return types.AudioResponse(
            fuchsia_async_extension.get_loop()
            .run_until_complete(self._inner.capture())
            ._capture_client
        )

    def wait_for_quiet(
        self,
        requested_quiet_period: timedelta,
        optional_maximum_time_to_wait_for_quiet: timedelta | None = None,
    ) -> types.WaitForQuietResult:
        """Wait until there is no audio playing on the output audio device.

        Args:
            requested_quiet_period (timedelta): Return only after the
                output is quiet for this duration.
            maximum_time_to_wait_for_quiet (optional timedelta): Maximum
                duration to wait.

        Returns:
            WaitForQuietResult: object. Enumeration that reports the
                success of the operation.

        Raises:
            VirtualAudioError: On internal failure. See component logs
                for details.
        """
        return fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.wait_for_quiet(
                requested_quiet_period, optional_maximum_time_to_wait_for_quiet
            )
        )

    def queue_triggered_capture(
        self,
        maximum_time_to_wait_for_sound: timedelta,
        maximum_time_to_capture_audio: timedelta,
        optional_quiet_time_before_stopping_capture: timedelta | None,
    ) -> None:
        """Queue a triggered audio capture that starts when audio is detected
        and ends after a configurable period.

        This function returns when the capture is queued. Use
        `wait_for_triggered_capture` to wait for the result of this
        capture.

        Args:
            maximum_time_to_wait_for_sound (timedelta): Limit on how
                long to wait before recording should start.
            maximum_time_to_capture_audio (timedelta): Maximum duration
                of the recording.
            optional_quiet_time_before_stopping_capture (timedelta):
                If set, stop the capture early if this duration of quiet
                time is observed after recording started. If not set,
                continue capturing until `maximum_time_to_capture_audio`
                duration of audio is recorded.

        Raises:
            VirtualAudioError: On internal failure. See component logs
                for details.
        """
        fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.queue_triggered_capture(
                maximum_time_to_wait_for_sound,
                maximum_time_to_capture_audio,
                optional_quiet_time_before_stopping_capture,
            )
        )

    def wait_for_triggered_capture(self) -> types.TriggeredCaptureResult:
        """Wait for a previously queued triggered capture to complete.

        Returns:
            TriggeredCaptureResult: object. The result of the capture
                operation, containing information on whether recording was
                triggered and, if so, the recorded data.

        Raises:
            VirtualAudioError: On internal failure. See component logs
                for details.
        """
        return fuchsia_async_extension.get_loop().run_until_complete(
            self._inner.wait_for_triggered_capture()
        )

    def as_async(self) -> AsyncVirtualAudioUsingFuchsiaController:
        """Returns the async version of VirtualAudio."""
        return self._inner

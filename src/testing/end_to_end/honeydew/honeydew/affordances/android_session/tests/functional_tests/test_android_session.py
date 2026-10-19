# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for AndroidSession affordance."""

import fuchsia_base_test
from honeydew.affordances.android_session import android_session, errors, types
from honeydew.device_classes import android_device
from mobly import asserts, test_runner


class AndroidSessionAffordanceTests(fuchsia_base_test.FuchsiaBaseTest):
    """AndroidSession affordance tests."""

    dut: android_device.AndroidDevice

    async def setup_class(self) -> None:
        """setup_class is called once before running tests."""
        await super().setup_class()
        asserts.assert_is_instance(self.dut, android_device.AndroidDevice)
        self.session: android_session.AndroidSession = self.dut.android_session
        asserts.assert_is_instance(self.session, android_session.AndroidSession)

    def test_fetch_current(self) -> None:
        """Test case for AndroidSession.fetch_current()."""
        session_id: types.SessionId = self.session.fetch_current()
        pid_output: str = self.dut.adb.run(["shell", "pidof", "system_server"])
        asserts.assert_equal(
            session_id.system_server_pid, int(pid_output.split()[0])
        )
        asserts.assert_equal(
            self.session.fetch_current(wait=False),
            session_id,
        )

    def test_record_session_and_verify_same_session(self) -> None:
        """Test case for record_session() and verify_same_session()."""
        self.session.record_session()
        # Android has not restarted, so this must not raise.
        self.session.verify_same_session()

    def test_session_will_change_invokes_callbacks(self) -> None:
        """Test case for register_session_change_callback(),
        unregister_session_change_callback() and session_will_change()."""
        calls: list[str] = []

        def _callback() -> None:
            calls.append("called")

        # Registering the same callback twice must only invoke it once.
        self.session.register_session_change_callback(_callback)
        self.session.register_session_change_callback(_callback)
        try:
            self.session.record_session()
            self.session.session_will_change()
            asserts.assert_equal(calls, ["called"])
            # The recorded session was cleared, so this is a no-op.
            self.session.verify_same_session()
        finally:
            self.session.unregister_session_change_callback(_callback)

        self.session.session_will_change()
        asserts.assert_equal(calls, ["called"])

    async def test_verify_same_session_detects_android_restart(self) -> None:
        """Test case for verify_same_session() after an Android restart.

        Destructive: reboots the device from Android (`adb reboot`), bypassing
        AndroidDevice.reboot() which would announce the session change.
        """
        before: types.SessionId = self.session.record_session()

        # Reboot via `adb reboot` instead of `self.dut.reboot()` because
        # `AndroidDevice.reboot()` calls `session_will_change()`, which clears
        # the recorded session and makes `verify_same_session()` a no-op.
        self.dut.adb.run(["reboot"])
        self.dut.wait_for_offline()
        await self.dut.wait_for_online()
        await self.dut.on_device_boot()

        after: types.SessionId = self.session.fetch_current()
        asserts.assert_not_equal(before, after)
        with asserts.assert_raises(errors.AndroidSessionError):
            self.session.verify_same_session()

        # Leave the affordance in a clean state for subsequent tests.
        self.session.session_will_change()
        self.session.record_session()


if __name__ == "__main__":
    test_runner.main()

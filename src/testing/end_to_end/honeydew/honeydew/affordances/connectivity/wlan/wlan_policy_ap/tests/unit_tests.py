# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio
import types
import unittest
from typing import Any, TypeVar
from unittest import mock

import fidl_fuchsia_wlan_common as f_wlan_common
import fidl_fuchsia_wlan_device_service as f_wlan_device_service
import fidl_fuchsia_wlan_policy as f_wlan_policy
import fuchsia_controller_py as fc
from fuchsia_controller_py import Channel, Context, FcTransportStatus
from honeydew import affordances_capable
from honeydew.affordances.connectivity.wlan import wlan_policy_ap
from honeydew.affordances.connectivity.wlan.utils.errors import (
    HoneydewWlanError,
)
from honeydew.errors import NotSupportedError
from honeydew.transports.ffx import ffx as ffx_transport
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)

_TEST_SSID = "ThepromisedLAN"

_ACCESS_POINT_STATE = f_wlan_policy.AccessPointState(
    state=f_wlan_policy.OperatingState.STARTING,
    mode=f_wlan_policy.ConnectivityMode.LOCAL_ONLY,
    band=f_wlan_policy.OperatingBand.ONLY_2_4_GHZ,
    frequency=None,
    clients=None,
    id_=f_wlan_policy.NetworkIdentifier(
        ssid=list(_TEST_SSID.encode("utf-8")),
        type_=f_wlan_policy.SecurityType.WPA2,
    ),
)


_T = TypeVar("_T")


async def _async_response(response: _T) -> _T:
    return response


async def _async_error(err: Exception) -> None:
    raise err


# pylint: disable=protected-access
class WlanPolicyApTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        await super().asyncSetUp()

        self.reboot_affordance_obj = mock.MagicMock(
            spec=affordances_capable.RebootCapableDevice,
            autospec=True,
        )
        self.suspend_resume_affordance_obj = mock.MagicMock(
            spec=affordances_capable.SuspendResumeCapableDevice,
            autospec=True,
        )
        self.fuchsia_device_close_obj = mock.MagicMock(
            spec=affordances_capable.FuchsiaDeviceClose,
            autospec=True,
        )
        self.fc_transport_obj = mock.MagicMock(
            spec=fc_transport.FuchsiaController,
            autospec=True,
        )
        self.fc_transport_obj.ctx = Context()

        def channel_create(
            self: fc_transport.FuchsiaController,
        ) -> tuple[fc.Channel, fc.Channel]:
            return self.ctx.channel_create()

        self.fc_transport_obj.channel_create = types.MethodType(
            channel_create, self.fc_transport_obj
        )
        self.ffx_transport_obj = mock.MagicMock(
            spec=ffx_transport.FFX,
            autospec=True,
        )

        self.ffx_transport_obj.run.return_value = "".join(
            wlan_policy_ap._REQUIRED_CAPABILITIES
        )

        self.access_point_state_updates_proxy: (
            f_wlan_policy.AccessPointStateUpdatesClient | None
        ) = None

        self.ap_provider_proxy = mock.MagicMock(
            spec=f_wlan_policy.AccessPointProviderClient,
            autospec=True,
        )

        # pylint: disable-next=unused-argument
        def get_controller(requests: Channel, updates: Channel) -> None:
            self.access_point_state_updates_proxy = (
                f_wlan_policy.AccessPointStateUpdatesClient(updates)
            )

        self.ap_provider_proxy.get_controller = mock.Mock(wraps=get_controller)

        self.ap_listener_proxy = mock.MagicMock(
            spec=f_wlan_policy.AccessPointListenerClient,
            autospec=True,
        )

        def get_listener(updates: Channel) -> None:
            self.access_point_state_updates_proxy = (
                f_wlan_policy.AccessPointStateUpdatesClient(updates)
            )

        self.ap_listener_proxy.get_listener = mock.Mock(wraps=get_listener)

        self.access_point_controller_obj = mock.MagicMock(
            spec=f_wlan_policy.AccessPointControllerClient,
            autospec=True,
        )

        self.device_monitor_proxy = mock.MagicMock(
            spec=f_wlan_device_service.DeviceMonitorClient,
            autospec=True,
        )
        self.device_monitor_proxy.list_phys.side_effect = (
            lambda *args, **kwargs: _async_response(
                f_wlan_device_service.DeviceMonitorListPhysResponse(
                    phy_list=[1]
                )
            )
        )

        mock_get_supported_mac_roles_response = mock.Mock()
        mock_get_supported_mac_roles_response.unwrap.return_value.supported_mac_roles = [
            f_wlan_common.WlanMacRole.AP
        ]

        self.device_monitor_proxy.get_supported_mac_roles.side_effect = (
            lambda *args, **kwargs: _async_response(
                mock_get_supported_mac_roles_response
            )
        )

        for target, return_value in [
            (
                "fidl_fuchsia_wlan_policy.AccessPointProviderClient",
                self.ap_provider_proxy,
            ),
            (
                "fidl_fuchsia_wlan_policy.AccessPointListenerClient",
                self.ap_listener_proxy,
            ),
            (
                "fidl_fuchsia_wlan_policy.AccessPointControllerClient",
                self.access_point_controller_obj,
            ),
            (
                "fidl_fuchsia_wlan_device_service.DeviceMonitorClient",
                self.device_monitor_proxy,
            ),
        ]:
            patcher = mock.patch(target, return_value=return_value)
            patcher.start()
            self.addCleanup(patcher.stop)

        self.wlan_policy_ap_obj = wlan_policy_ap.WlanPolicyAp(
            device_name="fuchsia-emulator",
            ffx=self.ffx_transport_obj,
            fuchsia_controller=self.fc_transport_obj,
            reboot_affordance=self.reboot_affordance_obj,
            fuchsia_device_close=self.fuchsia_device_close_obj,
            suspend_resume_affordance=self.suspend_resume_affordance_obj,
        )

        # Call make_ready() to ensure the affordance is initialized. This is
        # necessary because some tests push updates through the update server
        # task which is created during initialization. Normally make_ready()
        # is called lazily via the @ensure_ready decorator.
        await self.wlan_policy_ap_obj.make_ready()

        assert self.access_point_state_updates_proxy is not None

    async def test_verify_supported(self) -> None:
        """Verify verify_supported fails."""
        self.ffx_transport_obj.run.return_value = ""

        with self.assertRaises(NotSupportedError):
            wlan_policy_ap.WlanPolicyAp(
                device_name="fuchsia-emulator",
                ffx=self.ffx_transport_obj,
                fuchsia_controller=self.fc_transport_obj,
                reboot_affordance=self.reboot_affordance_obj,
                fuchsia_device_close=self.fuchsia_device_close_obj,
                suspend_resume_affordance=self.suspend_resume_affordance_obj,
            )

    async def test_init_register_for_on_device_boot(self) -> None:
        """Verify WlanPolicyAp registers on_device_boot."""
        self.reboot_affordance_obj.register_for_on_device_boot.assert_called_once()

    async def test_init_register_suspend_resume(self) -> None:
        """Verify WlanPolicyAp registers suspend and resume callbacks."""
        self.suspend_resume_affordance_obj.register_on_device_suspend_fn.assert_called_once_with(
            self.wlan_policy_ap_obj._close
        )
        self.suspend_resume_affordance_obj.register_on_device_resume_fn.assert_called_once_with(
            self.wlan_policy_ap_obj.make_ready
        )

    async def test_init_connect_proxy(self) -> None:
        """Verify WlanPolicyAp connects to
        fuchsia.wlan.policy/AccessPointProvider."""
        self.assertIsNotNone(self.wlan_policy_ap_obj._access_point_controller)

    async def test_start(self) -> None:
        """Verify WlanPolicyAp.start()."""
        self.access_point_controller_obj.start_access_point.side_effect = [
            _async_response(
                f_wlan_policy.AccessPointControllerStartAccessPointResponse(
                    status=f_wlan_policy.RequestStatus.ACKNOWLEDGED
                )
            )
        ]

        await self.wlan_policy_ap_obj.start(
            _TEST_SSID,
            f_wlan_policy.SecurityType.NONE,
            None,
            f_wlan_policy.ConnectivityMode.LOCAL_ONLY,
            f_wlan_policy.OperatingBand.ANY,
        )

    async def test_start_fails(self) -> None:
        """Verify WlanPolicyAp.start() throws HoneydewWlanError on internal error."""
        self.access_point_controller_obj.start_access_point.side_effect = [
            _async_error(FcTransportStatus(FcTransportStatus.FC_ERR_INTERNAL))
        ]

        with self.assertRaises(HoneydewWlanError):
            await self.wlan_policy_ap_obj.start(
                _TEST_SSID,
                f_wlan_policy.SecurityType.NONE,
                None,
                f_wlan_policy.ConnectivityMode.LOCAL_ONLY,
                f_wlan_policy.OperatingBand.ANY,
            )

    async def test_stop(self) -> None:
        """Verify WlanPolicyAp.stop()."""
        self.access_point_controller_obj.stop_access_point.side_effect = [
            _async_response(
                f_wlan_policy.AccessPointControllerStartAccessPointResponse(
                    status=f_wlan_policy.RequestStatus.ACKNOWLEDGED
                )
            )
        ]

        await self.wlan_policy_ap_obj.stop(
            _TEST_SSID,
            f_wlan_policy.SecurityType.NONE,
            None,
        )

    async def test_stop_fails(self) -> None:
        """Verify WlanPolicyAp.stop() throws HoneydewWlanError on internal error."""
        self.access_point_controller_obj.stop_access_point.side_effect = [
            _async_error(FcTransportStatus(FcTransportStatus.FC_ERR_INTERNAL))
        ]

        with self.assertRaises(HoneydewWlanError):
            await self.wlan_policy_ap_obj.stop(
                _TEST_SSID,
                f_wlan_policy.SecurityType.NONE,
                None,
            )

    async def test_stop_all(self) -> None:
        """Verify WlanPolicyAp.stop_all()."""
        await self.wlan_policy_ap_obj.stop_all()

    async def test_set_new_update_listener_overrides(self) -> None:
        """Verify WlanPolicyAp.set_new_update_listener() overrides the existing
        access point state updates server."""
        assert self.wlan_policy_ap_obj._access_point_controller is not None
        old_server = (
            self.wlan_policy_ap_obj._access_point_controller.access_point_state_updates_server_task
        )

        await self.wlan_policy_ap_obj.set_new_update_listener()

        new_server = (
            self.wlan_policy_ap_obj._access_point_controller.access_point_state_updates_server_task
        )

        self.assertNotEqual(new_server, old_server)
        self.assertTrue(old_server.cancelled())
        self.assertFalse(new_server.cancelled())
        self.assertEqual(
            self.wlan_policy_ap_obj._access_point_controller.updates.qsize(),
            0,
        )

    async def test_get_update(self) -> None:
        """Verify WlanPolicyAp.get_update()."""
        self.assertIsNotNone(self.access_point_state_updates_proxy)
        assert self.access_point_state_updates_proxy is not None

        (
            await self.access_point_state_updates_proxy.on_access_point_state_update(
                access_points=[
                    _ACCESS_POINT_STATE,
                ]
            )
        )
        self.assertEqual(
            await self.wlan_policy_ap_obj.get_update(), [_ACCESS_POINT_STATE]
        )

    async def test_get_update_queuing(self) -> None:
        """Verify WlanPolicyAp.get_update() queues updates."""
        self.assertIsNotNone(self.access_point_state_updates_proxy)
        assert self.access_point_state_updates_proxy is not None

        (
            await self.access_point_state_updates_proxy.on_access_point_state_update(
                access_points=[]
            )
        )
        (
            await self.access_point_state_updates_proxy.on_access_point_state_update(
                access_points=[_ACCESS_POINT_STATE]
            )
        )
        self.assertEqual(await self.wlan_policy_ap_obj.get_update(), [])
        self.assertEqual(
            await self.wlan_policy_ap_obj.get_update(), [_ACCESS_POINT_STATE]
        )

    @mock.patch(
        "asyncio.wait_for", autospec=True, side_effect=[asyncio.TimeoutError]
    )
    async def test_get_update_timeout(
        self, wait_for_mock: mock.MagicMock
    ) -> None:
        """Verify WlanPolicyAp.get_update() throws TimeoutError on timeout."""
        with self.assertRaises(TimeoutError):
            await self.wlan_policy_ap_obj.get_update(timeout=10)
        wait_for_mock.assert_called_once()

    async def test_make_ready_unsupported_ap_resets_ready(self) -> None:
        """Verify that if make_ready fails due to unsupported AP role, _ready remains False."""
        mock_roles_resp = mock.Mock()
        mock_roles_resp.unwrap.return_value.supported_mac_roles = [
            f_wlan_common.WlanMacRole.CLIENT
        ]
        self.device_monitor_proxy.list_phys.side_effect = (
            lambda *args, **kwargs: _async_response(
                f_wlan_device_service.DeviceMonitorListPhysResponse(
                    phy_list=[1]
                )
            )
        )
        self.device_monitor_proxy.get_supported_mac_roles.side_effect = (
            lambda *args, **kwargs: _async_response(mock_roles_resp)
        )

        affordance = wlan_policy_ap.WlanPolicyAp(
            device_name="fuchsia-emulator",
            ffx=self.ffx_transport_obj,
            fuchsia_controller=self.fc_transport_obj,
            reboot_affordance=self.reboot_affordance_obj,
            fuchsia_device_close=self.fuchsia_device_close_obj,
            suspend_resume_affordance=self.suspend_resume_affordance_obj,
        )
        self.assertFalse(affordance._ready)

        # First call fails in make_ready
        with self.assertRaises(HoneydewWlanError) as ctx:
            await affordance.start(
                _TEST_SSID,
                f_wlan_policy.SecurityType.NONE,
                None,
                f_wlan_policy.ConnectivityMode.LOCAL_ONLY,
                f_wlan_policy.OperatingBand.ANY,
            )
        self.assertIn(
            "Device does not support an access point interface",
            str(ctx.exception),
        )
        self.assertFalse(affordance._ready)

        # Subsequent call must re-attempt make_ready and raise HoneydewWlanError, NOT AssertionError
        with self.assertRaises(HoneydewWlanError) as ctx:
            await affordance.start(
                _TEST_SSID,
                f_wlan_policy.SecurityType.NONE,
                None,
                f_wlan_policy.ConnectivityMode.LOCAL_ONLY,
                f_wlan_policy.OperatingBand.ANY,
            )
        self.assertIn(
            "Device does not support an access point interface",
            str(ctx.exception),
        )
        self.assertFalse(affordance._ready)

    async def test_start_uninitialized_controller_raises_assertion_error(
        self,
    ) -> None:
        """Verify calling start with uninitialized controller raises AssertionError."""
        affordance = wlan_policy_ap.WlanPolicyAp(
            device_name="fuchsia-emulator",
            ffx=self.ffx_transport_obj,
            fuchsia_controller=self.fc_transport_obj,
            reboot_affordance=self.reboot_affordance_obj,
            fuchsia_device_close=self.fuchsia_device_close_obj,
            suspend_resume_affordance=self.suspend_resume_affordance_obj,
        )
        # Simulate state where _ready is True but controller is None
        affordance._ready = True
        affordance._access_point_controller = None

        with self.assertRaises(AssertionError):
            await affordance.start(
                _TEST_SSID,
                f_wlan_policy.SecurityType.NONE,
                None,
                f_wlan_policy.ConnectivityMode.LOCAL_ONLY,
                f_wlan_policy.OperatingBand.ANY,
            )

    async def test_make_ready_direct_call_failure_resets_ready(self) -> None:
        """Verify calling make_ready directly resets _ready on failure."""
        mock_roles_resp = mock.Mock()
        mock_roles_resp.unwrap.return_value.supported_mac_roles = [
            f_wlan_common.WlanMacRole.CLIENT
        ]
        self.device_monitor_proxy.list_phys.side_effect = (
            lambda *args, **kwargs: _async_response(
                f_wlan_device_service.DeviceMonitorListPhysResponse(
                    phy_list=[1]
                )
            )
        )
        self.device_monitor_proxy.get_supported_mac_roles.side_effect = (
            lambda *args, **kwargs: _async_response(mock_roles_resp)
        )

        affordance = wlan_policy_ap.WlanPolicyAp(
            device_name="fuchsia-emulator",
            ffx=self.ffx_transport_obj,
            fuchsia_controller=self.fc_transport_obj,
            reboot_affordance=self.reboot_affordance_obj,
            fuchsia_device_close=self.fuchsia_device_close_obj,
            suspend_resume_affordance=self.suspend_resume_affordance_obj,
        )
        self.assertFalse(affordance._ready)

        # Call make_ready directly (like the resume callback does)
        with self.assertRaises(HoneydewWlanError):
            await affordance.make_ready()
        self.assertFalse(affordance._ready)

    async def test_close_resets_ready(self) -> None:
        """Verify _close() resets _ready to False and clears controller."""
        self.assertTrue(self.wlan_policy_ap_obj._ready)
        self.assertIsNotNone(self.wlan_policy_ap_obj._access_point_controller)

        await self.wlan_policy_ap_obj._close()

        self.assertFalse(self.wlan_policy_ap_obj._ready)
        self.assertIsNone(self.wlan_policy_ap_obj._access_point_controller)

    async def test_make_ready_cleans_up_existing_controller(self) -> None:
        """Verify make_ready() cancels existing controller task and replaces it."""
        assert self.wlan_policy_ap_obj._access_point_controller is not None
        old_task = (
            self.wlan_policy_ap_obj._access_point_controller.access_point_state_updates_server_task
        )
        self.assertFalse(old_task.cancelled())

        await self.wlan_policy_ap_obj.make_ready()

        self.assertTrue(old_task.cancelled())
        assert self.wlan_policy_ap_obj._access_point_controller is not None
        new_task = (
            self.wlan_policy_ap_obj._access_point_controller.access_point_state_updates_server_task
        )
        self.assertNotEqual(old_task, new_task)
        self.assertFalse(new_task.cancelled())
        self.assertTrue(self.wlan_policy_ap_obj._ready)

    async def test_make_ready_failure_clears_existing_controller(self) -> None:
        """Verify make_ready() cleans up existing controller even if make_ready fails."""
        assert self.wlan_policy_ap_obj._access_point_controller is not None
        old_task = (
            self.wlan_policy_ap_obj._access_point_controller.access_point_state_updates_server_task
        )
        self.assertFalse(old_task.cancelled())

        mock_roles_resp = mock.Mock()
        mock_roles_resp.unwrap.return_value.supported_mac_roles = [
            f_wlan_common.WlanMacRole.CLIENT
        ]
        self.device_monitor_proxy.get_supported_mac_roles.side_effect = (
            lambda *args, **kwargs: _async_response(mock_roles_resp)
        )

        with self.assertRaises(HoneydewWlanError):
            await self.wlan_policy_ap_obj.make_ready()

        self.assertTrue(old_task.cancelled())
        self.assertIsNone(self.wlan_policy_ap_obj._access_point_controller)
        self.assertFalse(self.wlan_policy_ap_obj._ready)

    async def test_set_new_update_listener_cancels_task_on_error(self) -> None:
        """Verify set_new_update_listener() cancels and awaits task if get_listener fails."""
        self.ap_listener_proxy.get_listener.side_effect = FcTransportStatus(
            FcTransportStatus.FC_ERR_INTERNAL
        )
        created_tasks: list[asyncio.Task[None]] = []
        original_create_task = asyncio.create_task

        def create_task_spy(*args: Any, **kwargs: Any) -> asyncio.Task[Any]:
            t = original_create_task(*args, **kwargs)
            created_tasks.append(t)
            return t

        with mock.patch("asyncio.create_task", side_effect=create_task_spy):
            with self.assertRaises(HoneydewWlanError):
                await self.wlan_policy_ap_obj.set_new_update_listener()

        self.assertEqual(len(created_tasks), 1)
        self.assertTrue(created_tasks[0].cancelled())


if __name__ == "__main__":
    unittest.main()

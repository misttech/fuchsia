# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for honeydew.affordances.media.media_using_fc.py."""

import json
import unittest
from unittest import mock

import fidl_fuchsia_media_sessions2 as media_session
import fuchsia_controller_py as fc

from honeydew import affordances_capable, errors
from honeydew.affordances.media import media, media_using_fc
from honeydew.affordances.media.errors import MediaError
from honeydew.transports.ffx import ffx
from honeydew.transports.fuchsia_controller import (
    fuchsia_controller as fc_transport,
)


class MediaFcTests(unittest.TestCase):
    """Unit tests for the media_using_fc.MediaUsingFc class."""

    def setUp(self) -> None:
        super().setUp()
        self.reboot_affordance_obj = mock.MagicMock(
            spec=affordances_capable.RebootCapableDevice
        )
        self.fc_transport_obj = mock.MagicMock(
            spec=fc_transport.FuchsiaController
        )

        self.ffx_transport_obj = mock.MagicMock(spec=ffx.FFX)
        self.ffx_transport_obj.run.return_value = json.dumps(
            {"instances": [{"moniker": "core/mediasession"}]}
        )

        self.media_obj = media_using_fc.MediaUsingFc(
            device_name="fuchsia-emulator",
            fuchsia_controller=self.fc_transport_obj,
            ffx_transport=self.ffx_transport_obj,
            reboot_affordance=self.reboot_affordance_obj,
        )

    def test_verify_supported_success(self) -> None:
        """Test verify_supported success."""
        self.media_obj.verify_supported()
        self.ffx_transport_obj.run.assert_called()

    def test_verify_supported_failure(self) -> None:
        """Test verify_supported failure."""
        self.ffx_transport_obj.run.return_value = json.dumps({"instances": []})
        with self.assertRaises(errors.NotSupportedError):
            self.media_obj.verify_supported()

    @mock.patch.object(media_session, "ActiveSessionClient", autospec=True)
    @mock.patch.object(media_session, "SessionControlClient", autospec=True)
    def test_get_active_session_status_playing(
        self,
        mock_session_control_client: mock.Mock,
        mock_active_session_client: mock.Mock,
    ) -> None:
        """Test get_active_session_status returns PLAYING."""
        mock_active_session_proxy = mock_active_session_client.return_value
        mock_active_session_proxy.watch_active_session = mock.AsyncMock(
            return_value=media_session.ActiveSessionWatchActiveSessionResponse(
                session=mock.MagicMock(spec=fc.Channel)
            )
        )

        mock_session_control_proxy = mock_session_control_client.return_value
        mock_session_control_proxy.watch_status = mock.AsyncMock(
            return_value=media_session.SessionControlWatchStatusResponse(
                session_info_delta=media_session.SessionInfoDelta(
                    player_status=media_session.PlayerStatus(
                        player_state=media_session.PlayerState.PLAYING
                    )
                )
            )
        )

        status = self.media_obj.get_active_session_status()
        self.assertEqual(status, media.PlayerState.PLAYING)

    @mock.patch.object(media_session, "ActiveSessionClient", autospec=True)
    @mock.patch.object(media_session, "SessionControlClient", autospec=True)
    def test_get_active_session_status_no_session(
        self,
        mock_session_control_client: mock.Mock,
        mock_active_session_client: mock.Mock,
    ) -> None:
        """Test get_active_session_status returns None when no session exists."""
        mock_active_session_proxy = mock_active_session_client.return_value
        mock_active_session_proxy.watch_active_session = mock.AsyncMock(
            return_value=media_session.ActiveSessionWatchActiveSessionResponse(
                session=None
            )
        )

        status = self.media_obj.get_active_session_status()
        self.assertIsNone(status)

    @mock.patch.object(media_session, "ActiveSessionClient", autospec=True)
    def test_get_active_session_status_error(
        self, mock_active_session_client: mock.Mock
    ) -> None:
        """Test get_active_session_status raises MediaError on FIDL failure."""
        mock_active_session_proxy = mock_active_session_client.return_value
        mock_active_session_proxy.watch_active_session = mock.AsyncMock(
            side_effect=fc.ZxStatus(fc.ZxStatus.ZX_ERR_PEER_CLOSED)
        )

        with self.assertRaises(MediaError):
            self.media_obj.get_active_session_status()


if __name__ == "__main__":
    unittest.main()

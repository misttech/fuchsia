# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Fakes for media affordance unit tests."""

import fidl_fuchsia_media_sessions2 as media_session
import fuchsia_controller_py as fc


class FakeSessionControlServer(media_session.SessionControlServer):
    """Fake SessionControlServer for testing."""

    def __init__(
        self,
        channel: fc.Channel,
        status_response: media_session.SessionControlWatchStatusResponse
        | None = None,
        exception: Exception | None = None,
    ):
        super().__init__(channel)
        self.status_response: media_session.SessionControlWatchStatusResponse | None = (
            status_response
        )
        self.exception: Exception | None = exception

    async def watch_status(
        self,
    ) -> media_session.SessionControlWatchStatusResponse:
        if self.exception:
            raise self.exception
        if self.status_response:
            return self.status_response
        return media_session.SessionControlWatchStatusResponse(
            session_info_delta=media_session.SessionInfoDelta(
                player_status=media_session.PlayerStatus(
                    player_state=media_session.PlayerState.ERROR
                )
            )
        )


FakeSessionControlServer.__abstractmethods__ = frozenset()


class FakeActiveSessionServer(media_session.ActiveSessionServer):
    """Fake ActiveSessionServer for testing."""

    def __init__(
        self,
        channel: fc.Channel,
        session_channel: fc.Channel | None = None,
        response: media_session.ActiveSessionWatchActiveSessionResponse
        | None = None,
        exception: Exception | None = None,
    ):
        super().__init__(channel)
        self.session_channel: fc.Channel | None = session_channel
        self.response: media_session.ActiveSessionWatchActiveSessionResponse | None = (
            response
        )
        self.exception: Exception | None = exception

    async def watch_active_session(
        self,
    ) -> media_session.ActiveSessionWatchActiveSessionResponse:
        if self.exception:
            raise self.exception
        if self.response:
            return self.response
        if self.session_channel:
            return media_session.ActiveSessionWatchActiveSessionResponse(
                session=self.session_channel.take()
            )
        return media_session.ActiveSessionWatchActiveSessionResponse(
            session=None
        )


FakeActiveSessionServer.__abstractmethods__ = frozenset()

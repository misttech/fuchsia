# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Any

from pydantic import BaseModel, ConfigDict

PROTOCOL_VERSION = 6


class BaseRequest(BaseModel):
    """Base class for all requests, enforcing keyword-only instantiation."""

    model_config = ConfigDict(kw_only=True)

    command: str
    last_seen_seq: int | None = None
    ack_seq: int | None = None


class ThreadInfo(BaseModel):
    """Information about a single thread."""

    id: int
    name: str


class GetStateResponse(BaseModel):
    """Response for get-state command containing thread list, active
    processes, and active breakpoints.
    """

    threads: list[ThreadInfo]
    processes: dict[int, str] | None = None
    breakpoints: dict[str, list[int]] | None = None


from shared.protocol.evaluate import EvaluateResponse


class Response(BaseModel):
    """Standard response wrapper."""

    success: bool
    message: str | None = None
    # TODO(https://fxbug.dev/531840329): Decouple command response models from base.py
    # using dynamic registration in ProtocolRegistry.
    body: GetStateResponse | EvaluateResponse | dict[str, Any] | None = None
    events: list[dict[str, Any]] | None = None


class ProtocolRegistry:
    request_adapter: Any = None


def serialize(obj: BaseModel) -> str:
    return obj.model_dump_json() + "\n"


def make_request(data: dict[str, Any]) -> BaseRequest:
    if ProtocolRegistry.request_adapter is None:
        raise RuntimeError("ProtocolRegistry not initialized")
    return ProtocolRegistry.request_adapter.validate_python(data)


def deserialize_request(line: str) -> BaseRequest:
    if ProtocolRegistry.request_adapter is None:
        raise RuntimeError("ProtocolRegistry not initialized")
    return ProtocolRegistry.request_adapter.validate_json(line.strip())


def get_schema() -> dict[str, Any]:
    if ProtocolRegistry.request_adapter is None:
        raise RuntimeError("ProtocolRegistry not initialized")
    return {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "zxdb-cli Protocol Schema",
        "description": (
            "JSON schema for requests and responses in the "
            "zxdb-cli UDS protocol"
        ),
        "version": PROTOCOL_VERSION,
        "requests": ProtocolRegistry.request_adapter.json_schema(),
        "responses": Response.model_json_schema(),
    }

#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Common functions for working with trace formatting."""

from pathlib import Path
from typing import Any, Dict

_SCRIPT_BASENAME = Path(__file__).name

TraceEvent = dict[str, Any]
CompleteTrace = dict[str, dict[str, str] | list[TraceEvent]]


def event_json(
    name: str, category: str, time: int, value_type: str, value: Any
) -> TraceEvent:
    """Returns JSON for a single trace event's value."""
    return {
        "name": name,
        "cat": category,
        "ph": "C",
        "pid": 1,
        "tid": 1,
        "ts": time,
        "args": {value_type: value},
    }


def complete_trace(
    metadata: Dict[str, str],
    trace_events: list[TraceEvent],
) -> CompleteTrace:
    """Emit a complete trace of events with metadata, JSON object format.

    Args:
      metadata: additional information about the build invocation/environment.
      event_generator: generates the main event trace payload.
    """
    return {"otherData": metadata, "traceEvents": trace_events}


def metadata_arg_to_dict(metadata_arg: str) -> Dict[str, str]:
    d: Dict[str, str] = {}
    if not metadata_arg:
        return d
    pairs = metadata_arg.split(",")
    for p in pairs:
        k, sep, v = p.partition(":")
        if sep != ":":
            raise ValueError(f"Expected a 'key:value', but got '{p}'.")
        d[k] = v
    return d

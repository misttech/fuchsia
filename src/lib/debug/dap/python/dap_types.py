# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from dataclasses import dataclass
from typing import Optional


# TODO(https://fxbug.dev/510003272): Support presentationHint.
@dataclass
class Source:
    """A Source is a descriptor for source code.

    Attributes:
        name: The short name of the source.
        path: The path of the source to be shown in the UI.
        sourceReference: If the value > 0 the contents of the source must be
            retrieved through the `source` request.
    """

    name: Optional[str] = None
    path: Optional[str] = None
    sourceReference: Optional[int] = None


# TODO(https://fxbug.dev/510003272): Support presentationHint.
@dataclass
class StackFrame:
    """A StackFrame.

    Attributes:
        id: An identifier for the stack frame. It must be unique across all threads.
            This id can be used to retrieve the scopes of the frame with the `scopes`
            request or to restart the execution of a stack frame.
        name: The name of the stack frame, typically a method name.
        line: The line within the source of the frame. If the source attribute is missing
            or doesn't exist, `line` is 0 and should be ignored by the client.
        column: Start position of the range covered by the stack frame. It is measured in
            UTF-16 code units and the client capability `columnsStartAt1` determines
            whether it is 0- or 1-based. If attribute `source` is missing or doesn't
            exist, `column` is 0 and should be ignored by the client.
        source: The source of the frame.
    """

    id: int
    name: str
    line: int
    column: int
    source: Optional[Source] = None


@dataclass
class Thread:
    """A Thread.

    Attributes:
        id: Unique identifier for the thread.
        name: The name of the thread.
    """

    id: int
    name: str

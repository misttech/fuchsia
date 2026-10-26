# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Interactive operator consent for unknown register reads.

An access that matches neither an active plan approval nor a persistent
grant is undecided. In unattended operation it fails closed; with a
consent prompt attached, the operator chooses one of four decisions. The
persistent choices store the exact access rule -- never a region, range,
or wildcard.
"""

import datetime
import enum
from typing import Protocol

from driver_lab.models import AccessRequest, Decision, ReadGrant

READ_WARNING = (
    "Reading a hardware register may clear status bits, consume FIFO "
    "data, acknowledge events, or fault when hardware dependencies are "
    "disabled."
)
WRITE_WARNING = (
    "Writing to a hardware register modifies hardware state and can "
    "cause system crashes, bus hangs, or permanent damage. Mutations "
    "require explicit operator confirmation."
)


class ConsentDecision(enum.Enum):
    """The operator's choice for one exact access rule."""

    ALLOW_ONCE = "allow_once"
    ALWAYS_ALLOW = "always_allow"
    DENY_ONCE = "deny_once"
    ALWAYS_DENY = "always_deny"


class ConsentPrompt(Protocol):
    """The interactive consent surface.

    Implementations display the exact access rule and `warning`, and
    return the operator's decision. Absence of a prompt means unattended
    operation, which fails closed.
    """

    async def request_consent(
        self, request: AccessRequest, warning: str
    ) -> ConsentDecision:
        """Asks the operator to decide one exact access rule."""
        ...


def grant_from_decision(
    request: AccessRequest, decision: ConsentDecision
) -> ReadGrant:
    """The exact persistent rule for an always-allow or always-deny
    decision."""
    if decision is ConsentDecision.ALWAYS_ALLOW:
        stored = Decision.ALLOW
    elif decision is ConsentDecision.ALWAYS_DENY:
        stored = Decision.DENY
    else:
        raise ValueError(f"decision {decision} is not persistent")
    return ReadGrant(
        schema_version=1,
        target_scope=request.target_scope,
        node_id=request.node_id,
        resource_digest=request.resource_digest,
        resource=request.resource,
        offset=request.offset,
        width=request.width,
        access=request.access,
        decision=stored,
        approved_at=datetime.datetime.now(datetime.UTC).isoformat(),
        approval_source="interactive",
    )

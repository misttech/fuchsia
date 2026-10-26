# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Persistent read-grant resolution and storage.

Resolution semantics: a persistent deny takes precedence over any allow;
a request matching no persistent rule is undecided, and unattended
operation must fail closed on an undecided result. The store rejects
duplicate or ambiguous entries at load, and persists atomically under a
lock file.
"""

import dataclasses
import enum
import json
import os
import tomllib
from collections.abc import Sequence
from pathlib import Path

from driver_lab.models import AccessClass, AccessRequest, Decision, ReadGrant


class Outcome(enum.Enum):
    """Result of resolving one access request against persistent grants."""

    ALLOWED = "allowed"
    DENIED = "denied"
    UNDECIDED = "undecided"


@dataclasses.dataclass(frozen=True)
class Resolution:
    """The outcome and, when decided, the rule that decided it."""

    outcome: Outcome
    grant: ReadGrant | None

    @property
    def decided(self) -> bool:
        """Whether a persistent rule decided this request."""
        return self.outcome is not Outcome.UNDECIDED


def resolve(request: AccessRequest, grants: Sequence[ReadGrant]) -> Resolution:
    """Resolves `request` against `grants`.

    A matching deny always wins. With no matching rule the result is
    undecided: interactive flows may prompt, unattended flows fail closed.
    """
    matching = [grant for grant in grants if grant.matches(request)]
    for grant in matching:
        if grant.decision is Decision.DENY:
            return Resolution(Outcome.DENIED, grant)
    for grant in matching:
        if grant.decision is Decision.ALLOW:
            return Resolution(Outcome.ALLOWED, grant)
    return Resolution(Outcome.UNDECIDED, None)


class GrantStoreError(Exception):
    """A grant file could not be loaded, validated, or persisted."""


_FIELD_ORDER = (
    "target_scope",
    "node_id",
    "resource_digest",
    "resource",
    "offset",
    "width",
    "access",
    "decision",
    "approved_at",
    "approval_source",
    "reason",
    "max_poll_hz",
    "max_poll_timeout_s",
)


def _grant_from_entry(entry: dict[str, object], index: int) -> ReadGrant:
    if not isinstance(entry, dict):
        raise GrantStoreError(f"read_grants[{index}] is not a table")
    unknown = set(entry) - set(_FIELD_ORDER)
    if unknown:
        raise GrantStoreError(
            f"read_grants[{index}] has unknown fields: {sorted(unknown)}"
        )
    try:
        return ReadGrant(
            schema_version=1,
            target_scope=str(entry["target_scope"]),
            node_id=str(entry["node_id"]),
            resource_digest=str(entry["resource_digest"]),
            resource=str(entry["resource"]),
            offset=int(entry["offset"]),  # type: ignore[call-overload]
            width=int(entry["width"]),  # type: ignore[call-overload]
            access=AccessClass(entry["access"]),
            decision=Decision(entry["decision"]),
            approved_at=str(entry["approved_at"]),
            approval_source=(
                str(entry["approval_source"])
                if "approval_source" in entry
                else None
            ),
            reason=str(entry["reason"]) if "reason" in entry else None,
            max_poll_hz=(
                float(entry["max_poll_hz"])  # type: ignore[arg-type]
                if "max_poll_hz" in entry
                else None
            ),
            max_poll_timeout_s=(
                float(entry["max_poll_timeout_s"])  # type: ignore[arg-type]
                if "max_poll_timeout_s" in entry
                else None
            ),
        )
    except KeyError as error:
        raise GrantStoreError(
            f"read_grants[{index}] is missing field {error}"
        ) from error
    except (TypeError, ValueError) as error:
        raise GrantStoreError(
            f"read_grants[{index}] is invalid: {error}"
        ) from error


def _validate_no_duplicates(grants: Sequence[ReadGrant]) -> None:
    seen: dict[tuple[object, ...], int] = {}
    for index, grant in enumerate(grants):
        if grant.match_key in seen:
            raise GrantStoreError(
                f"read_grants[{index}] duplicates or conflicts with "
                f"read_grants[{seen[grant.match_key]}] (same match identity)"
            )
        seen[grant.match_key] = index


def load_grants(path: Path) -> list[ReadGrant]:
    """Loads and validates a grant file. A missing file is an empty store."""
    if not path.exists():
        return []
    try:
        with path.open("rb") as file:
            data = tomllib.load(file)
    except (tomllib.TOMLDecodeError, OSError) as error:
        raise GrantStoreError(f"cannot parse {path}: {error}") from error
    if data.get("schema_version") != 1:
        raise GrantStoreError(f"{path}: unsupported or missing schema_version")
    unknown = set(data) - {"schema_version", "read_grants"}
    if unknown:
        raise GrantStoreError(
            f"{path}: unknown top-level keys: {sorted(unknown)}"
        )
    entries = data.get("read_grants", [])
    if not isinstance(entries, list):
        raise GrantStoreError(f"{path}: read_grants is not an array of tables")
    grants = [
        _grant_from_entry(entry, index) for index, entry in enumerate(entries)
    ]
    _validate_no_duplicates(grants)
    return grants


def _toml_value(value: object) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return repr(value)
    if isinstance(value, str):
        # JSON string escaping is valid TOML basic-string escaping.
        return json.dumps(value)
    raise GrantStoreError(
        f"cannot serialize value of type {type(value).__name__}"
    )


def _serialize(grants: Sequence[ReadGrant]) -> str:
    lines = ["schema_version = 1", ""]
    for grant in grants:
        lines.append("[[read_grants]]")
        values: dict[str, object | None] = {
            "target_scope": grant.target_scope,
            "node_id": grant.node_id,
            "resource_digest": grant.resource_digest,
            "resource": grant.resource,
            "offset": grant.offset,
            "width": grant.width,
            "access": grant.access.value,
            "decision": grant.decision.value,
            "approved_at": grant.approved_at,
            "approval_source": grant.approval_source,
            "reason": grant.reason,
            "max_poll_hz": grant.max_poll_hz,
            "max_poll_timeout_s": grant.max_poll_timeout_s,
        }
        for field in _FIELD_ORDER:
            value = values[field]
            if value is not None:
                lines.append(f"{field} = {_toml_value(value)}")
        lines.append("")
    return "\n".join(lines)


def save_grants(path: Path, grants: Sequence[ReadGrant]) -> None:
    """Atomically persists `grants`, holding a lock file while writing.

    The content is validated (including duplicate rejection), written to a
    temporary file in the same directory, flushed, and atomically renamed
    over the original.
    """
    _validate_no_duplicates(grants)
    path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = path.with_name(path.name + ".lock")
    try:
        lock_fd = os.open(lock_path, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
    except FileExistsError as error:
        raise GrantStoreError(f"{path} is locked by another writer") from error
    try:
        os.write(lock_fd, str(os.getpid()).encode())
        temp_path = path.with_name(path.name + ".tmp")
        fd = os.open(temp_path, os.O_CREAT | os.O_TRUNC | os.O_WRONLY, 0o600)
        try:
            os.write(fd, _serialize(grants).encode())
            os.fsync(fd)
        finally:
            os.close(fd)
        os.replace(temp_path, path)
    finally:
        os.close(lock_fd)
        os.unlink(lock_path)


def add_grant(path: Path, grant: ReadGrant) -> None:
    """Adds one grant, rejecting a duplicate or conflicting match identity."""
    grants = load_grants(path)
    save_grants(path, [*grants, grant])


def revoke_grant(path: Path, grant_id: str) -> bool:
    """Removes the grant with `grant_id`. Returns whether it existed."""
    grants = load_grants(path)
    remaining = [grant for grant in grants if grant.grant_id != grant_id]
    if len(remaining) == len(grants):
        return False
    save_grants(path, remaining)
    return True

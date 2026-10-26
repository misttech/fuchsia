# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Target ceiling policy manifests, canonicalization, and narrowing verification.

The target ceiling defines the immutable hardware boundaries and safety
constraints for the proxy driver. Runtime configuration (such as structured
configuration) may only narrow this ceiling, never widen it (Spec 9.2, 22).

Manifests are canonicalized and digested via SHA-256 (Spec 9.6).
"""

import dataclasses
import hashlib
import json
from collections.abc import Mapping, Sequence
from typing import Any

SCHEMA_VERSION = 1
MAX_RESOURCES = 64
MAX_SNAPSHOT_ITEMS = 64
MAX_NAME_LENGTH = 64


class PolicyError(Exception):
    """The policy manifest is structurally invalid."""


class NarrowingError(PolicyError):
    """A runtime configuration widened rather than narrowed the baseline policy."""


def _require_int(value: object, what: str, minimum: int = 0) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise PolicyError(f"{what} must be an integer")
    if value < minimum:
        raise PolicyError(f"{what} must be at least {minimum}")
    return value


def _require_bool(value: object, what: str) -> bool:
    if not isinstance(value, bool):
        raise PolicyError(f"{what} must be a boolean")
    return value


def _require_str(
    value: object, what: str, max_length: int = MAX_NAME_LENGTH
) -> str:
    if not isinstance(value, str) or not value or len(value) > max_length:
        raise PolicyError(
            f"{what} must be a non-empty string of at most {max_length} characters"
        )
    return value


def canonicalize_ranges(ranges_in: Sequence[object]) -> list[list[int]]:
    """Validates and merges overlapping/adjacent ranges into canonical [start, end] pairs."""
    if not ranges_in:
        return []

    parsed: list[tuple[int, int]] = []
    for idx, item in enumerate(ranges_in):
        if isinstance(item, (list, tuple)):
            if len(item) != 2:
                raise PolicyError(f"range {idx} must be a pair of [start, end]")
            start = _require_int(item[0], f"range {idx} start")
            end = _require_int(item[1], f"range {idx} end")
        elif isinstance(item, Mapping):
            start = _require_int(item.get("start"), f"range {idx} start")
            end = _require_int(item.get("end"), f"range {idx} end")
        else:
            raise PolicyError(f"range {idx} has invalid type: {type(item)}")

        if start >= end:
            raise PolicyError(
                f"range {idx} invalid: start ({start}) must be strictly less than end ({end})"
            )
        parsed.append((start, end))

    parsed.sort()
    merged: list[list[int]] = []
    for start, end in parsed:
        if merged and start <= merged[-1][1]:
            # Overlapping or adjacent
            merged[-1][1] = max(merged[-1][1], end)
        else:
            merged.append([start, end])
    return merged


@dataclasses.dataclass(frozen=True)
class WritableRegister:
    """Policy specification for one writable register."""

    offset: int
    allow_mask: int
    width: int = 4
    allow_rmw: bool = False
    require_precondition: bool = False
    precondition_mask: int = 0
    readback: bool = False

    def __post_init__(self) -> None:
        _require_int(self.offset, "writable_register.offset")
        _require_int(self.allow_mask, "writable_register.allow_mask")
        _require_int(self.width, "writable_register.width")
        if self.width != 4:
            raise PolicyError(
                f"unsupported writable register width: {self.width}"
            )
        if self.allow_mask == 0:
            raise PolicyError("writable_register.allow_mask must be non-zero")
        _require_bool(self.allow_rmw, "writable_register.allow_rmw")
        _require_bool(
            self.require_precondition, "writable_register.require_precondition"
        )
        _require_int(
            self.precondition_mask, "writable_register.precondition_mask"
        )
        _require_bool(self.readback, "writable_register.readback")

    def canonical_dict(self) -> dict[str, Any]:
        return {
            "allow_mask": self.allow_mask,
            "allow_rmw": self.allow_rmw,
            "offset": self.offset,
            "precondition_mask": self.precondition_mask,
            "readback": self.readback,
            "require_precondition": self.require_precondition,
            "width": self.width,
        }


@dataclasses.dataclass(frozen=True)
class ResourcePolicyManifest:
    """Policy ceiling for one resource."""

    id: int
    name: str
    allow_unknown_reads: bool = True
    allow_poll: bool = False
    hard_denied: list[list[int]] = dataclasses.field(default_factory=list)
    writable_registers: list[WritableRegister] = dataclasses.field(
        default_factory=list
    )

    def __post_init__(self) -> None:
        _require_int(self.id, "resource.id")
        _require_str(self.name, "resource.name")
        _require_bool(self.allow_unknown_reads, "resource.allow_unknown_reads")
        _require_bool(self.allow_poll, "resource.allow_poll")
        canonical = canonicalize_ranges(self.hard_denied)
        object.__setattr__(self, "hard_denied", canonical)
        seen_offsets: set[int] = set()
        for reg in self.writable_registers:
            if reg.offset in seen_offsets:
                raise PolicyError(
                    f"duplicate writable register offset: {reg.offset}"
                )
            seen_offsets.add(reg.offset)
        sorted_writable = sorted(
            self.writable_registers, key=lambda w: w.offset
        )
        object.__setattr__(self, "writable_registers", sorted_writable)

    def canonical_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {
            "allow_poll": self.allow_poll,
            "allow_unknown_reads": self.allow_unknown_reads,
            "hard_denied": self.hard_denied,
            "id": self.id,
            "name": self.name,
        }
        if self.writable_registers:
            d["writable_registers"] = [
                w.canonical_dict() for w in self.writable_registers
            ]
        return d


@dataclasses.dataclass(frozen=True)
class TargetPolicyManifest:
    """Target-wide policy manifest."""

    schema_version: int = SCHEMA_VERSION
    allow_mutating_sessions: bool = False
    max_snapshot_items: int = MAX_SNAPSHOT_ITEMS
    audit_capacity: int = 1024
    resources: list[ResourcePolicyManifest] = dataclasses.field(
        default_factory=list
    )

    def __post_init__(self) -> None:
        if self.schema_version != SCHEMA_VERSION:
            raise PolicyError(
                f"unsupported schema_version: {self.schema_version}"
            )
        _require_bool(self.allow_mutating_sessions, "allow_mutating_sessions")
        snap = _require_int(
            self.max_snapshot_items, "max_snapshot_items", minimum=1
        )
        if snap > MAX_SNAPSHOT_ITEMS:
            raise PolicyError(
                f"max_snapshot_items ({snap}) exceeds maximum ({MAX_SNAPSHOT_ITEMS})"
            )
        _require_int(self.audit_capacity, "audit_capacity", minimum=1)
        if len(self.resources) > MAX_RESOURCES:
            raise PolicyError(
                f"resource count ({len(self.resources)}) exceeds maximum ({MAX_RESOURCES})"
            )

        seen_ids: set[int] = set()
        seen_names: set[str] = set()
        for res in self.resources:
            if res.id in seen_ids:
                raise PolicyError(f"duplicate resource id: {res.id}")
            if res.name in seen_names:
                raise PolicyError(f"duplicate resource name: {res.name}")
            seen_ids.add(res.id)
            seen_names.add(res.name)

        sorted_resources = sorted(self.resources, key=lambda r: r.id)
        object.__setattr__(self, "resources", sorted_resources)

    @classmethod
    def engineering_default(
        cls, resources: list[dict[str, Any]] | dict[int, str]
    ) -> TargetPolicyManifest:
        """Constructs the default Phase 1 engineering ceiling manifest."""
        res_list: list[ResourcePolicyManifest] = []
        if isinstance(resources, dict):
            for res_id, name in resources.items():
                res_list.append(
                    ResourcePolicyManifest(
                        id=res_id,
                        name=name,
                        allow_unknown_reads=True,
                        allow_poll=False,
                        hard_denied=[],
                    )
                )
        else:
            for item in resources:
                res_list.append(
                    ResourcePolicyManifest(
                        id=item["id"],
                        name=item["name"],
                        allow_unknown_reads=True,
                        allow_poll=False,
                        hard_denied=[],
                    )
                )
        return cls(resources=res_list)

    def canonical_dict(self) -> dict[str, Any]:
        return {
            "allow_mutating_sessions": self.allow_mutating_sessions,
            "audit_capacity": self.audit_capacity,
            "max_snapshot_items": self.max_snapshot_items,
            "resources": [r.canonical_dict() for r in self.resources],
            "schema_version": self.schema_version,
        }

    def canonical_json(self) -> bytes:
        """Returns the canonical byte form of the policy manifest."""
        return json.dumps(
            self.canonical_dict(),
            sort_keys=True,
            separators=(",", ":"),
            ensure_ascii=True,
        ).encode("utf-8")

    def policy_digest(self) -> str:
        """Returns the SHA-256 digest formatted as 'sha256:<hex>'."""
        return "sha256:" + hashlib.sha256(self.canonical_json()).hexdigest()

    def narrow_with(
        self, runtime: TargetPolicyManifest
    ) -> TargetPolicyManifest:
        """Verifies that `runtime` narrows or preserves this baseline manifest."""
        if self.schema_version != runtime.schema_version:
            raise NarrowingError("schema version mismatch")

        if not self.allow_mutating_sessions and runtime.allow_mutating_sessions:
            raise NarrowingError("cannot enable mutating sessions")

        if runtime.max_snapshot_items > self.max_snapshot_items:
            raise NarrowingError(
                f"snapshot limit {runtime.max_snapshot_items} exceeds baseline {self.max_snapshot_items}"
            )

        if runtime.audit_capacity > self.audit_capacity:
            raise NarrowingError(
                f"audit capacity {runtime.audit_capacity} exceeds baseline {self.audit_capacity}"
            )

        base_res_map = {r.id: r for r in self.resources}
        for rt_res in runtime.resources:
            base_res = base_res_map.get(rt_res.id)
            if base_res is None:
                raise NarrowingError(
                    f"unknown resource {rt_res.id} in runtime policy"
                )

            if rt_res.name != base_res.name:
                raise NarrowingError(
                    f"resource {rt_res.id} name mismatch: baseline '{base_res.name}', runtime '{rt_res.name}'"
                )

            if not base_res.allow_unknown_reads and rt_res.allow_unknown_reads:
                raise NarrowingError(
                    f"cannot enable unknown reads on resource {rt_res.id}"
                )

            if not base_res.allow_poll and rt_res.allow_poll:
                raise NarrowingError(
                    f"cannot enable polling on resource {rt_res.id}"
                )

            for b_start, b_end in base_res.hard_denied:
                covered = any(
                    r_start <= b_start and b_end <= r_end
                    for r_start, r_end in rt_res.hard_denied
                )
                if not covered:
                    raise NarrowingError(
                        f"runtime hard denials for resource {rt_res.id} fail to cover baseline denial [{b_start}, {b_end})"
                    )

            base_writable = {w.offset: w for w in base_res.writable_registers}
            for rt_w in rt_res.writable_registers:
                base_w = base_writable.get(rt_w.offset)
                if base_w is None:
                    raise NarrowingError(
                        f"writable register on resource {rt_res.id} at offset {rt_w.offset} widened baseline"
                    )
                if (rt_w.allow_mask & ~base_w.allow_mask) != 0:
                    raise NarrowingError(
                        f"mask for writable register on resource {rt_res.id} at offset {rt_w.offset} widened baseline"
                    )
                if not base_w.allow_rmw and rt_w.allow_rmw:
                    raise NarrowingError(
                        f"writable register on resource {rt_res.id} at offset {rt_w.offset} widened baseline"
                    )
                if (
                    base_w.require_precondition
                    and not rt_w.require_precondition
                ):
                    raise NarrowingError(
                        f"writable register on resource {rt_res.id} at offset {rt_w.offset} widened baseline"
                    )
                if (rt_w.precondition_mask & ~base_w.precondition_mask) != 0:
                    raise NarrowingError(
                        f"mask for writable register on resource {rt_res.id} at offset {rt_w.offset} widened baseline"
                    )

        return runtime

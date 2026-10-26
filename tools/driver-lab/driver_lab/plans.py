# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Plan validation, canonicalization, and digests.

Plans are data, not code: no expressions, no arbitrary driver URLs,
component monikers, or protocol paths. Validation rejects unknown keys
everywhere so nothing can be smuggled past review. Canonicalization
normalizes numeric representations and key order -- operation order is
semantic and preserved -- so any change to an executable field changes the
digest and invalidates approval.

The schema carries the phase 2 (managed takeover) keys as reserved:
setting one fails with a distinct reserved-key error, so phase 2 can
adopt them without a schema-shape change.
"""

import hashlib
import json
from collections.abc import Mapping
from typing import Any

SCHEMA_VERSION = 1

_TOP_KEYS = {
    "schema_version",
    "run_id",
    "case_id",
    "target",
    "node",
    "access",
    "operations",
}
_TARGET_KEYS = {
    "selector",
    "expected_boot_id",
    "requires_recovery",
    "requires_serial",
}
_NODE_KEYS = {
    "id",
    "driver_moniker",
    "expected_unclaimed",
    "expected_resource_digest",
} | {
    "expected_bound_driver_url",
    "expected_topology_generation",
}
_ACCESS_KEYS = {
    "mode",
    "activation",
    "requires_target_policy",
    "requires_target_audit",
    "requires_target_local_timing",
    "restoration",
}
# Keys the schema carries for phase 2 (managed takeover) so their later
# arrival changes no schema shape. A phase 1 plan that sets one fails
# with a reserved-key error, never an unknown-key error.
_RESERVED_PHASE2_KEYS = {
    "node.expected_bound_driver_url",
    "node.expected_topology_generation",
    "access.restoration",
}
_MAX_STRING = 128


class PlanError(Exception):
    """The plan is structurally invalid or requests something unsupported."""


def _require_str(value: object, what: str) -> str:
    if not isinstance(value, str) or not value or len(value) > _MAX_STRING:
        raise PlanError(
            f"{what} must be a non-empty string of at most {_MAX_STRING} characters"
        )
    return value


def _require_bool(value: object, what: str) -> bool:
    if not isinstance(value, bool):
        raise PlanError(f"{what} must be a boolean")
    return value


def _require_keys(
    mapping: Mapping[str, object], allowed: set[str], what: str
) -> None:
    if not isinstance(mapping, Mapping):
        raise PlanError(f"{what} must be an object")
    unknown = set(mapping) - allowed
    if unknown:
        raise PlanError(f"{what} has unknown keys: {sorted(unknown)}")
    reserved = {
        key for key in mapping if f"{what}.{key}" in _RESERVED_PHASE2_KEYS
    }
    if reserved:
        raise PlanError(
            f"{what} keys {sorted(reserved)} are reserved until phase 2 (managed takeover)"
        )


def _parse_offset(value: object, what: str) -> int:
    if isinstance(value, bool):
        raise PlanError(f"{what} must be an integer or hex string")
    if isinstance(value, int):
        offset = value
    elif isinstance(value, str):
        text = value.strip().lower()
        try:
            offset = int(text, 16) if text.startswith("0x") else int(text, 10)
        except ValueError as error:
            raise PlanError(
                f"{what} is not a valid offset: {value!r}"
            ) from error
    else:
        raise PlanError(f"{what} must be an integer or hex string")
    if offset < 0:
        raise PlanError(f"{what} must be non-negative")
    return offset


def _require_uint32(value: object, what: str) -> int:
    if isinstance(value, bool):
        raise PlanError(f"{what} must be an integer or hex string")
    if isinstance(value, int):
        val = value
    elif isinstance(value, str):
        text = value.strip().lower()
        try:
            val = int(text, 16) if text.startswith("0x") else int(text, 10)
        except ValueError as error:
            raise PlanError(
                f"{what} is not a valid integer: {value!r}"
            ) from error
    else:
        raise PlanError(f"{what} must be an integer or hex string")
    if not (0 <= val <= 0xFFFF_FFFF):
        raise PlanError(f"{what} must be in range [0, 0xffffffff]")
    return val


_READ32_KINDS = ("mmio_read32", "state_read32")
_WRITE32_KINDS = ("mmio_write32", "knob_write32", "trigger_write32")
_POLL32_KINDS = ("mmio_poll32", "state_poll32")


def _resolve_resource(
    mapping: Mapping[str, object],
    what: str,
    default_state0: bool,
    kind: str = "",
) -> str:
    if default_state0 and "resource" not in mapping:
        resource = "state0"
    else:
        resource = _require_str(mapping.get("resource"), f"{what}.resource")
    if default_state0 and resource.startswith("mmio"):
        raise PlanError(
            f"{what}.resource {resource!r} is an MMIO resource; use mmio_read32, mmio_write32, or mmio_poll32 instead of {kind}"
        )
    if not default_state0 and resource.startswith("state"):
        raise PlanError(
            f"{what}.resource {resource!r} is a StateBank resource; use state_read32, state_poll32, knob_write32, or trigger_write32 instead of {kind}"
        )
    return resource


def _validate_read32_op(
    mapping: Mapping[str, Any], kind: str, what: str
) -> dict[str, Any]:
    _require_keys(mapping, {"kind", "resource", "offset"}, what)
    resource = _resolve_resource(
        mapping, what, default_state0=(kind == "state_read32"), kind=kind
    )
    return {
        "kind": kind,
        "resource": resource,
        "offset": _parse_offset(mapping.get("offset"), f"{what}.offset"),
    }


def _validate_write32_op(
    mapping: Mapping[str, Any], kind: str, what: str
) -> dict[str, Any]:
    allowed_keys = {
        "kind",
        "resource",
        "offset",
        "value",
        "write_mask",
        "precondition",
        "readback",
    }
    if kind == "trigger_write32":
        allowed_keys = allowed_keys | {"arg"}
    _require_keys(mapping, allowed_keys, what)
    resource = _resolve_resource(
        mapping,
        what,
        default_state0=(kind in ("knob_write32", "trigger_write32")),
        kind=kind,
    )
    offset = _parse_offset(mapping.get("offset"), f"{what}.offset")
    if kind == "trigger_write32":
        if "value" in mapping and "arg" in mapping:
            raise PlanError(f"{what} cannot specify both value and arg")
        if "value" in mapping:
            value = _require_uint32(mapping.get("value"), f"{what}.value")
        elif "arg" in mapping:
            value = _require_uint32(mapping.get("arg"), f"{what}.arg")
        else:
            value = 1
    else:
        if "value" not in mapping:
            raise PlanError(f"{what}.value is required")
        value = _require_uint32(mapping.get("value"), f"{what}.value")
    write_mask = (
        _require_uint32(mapping.get("write_mask"), f"{what}.write_mask")
        if "write_mask" in mapping
        else 0xFFFF_FFFF
    )
    precondition = None
    if "precondition" in mapping and mapping["precondition"] is not None:
        prec = mapping["precondition"]
        prec_what = f"{what}.precondition"
        if not isinstance(prec, Mapping):
            raise PlanError(f"{prec_what} must be an object")
        _require_keys(prec, {"expected", "mask"}, prec_what)
        if "expected" not in prec:
            raise PlanError(f"{prec_what}.expected is required")
        p_expected = _require_uint32(
            prec.get("expected"), f"{prec_what}.expected"
        )
        p_mask = (
            _require_uint32(prec.get("mask"), f"{prec_what}.mask")
            if "mask" in prec
            else 0xFFFF_FFFF
        )
        precondition = {"expected": p_expected, "mask": p_mask}
    readback = (
        _require_bool(mapping.get("readback"), f"{what}.readback")
        if "readback" in mapping
        else (False if kind == "trigger_write32" else True)
    )
    res_write: dict[str, Any] = {
        "kind": kind,
        "resource": resource,
        "offset": offset,
        "value": value,
        "write_mask": write_mask,
        "readback": readback,
    }
    if precondition is not None:
        res_write["precondition"] = precondition
    return res_write


def _validate_poll32_op(
    mapping: Mapping[str, Any], kind: str, what: str, *, in_sequence: bool
) -> dict[str, Any]:
    _require_keys(
        mapping,
        {
            "kind",
            "resource",
            "offset",
            "expected",
            "mask",
            "interval_ns",
            "timeout_ns",
            "interval_s",
            "timeout_s",
        },
        what,
    )
    resource = _resolve_resource(
        mapping, what, default_state0=(kind == "state_poll32"), kind=kind
    )
    offset = _parse_offset(mapping.get("offset"), f"{what}.offset")
    if "expected" not in mapping:
        raise PlanError(f"{what}.expected is required")
    expected = _require_uint32(mapping.get("expected"), f"{what}.expected")
    mask = (
        _require_uint32(mapping.get("mask"), f"{what}.mask")
        if "mask" in mapping
        else 0xFFFF_FFFF
    )
    if "interval_ns" in mapping:
        interval_ns = int(mapping["interval_ns"])
    elif "interval_s" in mapping:
        interval_ns = int(float(mapping["interval_s"]) * 1e9)
    else:
        interval_ns = 1_000_000
    if not in_sequence and interval_ns < 0:
        raise PlanError(f"{what}.interval_ns must be non-negative")

    if "timeout_ns" in mapping:
        timeout_ns = int(mapping["timeout_ns"])
    elif "timeout_s" in mapping:
        timeout_ns = int(float(mapping["timeout_s"]) * 1e9)
    else:
        if kind == "state_poll32":
            timeout_ns = 100_000_000
        else:
            raise PlanError(f"{what} requires timeout_ns or timeout_s")
    if in_sequence:
        if timeout_ns <= 0 or timeout_ns > 10_000_000_000:
            raise PlanError(f"{what}.timeout_ns out of range")
    else:
        if timeout_ns <= 0:
            raise PlanError(f"{what}.timeout_ns must be positive")
        if timeout_ns > 10_000_000_000:
            raise PlanError(f"{what}.timeout_ns exceeds 10s ceiling")

    return {
        "kind": kind,
        "resource": resource,
        "offset": offset,
        "expected": expected,
        "mask": mask,
        "interval_ns": interval_ns,
        "timeout_ns": timeout_ns,
    }


def _validate_operation(operation: object, index: int) -> dict[str, Any]:
    what = f"operations[{index}]"
    if not isinstance(operation, Mapping):
        raise PlanError(f"{what} must be an object")
    kind = operation.get("kind")
    if isinstance(kind, str) and kind in _READ32_KINDS:
        return _validate_read32_op(operation, kind, what)
    if kind == "mmio_snapshot32":
        _require_keys(operation, {"kind", "items"}, what)
        items = operation.get("items")
        if not isinstance(items, list) or not items:
            raise PlanError(f"{what}.items must be a non-empty list")
        canonical_items = []
        for item_index, item in enumerate(items):
            item_what = f"{what}.items[{item_index}]"
            if not isinstance(item, Mapping):
                raise PlanError(f"{item_what} must be an object")
            _require_keys(item, {"resource", "offset"}, item_what)
            canonical_items.append(
                {
                    "resource": _require_str(
                        item.get("resource"), f"{item_what}.resource"
                    ),
                    "offset": _parse_offset(
                        item.get("offset"), f"{item_what}.offset"
                    ),
                }
            )
        return {"kind": "mmio_snapshot32", "items": canonical_items}
    if isinstance(kind, str) and kind in _WRITE32_KINDS:
        return _validate_write32_op(operation, kind, what)
    if isinstance(kind, str) and kind in _POLL32_KINDS:
        return _validate_poll32_op(operation, kind, what, in_sequence=False)
    if kind == "sequence":
        _require_keys(operation, {"kind", "items"}, what)
        items = operation.get("items")
        if not isinstance(items, list) or not items:
            raise PlanError(f"{what}.items must be a non-empty list")
        if len(items) > 64:
            raise PlanError(f"{what}.items exceeds maximum of 64 items")
        canonical_items = []
        for item_index, item in enumerate(items):
            item_what = f"{what}.items[{item_index}]"
            if not isinstance(item, Mapping):
                raise PlanError(f"{item_what} must be an object")
            item_kind = item.get("kind")
            if isinstance(item_kind, str) and item_kind in _READ32_KINDS:
                canonical_items.append(
                    _validate_read32_op(item, item_kind, item_what)
                )
            elif isinstance(item_kind, str) and item_kind in _WRITE32_KINDS:
                canonical_items.append(
                    _validate_write32_op(item, item_kind, item_what)
                )
            elif isinstance(item_kind, str) and item_kind in _POLL32_KINDS:
                canonical_items.append(
                    _validate_poll32_op(
                        item, item_kind, item_what, in_sequence=True
                    )
                )
            elif item_kind == "delay_ns":
                _require_keys(item, {"kind", "duration_ns"}, item_what)
                duration = int(item.get("duration_ns", 0))
                if duration < 0 or duration > 1_000_000_000:
                    raise PlanError(
                        f"{item_what}.duration_ns must be between 0 and 1s (1_000_000_000 ns)"
                    )
                canonical_items.append(
                    {"kind": "delay_ns", "duration_ns": duration}
                )
            elif item_kind == "barrier":
                _require_keys(item, {"kind", "variant"}, item_what)
                variant = item.get("variant", "memory")
                if variant != "memory":
                    raise PlanError(
                        f"{item_what}.variant {variant!r} is unsupported"
                    )
                canonical_items.append({"kind": "barrier", "variant": variant})
            else:
                raise PlanError(
                    f"{item_what}.kind is unsupported: {item_kind!r}"
                )
        return {"kind": "sequence", "items": canonical_items}
    if kind == "delay_ns":
        _require_keys(operation, {"kind", "duration_ns"}, what)
        duration = int(operation.get("duration_ns", 0))
        if duration < 0 or duration > 1_000_000_000:
            raise PlanError(
                f"{what}.duration_ns must be between 0 and 1s (1_000_000_000 ns)"
            )
        return {"kind": "delay_ns", "duration_ns": duration}
    if kind == "barrier":
        _require_keys(operation, {"kind", "variant"}, what)
        variant = operation.get("variant", "memory")
        if variant != "memory":
            raise PlanError(f"{what}.variant {variant!r} is unsupported")
        return {"kind": "barrier", "variant": variant}
    if kind == "fidl_call":
        _require_keys(operation, {"kind", "method", "args"}, what)
        method = _require_str(operation.get("method"), f"{what}.method")
        args = operation.get("args")
        if args is not None and not isinstance(args, Mapping):
            raise PlanError(f"{what}.args must be an object")
        canonical_args: dict[str, Any] = {}
        if isinstance(args, Mapping):
            for k, v in args.items():
                if not isinstance(k, str):
                    raise PlanError(f"{what}.args keys must be strings")
                canonical_args[k] = v
        return {
            "kind": "fidl_call",
            "method": method,
            "args": canonical_args,
        }
    raise PlanError(f"{what}.kind is unsupported: {kind!r}")


def validate_plan(plan: Mapping[str, object]) -> dict[str, Any]:
    """Validates `plan` and returns its canonical structure.

    Rejects unknown keys at every level, unsupported access modes and
    activations (managed takeover is phase 2), and malformed operations.
    """
    _require_keys(plan, _TOP_KEYS, "plan")
    if plan.get("schema_version") != SCHEMA_VERSION:
        raise PlanError("schema_version must be 1")

    run_id = _require_str(plan.get("run_id"), "run_id")
    case_id = _require_str(plan.get("case_id"), "case_id")

    target_in = plan.get("target")
    _require_keys(target_in, _TARGET_KEYS, "target")  # type: ignore[arg-type]
    assert isinstance(target_in, Mapping)
    target: dict[str, Any] = {
        "selector": _require_str(target_in.get("selector"), "target.selector")
    }
    if "expected_boot_id" in target_in:
        target["expected_boot_id"] = _require_str(
            target_in.get("expected_boot_id"), "target.expected_boot_id"
        )
    if "requires_recovery" in target_in:
        target["requires_recovery"] = _require_bool(
            target_in.get("requires_recovery"), "target.requires_recovery"
        )
    if "requires_serial" in target_in:
        target["requires_serial"] = _require_bool(
            target_in.get("requires_serial"), "target.requires_serial"
        )

    node_in = plan.get("node")
    _require_keys(node_in, _NODE_KEYS, "node")  # type: ignore[arg-type]
    assert isinstance(node_in, Mapping)
    node: dict[str, Any] = {"id": _require_str(node_in.get("id"), "node.id")}
    if "driver_moniker" in node_in:
        node["driver_moniker"] = _require_str(
            node_in.get("driver_moniker"), "node.driver_moniker"
        )
    if "expected_unclaimed" in node_in:
        node["expected_unclaimed"] = _require_bool(
            node_in.get("expected_unclaimed"), "node.expected_unclaimed"
        )
    if "expected_resource_digest" in node_in:
        node["expected_resource_digest"] = _require_str(
            node_in.get("expected_resource_digest"),
            "node.expected_resource_digest",
        )

    access_in = plan.get("access")
    _require_keys(access_in, _ACCESS_KEYS, "access")  # type: ignore[arg-type]
    assert isinstance(access_in, Mapping)
    mode = access_in.get("mode")
    if mode not in ("direct", "proxy", "in-situ"):
        raise PlanError(f"access.mode is unsupported: {mode!r}")
    access: dict[str, Any] = {"mode": mode}
    activation = access_in.get("activation")
    if mode == "proxy":
        if activation == "takeover":
            raise PlanError(
                "access.activation 'takeover' is not supported until phase 2"
            )
        if activation not in (None, "bind-unclaimed", "in-situ"):
            raise PlanError(f"access.activation is unsupported: {activation!r}")
        access["activation"] = activation or "bind-unclaimed"
    elif mode == "in-situ":
        if activation == "takeover":
            raise PlanError(
                "access.activation 'takeover' is not supported until phase 2"
            )
        if activation not in (None, "in-situ"):
            raise PlanError(f"access.activation is unsupported: {activation!r}")
        access["activation"] = "in-situ"
    elif activation is not None:
        raise PlanError(
            "access.activation is only valid in proxy or in-situ mode"
        )
    for flag in (
        "requires_target_policy",
        "requires_target_audit",
        "requires_target_local_timing",
    ):
        value = access_in.get(flag, False)
        access[flag] = _require_bool(value, f"access.{flag}")

    operations_in = plan.get("operations")
    if not isinstance(operations_in, list) or not operations_in:
        raise PlanError("operations must be a non-empty list")
    operations = [
        _validate_operation(operation, index)
        for index, operation in enumerate(operations_in)
    ]

    return {
        "schema_version": SCHEMA_VERSION,
        "run_id": run_id,
        "case_id": case_id,
        "target": target,
        "node": node,
        "access": access,
        "operations": operations,
    }


def canonical_json(plan: Mapping[str, object]) -> bytes:
    """The canonical byte form of `plan`: validated, keys sorted, numbers
    normalized, operation order preserved."""
    canonical = validate_plan(plan)
    return json.dumps(
        canonical, sort_keys=True, separators=(",", ":"), ensure_ascii=True
    ).encode()


def plan_digest(plan: Mapping[str, object]) -> str:
    """The canonical plan digest, as `sha256:<hex>`."""
    return "sha256:" + hashlib.sha256(canonical_json(plan)).hexdigest()


def is_mutating_plan(plan: Mapping[str, object]) -> bool:
    """Whether `plan` contains any mutating operations."""
    operations = plan.get("operations")
    if not isinstance(operations, list):
        return False
    for op in operations:
        if not isinstance(op, Mapping):
            continue
        kind = op.get("kind")
        if kind in _WRITE32_KINDS:
            return True
        if kind == "sequence":
            items = op.get("items")
            if isinstance(items, list):
                for item in items:
                    if (
                        isinstance(item, Mapping)
                        and item.get("kind") in _WRITE32_KINDS
                    ):
                        return True
    return False

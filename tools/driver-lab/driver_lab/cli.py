# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Command-line interface for driver-lab host operations.

Structured JSON results go to stdout; diagnostics go to stderr. Exit
categories follow the host specification: 0 for success, 2 for local
argument, plan, or permission errors; `run` returns the run's exit
category, 7 when evidence cannot be persisted, and 4 when interrupted.
"""

import argparse
import asyncio
import dataclasses
import datetime
import json
import sys
from collections.abc import Sequence
from pathlib import Path
from typing import Any

from driver_lab.api import (
    EXIT_ACTIVATION,
    EXIT_EVIDENCE,
    EXIT_OPERATION,
    DriverLab,
)
from driver_lab.consent import ConsentDecision
from driver_lab.discovery import (
    DEFAULT_PROXY_DRIVER_URL,
    DiscoveryError,
    NodeDiscovery,
    ProxyActivator,
)
from driver_lab.evidence import EvidenceError
from driver_lab.models import AccessClass, AccessRequest, Decision, ReadGrant
from driver_lab.permissions import (
    GrantStoreError,
    Outcome,
    add_grant,
    load_grants,
    resolve,
    revoke_grant,
)
from driver_lab.plans import PlanError, plan_digest, validate_plan
from driver_lab.transport import (
    AllowRule,
    ProxyTransport,
    SessionContext,
    SessionMode,
    TransportError,
)

EXIT_SUCCESS = 0
EXIT_ERROR = 2


def _grant_row(grant: ReadGrant) -> dict[str, object]:
    row = dataclasses.asdict(grant)
    row["access"] = grant.access.value
    row["decision"] = grant.decision.value
    row["grant_id"] = grant.grant_id
    return row


def _emit(payload: dict[str, object]) -> None:
    print(json.dumps(payload, sort_keys=True, indent=2))


def _load_plan(path: Path) -> dict[str, object]:
    with path.open("r") as file:
        loaded = json.load(file)
    if not isinstance(loaded, dict):
        raise PlanError("plan file must contain a JSON object")
    return loaded


class _StdinConsent:
    """Interactive consent on the controlling terminal.

    Prints the exact access rule and the side-effect warning to stderr
    (stdout stays reserved for JSON results) and reads one decision.
    """

    _CHOICES = {
        "a": ConsentDecision.ALLOW_ONCE,
        "A": ConsentDecision.ALWAYS_ALLOW,
        "d": ConsentDecision.DENY_ONCE,
        "D": ConsentDecision.ALWAYS_DENY,
    }

    async def request_consent(
        self, request: AccessRequest, warning: str
    ) -> ConsentDecision:
        print(
            f"Consent required: {request.access.value} {request.resource} "
            f"offset {request.offset:#x} width {request.width} "
            f"(node {request.node_id}, digest {request.resource_digest})",
            file=sys.stderr,
        )
        print(warning, file=sys.stderr)
        while True:
            print(
                "[a] allow once  [A] always allow  [d] deny once  [D] always deny: ",
                end="",
                file=sys.stderr,
                flush=True,
            )
            choice = sys.stdin.readline().strip()
            decision = self._CHOICES.get(choice)
            if decision is not None:
                return decision
            print(f"unrecognized choice: {choice!r}", file=sys.stderr)


def _permissions_list(args: argparse.Namespace) -> int:
    grants = load_grants(args.grants)
    _emit({"grants": [_grant_row(grant) for grant in grants]})
    return EXIT_SUCCESS


def _permissions_revoke(args: argparse.Namespace) -> int:
    if not revoke_grant(args.grants, args.grant_id):
        print(
            json.dumps({"error": f"no grant with id {args.grant_id}"}),
            file=sys.stderr,
        )
        return EXIT_ERROR
    _emit({"revoked": args.grant_id})
    return EXIT_SUCCESS


def _permissions_explain(args: argparse.Namespace) -> int:
    grants = load_grants(args.grants)
    if getattr(args, "plan", None) is not None:
        plan = _load_plan(args.plan)
        canonical = validate_plan(plan)
        target_scope = args.target_scope or canonical["target"]["selector"]
        node_id = args.node_id or canonical["node"]["id"]
        resource_digest = args.resource_digest or canonical["node"].get(
            "expected_resource_digest"
        )
        if not resource_digest:
            print(
                json.dumps(
                    {
                        "error": (
                            "plan node has no expected_resource_digest; "
                            "specify --resource-digest or include it in the plan"
                        )
                    }
                ),
                file=sys.stderr,
            )
            return EXIT_ERROR

        requests: list[tuple[int, AccessRequest]] = []
        for index, op in enumerate(canonical["operations"]):
            kind = op["kind"]
            if kind in ("mmio_read32", "state_read32"):
                requests.append(
                    (
                        index,
                        AccessRequest(
                            target_scope=target_scope,
                            node_id=node_id,
                            resource_digest=resource_digest,
                            resource=op["resource"],
                            offset=op["offset"],
                            width=4,
                            access=AccessClass.READ_ONCE,
                        ),
                    )
                )
            elif kind == "mmio_snapshot32":
                for item in op["items"]:
                    requests.append(
                        (
                            index,
                            AccessRequest(
                                target_scope=target_scope,
                                node_id=node_id,
                                resource_digest=resource_digest,
                                resource=item["resource"],
                                offset=item["offset"],
                                width=4,
                                access=AccessClass.READ_ONCE,
                            ),
                        )
                    )
            elif kind in ("mmio_write32", "knob_write32", "trigger_write32"):
                requests.append(
                    (
                        index,
                        AccessRequest(
                            target_scope=target_scope,
                            node_id=node_id,
                            resource_digest=resource_digest,
                            resource=op["resource"],
                            offset=op["offset"],
                            width=4,
                            access=AccessClass.WRITE,
                        ),
                    )
                )
            elif kind in ("mmio_poll32", "state_poll32"):
                requests.append(
                    (
                        index,
                        AccessRequest(
                            target_scope=target_scope,
                            node_id=node_id,
                            resource_digest=resource_digest,
                            resource=op["resource"],
                            offset=op["offset"],
                            width=4,
                            access=AccessClass.POLL,
                        ),
                    )
                )
            elif kind == "sequence":
                for item in op["items"]:
                    item_kind = item["kind"]
                    if item_kind in ("mmio_read32", "state_read32"):
                        requests.append(
                            (
                                index,
                                AccessRequest(
                                    target_scope=target_scope,
                                    node_id=node_id,
                                    resource_digest=resource_digest,
                                    resource=item["resource"],
                                    offset=item["offset"],
                                    width=4,
                                    access=AccessClass.READ_ONCE,
                                ),
                            )
                        )
                    elif item_kind in (
                        "mmio_write32",
                        "knob_write32",
                        "trigger_write32",
                    ):
                        requests.append(
                            (
                                index,
                                AccessRequest(
                                    target_scope=target_scope,
                                    node_id=node_id,
                                    resource_digest=resource_digest,
                                    resource=item["resource"],
                                    offset=item["offset"],
                                    width=4,
                                    access=AccessClass.WRITE,
                                ),
                            )
                        )
                    elif item_kind in ("mmio_poll32", "state_poll32"):
                        requests.append(
                            (
                                index,
                                AccessRequest(
                                    target_scope=target_scope,
                                    node_id=node_id,
                                    resource_digest=resource_digest,
                                    resource=item["resource"],
                                    offset=item["offset"],
                                    width=4,
                                    access=AccessClass.POLL,
                                ),
                            )
                        )

        resolutions: list[dict[str, Any]] = []
        all_allowed = True
        for op_idx, req in requests:
            res = resolve(req, grants)
            allowed = res.decided and res.outcome is Outcome.ALLOWED
            if not allowed:
                all_allowed = False
            resolutions.append(
                {
                    "operation": op_idx,
                    "resource": req.resource,
                    "offset": req.offset,
                    "access": req.access.value,
                    "outcome": res.outcome.value,
                    "grant_id": res.grant.grant_id if res.grant else None,
                }
            )

        _emit(
            {
                "plan_digest": plan_digest(plan),
                "all_allowed": all_allowed,
                "resolutions": resolutions,
            }
        )
        return EXIT_SUCCESS

    if not (
        args.target_scope
        and args.node_id
        and args.resource_digest
        and args.resource
        and args.offset is not None
    ):
        print(
            json.dumps(
                {
                    "error": (
                        "either --plan or (--target-scope, --node-id, "
                        "--resource-digest, --resource, --offset) is required"
                    )
                }
            ),
            file=sys.stderr,
        )
        return EXIT_ERROR

    request = AccessRequest(
        target_scope=args.target_scope,
        node_id=args.node_id,
        resource_digest=args.resource_digest,
        resource=args.resource,
        offset=args.offset,
        width=args.width,
        access=AccessClass(args.access),
    )
    resolution = resolve(request, grants)
    _emit(
        {
            "outcome": resolution.outcome.value,
            "grant": _grant_row(resolution.grant) if resolution.grant else None,
        }
    )
    return EXIT_SUCCESS


def _permissions_add(args: argparse.Namespace) -> int:
    grant = ReadGrant(
        schema_version=1,
        target_scope=args.target_scope,
        node_id=args.node_id,
        resource_digest=args.resource_digest,
        resource=args.resource,
        offset=args.offset,
        width=args.width,
        access=AccessClass(args.access),
        decision=Decision(args.decision),
        approved_at=datetime.datetime.now(datetime.UTC).isoformat(),
        approval_source="cli",
    )
    add_grant(args.grants, grant)
    # The exact persisted rule, so the operator sees precisely what was
    # added -- never a region or wildcard.
    _emit({"added": _grant_row(grant)})
    return EXIT_SUCCESS


def _connect_transport(moniker: str, target: str | None) -> ProxyTransport:
    # Deferred import: the device connection path needs the
    # fuchsia-controller runtime, which permissions/plan commands and
    # their tests should not load. Tests substitute this seam.
    from driver_lab.fidl_transport import LazyFidlProxyTransport

    return LazyFidlProxyTransport(moniker=moniker, target=target)


def _connect_discovery(target: str | None = None) -> NodeDiscovery | None:
    from driver_lab.discovery import connect_discovery

    try:
        return connect_discovery(target=target)
    except Exception:
        return None


def _connect_activator(target: str | None = None) -> ProxyActivator | None:
    from driver_lab.discovery import connect_activator

    try:
        return connect_activator(target=target)
    except Exception:
        return None


def _list(args: argparse.Namespace) -> int:
    discovery = _connect_discovery(target=args.target)
    if discovery is None:
        print(
            json.dumps({"error": "node discovery is not available"}),
            file=sys.stderr,
        )
        return EXIT_ERROR

    async def run() -> int:
        if getattr(args, "debug_capable", False):
            nodes = await discovery.find_debug_capable()
        elif args.unclaimed:
            nodes = await discovery.find_unclaimed()
        else:
            nodes = await discovery.list_nodes()
        _emit({"nodes": [dataclasses.asdict(n) for n in nodes]})
        return EXIT_SUCCESS

    return asyncio.run(run())


def _describe(args: argparse.Namespace) -> int:
    moniker = getattr(args, "moniker", None)
    node_id = getattr(args, "node", None)
    if not moniker and not node_id:
        print(
            json.dumps({"error": "either --node or --moniker is required"}),
            file=sys.stderr,
        )
        return EXIT_ERROR

    if moniker:
        transport = _connect_transport(moniker=moniker, target=args.target)
        discovery = _connect_discovery(target=args.target)

        async def run_moniker() -> int:
            try:
                desc = await transport.describe()
            except TransportError as exc:
                print(
                    json.dumps({"error": f"describe failed: {exc}"}),
                    file=sys.stderr,
                )
                return EXIT_ERROR
            payload: dict[str, object] = {
                "moniker": moniker,
                "description": dataclasses.asdict(desc),
            }
            if discovery is not None:
                node = await discovery.describe_node(node_id or moniker)
                if node is not None:
                    payload["node"] = dataclasses.asdict(node)
            _emit(payload)
            return EXIT_SUCCESS

        return asyncio.run(run_moniker())

    discovery = _connect_discovery(target=args.target)
    if discovery is None:
        print(
            json.dumps({"error": "node discovery is not available"}),
            file=sys.stderr,
        )
        return EXIT_ERROR

    async def run() -> int:
        assert node_id is not None
        node = await discovery.describe_node(node_id)
        if node is None:
            print(
                json.dumps({"error": f"node {node_id!r} not found"}),
                file=sys.stderr,
            )
            return EXIT_ERROR
        _emit({"node": dataclasses.asdict(node)})
        return EXIT_SUCCESS

    return asyncio.run(run())


def _inspect(args: argparse.Namespace) -> int:
    transport = _connect_transport(moniker=args.moniker, target=args.target)
    discovery = _connect_discovery(target=args.target)

    async def run() -> int:
        try:
            desc = await transport.describe()
        except TransportError as exc:
            print(
                json.dumps({"error": f"inspect describe failed: {exc}"}),
                file=sys.stderr,
            )
            return EXIT_ERROR

        driver_url: str | None = None
        node_payload: dict[str, object] | None = None
        if discovery is not None:
            lookup_id = getattr(args, "node", None) or args.moniker
            node = await discovery.describe_node(lookup_id)
            if node is not None:
                driver_url = node.bound_driver_url
                node_payload = dataclasses.asdict(node)

        allowlist: list[AllowRule] = []
        target_res_id: int | None = None
        if args.resource is not None and args.offset is not None:
            res_info = desc.resource_named(args.resource)
            if res_info is None:
                print(
                    json.dumps(
                        {"error": f"unknown resource {args.resource!r}"}
                    ),
                    file=sys.stderr,
                )
                return EXIT_ERROR
            target_res_id = res_info.id
            allowlist.append(
                AllowRule(
                    resource=res_info.id,
                    offset=args.offset,
                    width=4,
                    access=AccessClass.READ_ONCE,
                )
            )

        ctx = SessionContext(
            run_id=f"inspect-{args.moniker}",
            case_id="inspect",
            plan_digest="inspect-session",
        )
        read_result: dict[str, object] | None = None
        audit_entries: list[dict[str, object]] = []
        try:
            session = await transport.open_session(
                ctx,
                desc.expectations(),
                allowlist,
                mode=SessionMode.READ_ONLY,
            )
            try:
                if target_res_id is not None and args.offset is not None:
                    outcome = await session.read32(target_res_id, args.offset)
                    read_result = {
                        "resource": args.resource,
                        "offset": args.offset,
                        "value": outcome.value,
                        "audit_seq": outcome.audit_seq,
                        "timestamp_ns": outcome.timestamp_ns,
                    }
                page = await session.read_audit(0, 64)
                audit_entries = [entry.to_json() for entry in page.entries]
            finally:
                await session.close()
        except Exception as exc:
            print(
                json.dumps({"error": f"inspect session failed: {exc}"}),
                file=sys.stderr,
            )
            return EXIT_ERROR

        payload: dict[str, object] = {
            "moniker": args.moniker,
            "driver_url": driver_url,
            "description": dataclasses.asdict(desc),
            "audit": audit_entries,
        }
        if node_payload is not None:
            payload["node"] = node_payload
        if read_result is not None:
            payload["read"] = read_result
        _emit(payload)
        return EXIT_SUCCESS

    return asyncio.run(run())


def _direct(args: argparse.Namespace) -> int:
    discovery = _connect_discovery(target=args.target)
    if discovery is None:
        print(
            json.dumps({"error": "node discovery is not available"}),
            file=sys.stderr,
        )
        return EXIT_ERROR

    async def run() -> int:
        node = await discovery.describe_node(args.node)
        if node is None:
            print(
                json.dumps({"error": f"node {args.node!r} not found"}),
                file=sys.stderr,
            )
            return EXIT_ERROR
        if not node.has_protocol(args.protocol):
            print(
                json.dumps(
                    {
                        "error": (
                            f"node {args.node!r} does not offer protocol {args.protocol!r}; "
                            f"available: {sorted(node.offers)}"
                        )
                    }
                ),
                file=sys.stderr,
            )
            return EXIT_ERROR
        _emit(
            {
                "node": args.node,
                "protocol": args.protocol,
                "status": "verified",
            }
        )
        return EXIT_SUCCESS

    return asyncio.run(run())


def _bind_proxy(args: argparse.Namespace) -> int:
    discovery = _connect_discovery(target=args.target)
    activator = _connect_activator(target=args.target)
    if discovery is None or activator is None:
        print(
            json.dumps({"error": "discovery or activator is not available"}),
            file=sys.stderr,
        )
        return EXIT_ERROR

    async def run() -> int:
        node = await discovery.describe_node(args.node)
        if node is None:
            print(
                json.dumps({"error": f"node {args.node!r} not found"}),
                file=sys.stderr,
            )
            return EXIT_ERROR
        if not node.is_unclaimed:
            print(
                json.dumps(
                    {
                        "error": (
                            f"node {args.node!r} is not unclaimed; bound to "
                            f"{node.bound_driver_url}"
                        )
                    }
                ),
                file=sys.stderr,
            )
            return EXIT_ACTIVATION
        driver_url = args.driver_url or DEFAULT_PROXY_DRIVER_URL
        try:
            await activator.bind_proxy(args.node, driver_url)
        except Exception as exc:
            print(
                json.dumps({"error": f"bind_proxy failed: {exc}"}),
                file=sys.stderr,
            )
            return EXIT_ACTIVATION

        post_desc = await discovery.describe_node(args.node)
        is_bound = not post_desc.is_unclaimed if post_desc else True
        _emit(
            {
                "node": args.node,
                "driver_url": driver_url,
                "bound": is_bound,
            }
        )
        return EXIT_SUCCESS

    return asyncio.run(run())


def _end_proxy(args: argparse.Namespace) -> int:
    discovery = _connect_discovery(target=args.target)
    activator = _connect_activator(target=args.target)
    if discovery is None or activator is None:
        print(
            json.dumps({"error": "discovery or activator is not available"}),
            file=sys.stderr,
        )
        return EXIT_ERROR

    async def run() -> int:
        driver_url = args.driver_url or DEFAULT_PROXY_DRIVER_URL
        try:
            await activator.end_proxy(args.node, driver_url)
        except Exception as exc:
            print(
                json.dumps({"error": f"end_proxy failed: {exc}"}),
                file=sys.stderr,
            )
            return EXIT_ACTIVATION

        # Verified teardown: verify the node is again unclaimed.
        post_desc = await discovery.describe_node(args.node)
        if post_desc is not None and not post_desc.is_unclaimed:
            print(
                json.dumps(
                    {
                        "error": (
                            f"verified teardown failed: node {args.node!r} is "
                            f"still bound to {post_desc.bound_driver_url}"
                        )
                    }
                ),
                file=sys.stderr,
            )
            return EXIT_ACTIVATION

        _emit(
            {
                "node": args.node,
                "driver_url": driver_url,
                "unclaimed": True,
            }
        )
        return EXIT_SUCCESS

    return asyncio.run(run())


def _run(args: argparse.Namespace) -> int:
    plan = _load_plan(args.plan)
    transport = _connect_transport(moniker=args.moniker, target=args.target)
    discovery = _connect_discovery(target=args.target)
    activator = (
        _connect_activator(target=args.target)
        if discovery is not None
        else None
    )
    access_cfg = plan.get("access")
    is_in_situ = isinstance(access_cfg, dict) and (
        access_cfg.get("mode") == "in-situ"
        or access_cfg.get("activation") == "in-situ"
    )
    lab = DriverLab(
        transport,
        grants_path=args.grants,
        evidence_root=args.evidence_dir,
        target_scope=args.target_scope,
        node_id=args.node_id,
        driver_moniker=args.moniker if is_in_situ else None,
        consent=_StdinConsent() if args.consent else None,
        discovery=discovery,
        activator=activator,
    )
    result = asyncio.run(lab.run_plan(plan))
    _emit(
        {
            "exit_category": result.exit_category,
            "evidence_dir": str(result.evidence_dir),
            "plan_digest": result.plan_digest,
            "failure": result.failure,
            "reads": [dataclasses.asdict(read) for read in result.reads],
            "writes": [dataclasses.asdict(write) for write in result.writes],
            "polls": [dataclasses.asdict(poll) for poll in result.polls],
            "sequences": [dataclasses.asdict(seq) for seq in result.sequences],
            "calls": list(result.calls),
        }
    )
    return result.exit_category


def _plan_digest(args: argparse.Namespace) -> int:
    _emit({"digest": plan_digest(_load_plan(args.plan))})
    return EXIT_SUCCESS


def _plan_validate(args: argparse.Namespace) -> int:
    plan = _load_plan(args.plan)
    _emit({"digest": plan_digest(plan), "plan": validate_plan(plan)})
    return EXIT_SUCCESS


def _offset(value: str) -> int:
    return int(value, 0)


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="driver-lab")
    subcommands = parser.add_subparsers(dest="command", required=True)

    permissions = subcommands.add_parser(
        "permissions", help="manage persistent operator read grants"
    )
    permission_commands = permissions.add_subparsers(
        dest="subcommand", required=True
    )

    list_parser = permission_commands.add_parser("list", help="list grants")
    list_parser.add_argument("--grants", type=Path, required=True)
    list_parser.set_defaults(handler=_permissions_list)

    revoke_parser = permission_commands.add_parser(
        "revoke", help="revoke one grant"
    )
    revoke_parser.add_argument("--grants", type=Path, required=True)
    revoke_parser.add_argument("--grant-id", required=True)
    revoke_parser.set_defaults(handler=_permissions_revoke)

    explain_parser = permission_commands.add_parser(
        "explain",
        help="resolve one access request or a plan against the grants",
    )
    explain_parser.add_argument("--grants", type=Path, required=True)
    explain_parser.add_argument(
        "--plan", type=Path, default=None, help="probe plan to explain"
    )
    explain_parser.add_argument("--target-scope", required=False, default=None)
    explain_parser.add_argument("--node-id", required=False, default=None)
    explain_parser.add_argument(
        "--resource-digest", required=False, default=None
    )
    explain_parser.add_argument("--resource", required=False, default=None)
    explain_parser.add_argument(
        "--offset", type=_offset, required=False, default=None
    )
    explain_parser.add_argument("--width", type=int, default=4)
    explain_parser.add_argument(
        "--access",
        choices=[access.value for access in AccessClass],
        default=AccessClass.READ_ONCE.value,
    )
    explain_parser.set_defaults(handler=_permissions_explain)

    add_parser = permission_commands.add_parser(
        "add", help="persist one exact read grant or denial"
    )
    add_parser.add_argument("--grants", type=Path, required=True)
    add_parser.add_argument("--target-scope", required=True)
    add_parser.add_argument("--node-id", required=True)
    add_parser.add_argument("--resource-digest", required=True)
    add_parser.add_argument("--resource", required=True)
    add_parser.add_argument("--offset", type=_offset, required=True)
    add_parser.add_argument("--width", type=int, default=4)
    add_parser.add_argument(
        "--access",
        choices=[access.value for access in AccessClass],
        default=AccessClass.READ_ONCE.value,
    )
    add_parser.add_argument(
        "--decision",
        choices=[decision.value for decision in Decision],
        required=True,
    )
    add_parser.set_defaults(handler=_permissions_add)

    list_cmd = subcommands.add_parser(
        "list", help="list hardware nodes on target"
    )
    list_cmd.add_argument("--target", default=None, help="target nodename")
    list_cmd.add_argument(
        "--unclaimed",
        action="store_true",
        help="only list unclaimed nodes eligible for proxy activation",
    )
    list_cmd.add_argument(
        "--debug-capable",
        action="store_true",
        help="only list active bound drivers exposing fuchsia.driver.lab.Service",
    )
    list_cmd.set_defaults(handler=_list)

    describe_cmd = subcommands.add_parser(
        "describe",
        help="describe a hardware node or embedded driver debug endpoint",
    )
    describe_cmd.add_argument(
        "--node", required=False, default=None, help="node ID / moniker"
    )
    describe_cmd.add_argument(
        "--moniker",
        required=False,
        default=None,
        help="live driver moniker exposing fuchsia.driver.lab.Service",
    )
    describe_cmd.add_argument("--target", default=None, help="target nodename")
    describe_cmd.set_defaults(handler=_describe)

    inspect_cmd = subcommands.add_parser(
        "inspect",
        help="inspect a live driver's embedded fuchsia.driver.lab.Service endpoint in-situ",
    )
    inspect_cmd.add_argument(
        "--moniker",
        required=True,
        help="component moniker of the active driver exposing fuchsia.driver.lab.Service",
    )
    inspect_cmd.add_argument(
        "--node",
        required=False,
        default=None,
        help="optional node ID for discovery metadata",
    )
    inspect_cmd.add_argument(
        "--resource",
        required=False,
        default=None,
        help="optional MMIO resource name for single-register inspection",
    )
    inspect_cmd.add_argument(
        "--offset",
        type=_offset,
        required=False,
        default=None,
        help="optional 32-bit register byte offset for single-register inspection",
    )
    inspect_cmd.add_argument("--target", default=None, help="target nodename")
    inspect_cmd.set_defaults(handler=_inspect)

    direct_cmd = subcommands.add_parser(
        "direct", help="verify direct connection to a published protocol"
    )
    direct_cmd.add_argument("--node", required=True, help="node ID / moniker")
    direct_cmd.add_argument(
        "--protocol",
        required=True,
        help="published protocol / service selector",
    )
    direct_cmd.add_argument("--target", default=None, help="target nodename")
    direct_cmd.set_defaults(handler=_direct)

    bind_cmd = subcommands.add_parser(
        "bind-proxy", help="bind proxy driver to an unclaimed node"
    )
    bind_cmd.add_argument("--node", required=True, help="node ID / moniker")
    bind_cmd.add_argument(
        "--driver-url",
        default=DEFAULT_PROXY_DRIVER_URL,
        help="proxy driver component URL",
    )
    bind_cmd.add_argument("--target", default=None, help="target nodename")
    bind_cmd.set_defaults(handler=_bind_proxy)

    end_cmd = subcommands.add_parser(
        "end-proxy", help="end proxy access and verify node is unclaimed"
    )
    end_cmd.add_argument("--node", required=True, help="node ID / moniker")
    end_cmd.add_argument(
        "--driver-url",
        default=DEFAULT_PROXY_DRIVER_URL,
        help="proxy driver component URL",
    )
    end_cmd.add_argument("--target", default=None, help="target nodename")
    end_cmd.set_defaults(handler=_end_proxy)

    run_parser = subcommands.add_parser(
        "run", help="run one probe plan to a finalized evidence bundle"
    )
    run_parser.add_argument("--plan", type=Path, required=True)
    run_parser.add_argument("--evidence-dir", type=Path, required=True)
    run_parser.add_argument("--grants", type=Path, required=True)
    run_parser.add_argument("--target-scope", required=True)
    run_parser.add_argument("--node-id", required=True)
    run_parser.add_argument(
        "--moniker",
        required=True,
        help="component moniker of the proxy driver on the target",
    )
    run_parser.add_argument(
        "--target",
        default=None,
        help="target nodename (default: ffx default target)",
    )
    run_parser.add_argument(
        "--consent",
        action="store_true",
        help="prompt interactively for undecided accesses (default: fail closed)",
    )
    run_parser.set_defaults(handler=_run)

    plan = subcommands.add_parser(
        "plan", help="validate and digest probe plans"
    )
    plan_commands = plan.add_subparsers(dest="subcommand", required=True)

    digest_parser = plan_commands.add_parser(
        "digest", help="canonical plan digest"
    )
    digest_parser.add_argument("--plan", type=Path, required=True)
    digest_parser.set_defaults(handler=_plan_digest)

    validate_parser = plan_commands.add_parser(
        "validate", help="validate and canonicalize a plan"
    )
    validate_parser.add_argument("--plan", type=Path, required=True)
    validate_parser.set_defaults(handler=_plan_validate)

    return parser


def main(argv: Sequence[str] | None = None) -> int:
    """Runs one CLI command; returns the exit category."""
    args = _build_parser().parse_args(argv)
    try:
        handler = args.handler
        result: int = handler(args)
        return result
    except EvidenceError as error:
        print(json.dumps({"error": str(error)}), file=sys.stderr)
        return EXIT_EVIDENCE
    except KeyboardInterrupt:
        # Cancellation inside a run finalizes evidence and reports
        # through the run result; reaching here means the interrupt
        # arrived outside that path.
        print(json.dumps({"error": "interrupted"}), file=sys.stderr)
        return EXIT_OPERATION
    except (
        GrantStoreError,
        PlanError,
        DiscoveryError,
        OSError,
        ValueError,
        json.JSONDecodeError,
    ) as error:
        print(json.dumps({"error": str(error)}), file=sys.stderr)
        return EXIT_ERROR


if __name__ == "__main__":
    sys.exit(main())

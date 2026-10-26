# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for independent recovery, serial capture, and interpretation.

Fulfills Phase 1 Milestone H5 (Spec Sections 3, 6.4, 14.1-14.3, 16, 17).
"""

import json
import tempfile
import unittest
from collections.abc import Sequence
from pathlib import Path
from typing import Any

from driver_lab.api import (
    EXIT_ACTIVATION,
    EXIT_SUCCESS,
    EXIT_TRANSPORT,
    EXIT_UNSUPPORTED,
    DriverLab,
)
from driver_lab.models import AccessClass, Decision, ReadGrant, ResourceKind
from driver_lab.permissions import save_grants
from driver_lab.recovery import (
    FakeRecoveryController,
    FakeSerialCapture,
    SerialCaptureResult,
    detect_panic,
    extract_panic_summary,
)
from driver_lab.transport import (
    AllowRule,
    Expectations,
    FakeProxyTarget,
    ProxyDescription,
    ProxySession,
    ResourceInfo,
    SessionContext,
    SessionMode,
    TransportError,
)

FAKE_RESOURCE = ResourceInfo(
    id=1,
    name="ctrl",
    logical_size=0x1000,
    digest="sha256:target-digest",
    kind=ResourceKind.MMIO,
)


def _make_description() -> ProxyDescription:
    return ProxyDescription(
        protocol_major=1,
        protocol_minor=0,
        proxy_generation=1,
        boot_id="boot-1",
        resource_digest="sha256:target-digest",
        policy_digest="sha256:policy-digest",
        resources=(FAKE_RESOURCE,),
        max_snapshot_items=64,
        audit_capacity=256,
        node_moniker="sample/node",
    )


def _make_target() -> FakeProxyTarget:
    target = FakeProxyTarget(_make_description())
    target.set_value(1, 16, 0x1234_5678)
    target.set_value(1, 32, 0xCAFE_BABE)
    return target


class DisconnectingProxyTarget(FakeProxyTarget):
    """FakeProxyTarget that fails the second read with TransportError."""

    def __init__(self, description: ProxyDescription) -> None:
        super().__init__(description)
        self.read_count = 0

    async def open_session(
        self,
        context: SessionContext,
        expectations: Expectations,
        allowlist: Sequence[AllowRule],
        mode: SessionMode = SessionMode.READ_ONLY,
    ) -> ProxySession:
        inner = await super().open_session(
            context, expectations, allowlist, mode
        )
        target_self = self

        class WrapperSession:
            def __getattr__(self, name: str) -> Any:
                return getattr(inner, name)

            async def read32(self, resource: int, offset: int) -> Any:
                target_self.read_count += 1
                if target_self.read_count > 1:
                    raise TransportError("channel disconnected")
                return await inner.read32(resource, offset)

        return WrapperSession()


def make_grant(**overrides: object) -> ReadGrant:
    fields: dict[str, Any] = dict(
        schema_version=1,
        target_scope="target-1",
        node_id="node-1",
        resource_digest="sha256:target-digest",
        resource="ctrl",
        offset=16,
        width=4,
        access=AccessClass.READ_ONCE,
        decision=Decision.ALLOW,
        approved_at="2026-09-08T00:00:00Z",
    )
    fields.update(overrides)
    return ReadGrant(**fields)


class RecoveryTest(unittest.IsolatedAsyncioTestCase):
    def setUp(self) -> None:
        self.tmp_dir = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp_dir.name)
        self.grants_path = self.root / "grants.toml"
        save_grants(self.grants_path, [make_grant()])
        self.evidence_root = self.root / "evidence"

    def tearDown(self) -> None:
        self.tmp_dir.cleanup()

    def test_detect_panic_and_extract_summary(self) -> None:
        benign_log = "[00001.000] [driver] normal boot and device init\n"
        self.assertFalse(detect_panic(benign_log))
        self.assertIsNone(extract_panic_summary(benign_log))

        panic_log = (
            "[00001.000] [driver] starting up\n"
            "[00002.500] [klog] ZIRCON KERNEL PANIC: fatal exception in thread\n"
            "[00002.501] [klog] {{{bt:0:0x1234}}}\n"
        )
        self.assertTrue(detect_panic(panic_log))
        self.assertEqual(
            extract_panic_summary(panic_log),
            "[00002.500] [klog] ZIRCON KERNEL PANIC: fatal exception in thread",
        )

        assert_fail_log = b"[00001.100] [klog] ASSERT FAILED at foo.cc:42\n"
        self.assertTrue(detect_panic(assert_fail_log))
        self.assertEqual(
            extract_panic_summary(assert_fail_log),
            "[00001.100] [klog] ASSERT FAILED at foo.cc:42",
        )

    async def test_serial_capture_lifecycle(self) -> None:
        capture = FakeSerialCapture(
            initial_lines=["[00001.000] boot: hello world"]
        )
        self.assertTrue(await capture.is_available())

        session = await capture.start()
        capture.emit_line("[00001.100] driver: starting session")

        curr = await session.get_current_log()
        self.assertIn(b"starting session", curr)

        result = await session.stop()
        self.assertIsInstance(result, SerialCaptureResult)
        self.assertEqual(result.source, "fake")
        self.assertEqual(result.lines_count, 2)
        self.assertFalse(result.contains_panic)

        meta = result.to_manifest_metadata()
        self.assertEqual(meta["status"], "captured")
        self.assertEqual(meta["lines"], 2)
        self.assertFalse(meta["panic_detected"])

    async def test_run_plan_with_serial_capture_evidence(self) -> None:
        target = _make_target()
        serial = FakeSerialCapture(
            initial_lines=[
                "[00001.000] boot: system initialized",
                "[00001.050] driver-lab: target proxy listening",
            ]
        )
        recovery = FakeRecoveryController()

        lab = DriverLab(
            target,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
            serial_capture=serial,
            recovery=recovery,
        )

        plan = {
            "schema_version": 1,
            "run_id": "run-serial-1",
            "case_id": "case-serial-1",
            "target": {"selector": "lab-target"},
            "node": {"id": "node-1"},
            "access": {"mode": "proxy"},
            "operations": [
                {
                    "kind": "mmio_read32",
                    "resource": "ctrl",
                    "offset": 16,
                }
            ],
        }

        result = await lab.run_plan(plan)
        self.assertTrue(result.ok)
        self.assertEqual(result.exit_category, EXIT_SUCCESS)

        # Verify serial.log exists in evidence directory
        serial_log_file = result.evidence_dir / "serial.log"
        self.assertTrue(serial_log_file.exists())
        content = serial_log_file.read_text()
        self.assertIn("boot: system initialized", content)

        # Verify manifest includes serial.log in files and serial_capture in metadata
        manifest = json.loads(
            (result.evidence_dir / "manifest.json").read_text()
        )
        self.assertIn("serial.log", manifest["files"])
        self.assertNotEqual(manifest["files"]["serial.log"], "not_applicable")
        self.assertIn("serial_capture", manifest)
        self.assertEqual(manifest["serial_capture"]["status"], "captured")
        self.assertEqual(manifest["serial_capture"]["lines"], 2)

    async def test_recovery_requirements_fail_closed(self) -> None:
        target = _make_target()

        # 1. Plan requires recovery, but none configured -> EXIT_UNSUPPORTED
        lab_no_rec = DriverLab(
            target,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
            recovery=None,
        )
        plan_rec = {
            "schema_version": 1,
            "run_id": "run-rec-unsupported",
            "case_id": "case-rec",
            "target": {
                "selector": "lab-target",
                "requires_recovery": True,
            },
            "node": {"id": "node-1"},
            "access": {"mode": "proxy"},
            "operations": [
                {"kind": "mmio_read32", "resource": "ctrl", "offset": 16}
            ],
        }
        res1 = await lab_no_rec.run_plan(plan_rec)
        self.assertEqual(res1.exit_category, EXIT_UNSUPPORTED)
        self.assertIn("requires_recovery", str(res1.failure))

        # 2. Recovery configured but unavailable -> EXIT_ACTIVATION
        unavail_rec = FakeRecoveryController(available=False)
        lab_unavail = DriverLab(
            target,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
            recovery=unavail_rec,
        )
        plan_rec2 = dict(plan_rec, run_id="run-rec-unavailable")
        res2 = await lab_unavail.run_plan(plan_rec2)
        self.assertEqual(res2.exit_category, EXIT_ACTIVATION)
        self.assertIn("recovery channel is unavailable", str(res2.failure))

        # 3. Serial required but unavailable -> EXIT_ACTIVATION
        unavail_ser = FakeSerialCapture(available=False)
        lab_ser_unavail = DriverLab(
            target,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
            serial_capture=unavail_ser,
        )
        plan_ser = {
            "schema_version": 1,
            "run_id": "run-ser-unavail",
            "case_id": "case-ser",
            "target": {
                "selector": "lab-target",
                "requires_serial": True,
            },
            "node": {"id": "node-1"},
            "access": {"mode": "proxy"},
            "operations": [
                {"kind": "mmio_read32", "resource": "ctrl", "offset": 16}
            ],
        }
        res3 = await lab_ser_unavail.run_plan(plan_ser)
        self.assertEqual(res3.exit_category, EXIT_ACTIVATION)
        self.assertIn(
            "serial capture channel is unavailable", str(res3.failure)
        )

    async def test_panic_detected_in_serial_causes_transport_failure(
        self,
    ) -> None:
        target = _make_target()
        serial = FakeSerialCapture(
            initial_lines=[
                "[00001.000] system running",
                "[00001.500] [klog] ZIRCON KERNEL PANIC: out of memory fault in driver",
            ]
        )
        lab = DriverLab(
            target,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
            serial_capture=serial,
        )

        plan = {
            "schema_version": 1,
            "run_id": "run-panic-test",
            "case_id": "case-panic",
            "target": {"selector": "lab-target"},
            "node": {"id": "node-1"},
            "access": {"mode": "proxy"},
            "operations": [
                {"kind": "mmio_read32", "resource": "ctrl", "offset": 16}
            ],
        }

        result = await lab.run_plan(plan)
        # Panic detected in serial should reflect in failure and metadata
        self.assertEqual(result.exit_category, EXIT_TRANSPORT)
        self.assertIn("kernel panic detected", str(result.failure))

        manifest = json.loads(
            (result.evidence_dir / "manifest.json").read_text()
        )
        self.assertTrue(manifest["serial_capture"]["panic_detected"])

    async def test_no_mutating_retry_on_transport_failure(self) -> None:
        """Spec Section 14.3: Transport loss never causes automatic replay.

        If a write may have completed, the result remains partial or unknown.
        """
        target = DisconnectingProxyTarget(_make_description())
        target.set_value(1, 16, 0x1234_5678)
        serial = FakeSerialCapture()
        recovery = FakeRecoveryController()

        lab = DriverLab(
            target,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
            serial_capture=serial,
            recovery=recovery,
        )

        plan = {
            "schema_version": 1,
            "run_id": "run-no-retry",
            "case_id": "case-no-retry",
            "target": {"selector": "lab-target"},
            "node": {"id": "node-1"},
            "access": {"mode": "proxy"},
            "operations": [
                {"kind": "mmio_read32", "resource": "ctrl", "offset": 16},
                {"kind": "mmio_read32", "resource": "ctrl", "offset": 16},
            ],
        }

        result = await lab.run_plan(plan)
        self.assertEqual(result.exit_category, EXIT_TRANSPORT)
        # Verified only 1 read succeeded; second was not replayed or retried
        self.assertEqual(len(result.reads), 1)

        # Verify operations.jsonl shows operation 0 succeeded, operation 1 failed
        ops_lines = (
            (result.evidence_dir / "operations.jsonl")
            .read_text()
            .strip()
            .splitlines()
        )
        self.assertEqual(len(ops_lines), 2)
        op0 = json.loads(ops_lines[0])
        op1 = json.loads(ops_lines[1])
        self.assertEqual(op0["operation"], 0)
        self.assertIn("value", op0)
        self.assertEqual(op1["operation"], 1)
        self.assertIn("error", op1)

    async def test_interpretation_generation(self) -> None:
        target = _make_target()
        serial = FakeSerialCapture(
            initial_lines=["[00001.000] healthy target running"]
        )
        lab = DriverLab(
            target,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
            serial_capture=serial,
        )

        plan = {
            "schema_version": 1,
            "run_id": "run-interpret-test",
            "case_id": "case-interpret",
            "target": {"selector": "lab-target"},
            "node": {"id": "node-1"},
            "access": {"mode": "proxy"},
            "operations": [
                {"kind": "mmio_read32", "resource": "ctrl", "offset": 16}
            ],
        }

        result = await lab.run_plan(plan)
        self.assertTrue(result.ok)

        report = result.interpret()
        self.assertEqual(report["run_id"], "run-interpret-test")
        self.assertEqual(report["exit_category"], EXIT_SUCCESS)
        self.assertTrue(len(report["findings"]) >= 1)

        finding = report["findings"][0]
        self.assertEqual(finding["status"], "verified")
        self.assertEqual(len(finding["citations"]), 1)
        citation = finding["citations"][0]
        self.assertEqual(citation["operation_index"], 0)
        self.assertEqual(citation["resource"], "ctrl")
        self.assertEqual(citation["offset"], 16)
        self.assertEqual(citation["raw_value"], 0x1234_5678)

    async def test_recovery_controller_operations(self) -> None:
        rec = FakeRecoveryController(available=True, alive=True)
        self.assertTrue(await rec.is_available())
        self.assertTrue(await rec.check_liveness())

        await rec.reboot(mode="normal")
        self.assertEqual(rec.reboot_count, 1)
        self.assertEqual(rec.history, ["reboot:normal"])

        await rec.power_cycle()
        self.assertEqual(rec.power_cycle_count, 1)
        self.assertEqual(rec.history, ["reboot:normal", "power_cycle"])

        target = _make_target()
        lab = DriverLab(
            target,
            grants_path=self.grants_path,
            evidence_root=self.evidence_root,
            target_scope="target-1",
            node_id="node-1",
            recovery=rec,
        )
        await lab.recover_target(mode="recovery")
        self.assertEqual(rec.reboot_count, 2)
        self.assertEqual(rec.history[-1], "reboot:recovery")


if __name__ == "__main__":
    unittest.main()

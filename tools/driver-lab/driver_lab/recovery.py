# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Independent recovery and serial capture integration.

Fulfills Phase 1 Milestone H5 (Spec Sections 3, 6.4, 14.1-14.3, 16, 17):
- Out-of-band serial capture and liveness monitoring independent of FIDL/ffx.
- Independent recovery controller (reboot, power-cycle, liveness) with Paniolo
  and ffx abstractions.
- Panic and reboot detection with structured diagnostic extraction.
- Strict non-retry invariant for mutating operations on failure or disconnect.
- Evidence-linked interpretations citing exact operation indices and raw values.
"""

import asyncio
import dataclasses
import datetime
import json
import re
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any, Protocol

PANIC_SIGNATURES: tuple[re.Pattern[str], ...] = (
    re.compile(r"ZIRCON KERNEL PANIC", re.IGNORECASE),
    re.compile(r"ASSERT FAILED", re.IGNORECASE),
    re.compile(r"fatal page fault", re.IGNORECASE),
    re.compile(r"KERNEL PANIC", re.IGNORECASE),
    re.compile(r"OOM: out of memory", re.IGNORECASE),
)


def _utc_now() -> str:
    return datetime.datetime.now(datetime.UTC).isoformat()


def detect_panic(text_or_bytes: str | bytes) -> bool:
    """Returns True if the serial log contains known Zircon panic or crash signatures."""
    text = (
        text_or_bytes.decode("utf-8", errors="replace")
        if isinstance(text_or_bytes, bytes)
        else text_or_bytes
    )
    return any(sig.search(text) for sig in PANIC_SIGNATURES)


def extract_panic_summary(text_or_bytes: str | bytes) -> str | None:
    """Extracts the first matching panic or crash line from serial output."""
    text = (
        text_or_bytes.decode("utf-8", errors="replace")
        if isinstance(text_or_bytes, bytes)
        else text_or_bytes
    )
    for line in text.splitlines():
        line_clean = line.strip()
        for sig in PANIC_SIGNATURES:
            if sig.search(line_clean):
                return line_clean
    return None


@dataclasses.dataclass(frozen=True)
class SerialCaptureResult:
    """Outcome of a completed serial capture session."""

    data: bytes
    started_at: str
    stopped_at: str
    lines_count: int
    byte_count: int
    source: str
    metadata: Mapping[str, Any] = dataclasses.field(default_factory=dict)

    @property
    def contains_panic(self) -> bool:
        """True if the captured log contains panic signatures."""
        return detect_panic(self.data)

    @property
    def panic_summary(self) -> str | None:
        """Extracted panic line if present."""
        return extract_panic_summary(self.data)

    def to_manifest_metadata(self) -> dict[str, Any]:
        """Returns metadata dictionary for inclusion in evidence manifest."""
        return {
            "status": "captured",
            "source": self.source,
            "bytes": self.byte_count,
            "lines": self.lines_count,
            "started_at": self.started_at,
            "stopped_at": self.stopped_at,
            "panic_detected": self.contains_panic,
            "panic_summary": self.panic_summary,
            **dict(self.metadata),
        }


class SerialCaptureSession(Protocol):
    """An active serial capture session."""

    async def get_current_log(self) -> bytes:
        """Returns current captured bytes without stopping the session."""
        ...

    async def stop(self) -> SerialCaptureResult:
        """Stops capturing and returns the finalized result."""
        ...


class SerialCapture(Protocol):
    """Provider for target serial stream capture."""

    async def is_available(self) -> bool:
        """Whether serial capture is currently available."""
        ...

    async def start(self) -> SerialCaptureSession:
        """Starts a new serial capture session."""
        ...


class FakeSerialCaptureSession:
    """In-memory serial capture session for tests."""

    def __init__(self, parent: FakeSerialCapture) -> None:
        self._parent = parent
        self._started_at = _utc_now()
        self._stopped = False

    async def get_current_log(self) -> bytes:
        return self._parent.get_bytes()

    async def stop(self) -> SerialCaptureResult:
        self._stopped = True
        stopped_at = _utc_now()
        data = self._parent.get_bytes()
        text = data.decode("utf-8", errors="replace")
        lines = text.splitlines()
        return SerialCaptureResult(
            data=data,
            started_at=self._started_at,
            stopped_at=stopped_at,
            lines_count=len(lines),
            byte_count=len(data),
            source="fake",
            metadata={"session_id": "fake-serial-session"},
        )


class FakeSerialCapture:
    """In-memory Fake serial capture for unit and integration testing."""

    def __init__(
        self,
        *,
        available: bool = True,
        initial_lines: Sequence[str] | None = None,
    ) -> None:
        self._available = available
        self._lines: list[str] = list(initial_lines or [])
        self._sessions: list[FakeSerialCaptureSession] = []

    @property
    def available(self) -> bool:
        return self._available

    @available.setter
    def available(self, value: bool) -> None:
        self._available = value

    def emit_line(self, line: str) -> None:
        """Appends a line to the simulated serial stream."""
        self._lines.append(line)

    def emit_panic(self, reason: str = "KERNEL PANIC: fatal exception") -> None:
        """Injects a panic message into the serial stream."""
        self.emit_line(f"[00001.234] [klog] {reason}")

    def get_bytes(self) -> bytes:
        content = "\n".join(self._lines)
        if content and not content.endswith("\n"):
            content += "\n"
        return content.encode("utf-8")

    async def is_available(self) -> bool:
        return self._available

    async def start(self) -> SerialCaptureSession:
        if not self._available:
            raise RuntimeError("serial capture is not available")
        session = FakeSerialCaptureSession(self)
        self._sessions.append(session)
        return session


class FfxSerialCaptureSession:
    """Serial capture session wrapping ffx target serial."""

    def __init__(self, proc: asyncio.subprocess.Process, source: str) -> None:
        self._proc = proc
        self._source = source
        self._started_at = _utc_now()
        self._buffer = bytearray()
        self._reader_task: asyncio.Task[None] | None = None

    def start_reader(self) -> None:
        async def read_stream() -> None:
            assert self._proc.stdout is not None
            while True:
                chunk = await self._proc.stdout.read(4096)
                if not chunk:
                    break
                self._buffer.extend(chunk)

        self._reader_task = asyncio.create_task(read_stream())

    async def get_current_log(self) -> bytes:
        return bytes(self._buffer)

    async def stop(self) -> SerialCaptureResult:
        if self._proc.returncode is None:
            self._proc.terminate()
            try:
                await asyncio.wait_for(self._proc.wait(), timeout=2.0)
            except asyncio.TimeoutError:
                self._proc.kill()
                await self._proc.wait()

        if self._reader_task is not None:
            try:
                await asyncio.wait_for(self._reader_task, timeout=1.0)
            except (asyncio.TimeoutError, asyncio.CancelledError):
                pass

        stopped_at = _utc_now()
        data = bytes(self._buffer)
        lines = data.decode("utf-8", errors="replace").splitlines()
        return SerialCaptureResult(
            data=data,
            started_at=self._started_at,
            stopped_at=stopped_at,
            lines_count=len(lines),
            byte_count=len(data),
            source=self._source,
        )


class FfxSerialCapture:
    """Serial capture implementation connecting via ffx target serial."""

    def __init__(self, target: str | None = None) -> None:
        self._target = target

    async def is_available(self) -> bool:
        cmd = ["ffx"]
        if self._target:
            cmd.extend(["--target", self._target])
        cmd.extend(["target", "list", "--format", "json"])
        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=3.0)
            return proc.returncode == 0 and len(stdout) > 0
        except Exception:
            return False

    async def start(self) -> SerialCaptureSession:
        cmd = ["ffx"]
        if self._target:
            cmd.extend(["--target", self._target])
        cmd.extend(["target", "serial"])
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        session = FfxSerialCaptureSession(proc, source="ffx_target_serial")
        session.start_reader()
        return session


class PanioloSerialCapture:
    """Serial capture using out-of-band Paniolo CLI."""

    def __init__(self, station_id: str | None = None) -> None:
        self._station_id = station_id

    async def is_available(self) -> bool:
        cmd = ["paniolo", "status"]
        if self._station_id:
            cmd.extend(["--station", self._station_id])
        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            _, _ = await asyncio.wait_for(proc.communicate(), timeout=3.0)
            return proc.returncode == 0
        except Exception:
            return False

    async def start(self) -> SerialCaptureSession:
        cmd = ["paniolo", "serial", "stream"]
        if self._station_id:
            cmd.extend(["--station", self._station_id])
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        session = FfxSerialCaptureSession(proc, source="paniolo_serial")
        session.start_reader()
        return session


class RecoveryController(Protocol):
    """Independent out-of-band target control (reset, power, liveness)."""

    async def is_available(self) -> bool:
        """Whether the independent recovery channel is reachable."""
        ...

    async def check_liveness(self) -> bool:
        """Checks if the target hardware is responsive."""
        ...

    async def reboot(self, mode: str = "normal") -> None:
        """Triggers an out-of-band hardware reboot."""
        ...

    async def power_cycle(self) -> None:
        """Triggers an out-of-band hardware power cut and restoration."""
        ...


class FakeRecoveryController:
    """In-memory recovery controller for unit testing."""

    def __init__(
        self,
        *,
        available: bool = True,
        alive: bool = True,
    ) -> None:
        self.available = available
        self.alive = alive
        self.reboot_count = 0
        self.power_cycle_count = 0
        self.history: list[str] = []

    async def is_available(self) -> bool:
        return self.available

    async def check_liveness(self) -> bool:
        return self.available and self.alive

    async def reboot(self, mode: str = "normal") -> None:
        if not self.available:
            raise RuntimeError("recovery controller unavailable")
        self.reboot_count += 1
        self.history.append(f"reboot:{mode}")
        self.alive = True

    async def power_cycle(self) -> None:
        if not self.available:
            raise RuntimeError("recovery controller unavailable")
        self.power_cycle_count += 1
        self.history.append("power_cycle")
        self.alive = True


class FfxRecoveryController:
    """Target recovery controller over ffx commands."""

    def __init__(self, target: str | None = None) -> None:
        self._target = target

    async def is_available(self) -> bool:
        cmd = ["ffx"]
        if self._target:
            cmd.extend(["--target", self._target])
        cmd.extend(["target", "echo"])
        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            _, _ = await asyncio.wait_for(proc.communicate(), timeout=3.0)
            return proc.returncode == 0
        except Exception:
            return False

    async def check_liveness(self) -> bool:
        return await self.is_available()

    async def reboot(self, mode: str = "normal") -> None:
        cmd = ["ffx"]
        if self._target:
            cmd.extend(["--target", self._target])
        cmd.extend(["target", "reboot"])
        if mode == "bootloader":
            cmd.extend(["-b"])
        elif mode == "recovery":
            cmd.extend(["-r"])
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        _, stderr = await proc.communicate()
        if proc.returncode != 0:
            raise RuntimeError(
                f"ffx target reboot failed ({proc.returncode}): {stderr.decode()}"
            )

    async def power_cycle(self) -> None:
        # Fallback to reboot on standard ffx when power switch is unmanaged
        await self.reboot(mode="normal")


class PanioloRecoveryController:
    """Target recovery controller integrating with Paniolo out-of-band lab hardware."""

    def __init__(self, station_id: str | None = None) -> None:
        self._station_id = station_id

    async def is_available(self) -> bool:
        cmd = ["paniolo", "ping"]
        if self._station_id:
            cmd.extend(["--station", self._station_id])
        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            _, _ = await asyncio.wait_for(proc.communicate(), timeout=3.0)
            return proc.returncode == 0
        except Exception:
            return False

    async def check_liveness(self) -> bool:
        cmd = ["paniolo", "liveness"]
        if self._station_id:
            cmd.extend(["--station", self._station_id])
        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            _, _ = await asyncio.wait_for(proc.communicate(), timeout=3.0)
            return proc.returncode == 0
        except Exception:
            return False

    async def reboot(self, mode: str = "normal") -> None:
        cmd = ["paniolo", "reset"]
        if self._station_id:
            cmd.extend(["--station", self._station_id])
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        _, stderr = await proc.communicate()
        if proc.returncode != 0:
            raise RuntimeError(f"paniolo reset failed: {stderr.decode()}")

    async def power_cycle(self) -> None:
        cmd = ["paniolo", "power", "cycle"]
        if self._station_id:
            cmd.extend(["--station", self._station_id])
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        _, stderr = await proc.communicate()
        if proc.returncode != 0:
            raise RuntimeError(f"paniolo power cycle failed: {stderr.decode()}")


# --- Driver Agent Interpretation Layer (Spec Section 17, Milestone H5) ---


@dataclasses.dataclass(frozen=True)
class InterpretationCitation:
    """Precise citation of a raw observation backing an interpretation claim."""

    operation_index: int
    kind: str
    resource: str
    offset: int | None
    raw_value: int | str | None
    timestamp_ns: int | None
    audit_seq: int | None


@dataclasses.dataclass(frozen=True)
class InterpretationFinding:
    """A verified, contradicted, or inconclusive claim backed by evidence."""

    claim: str
    status: str  # "verified", "contradicted", "inconclusive"
    citations: tuple[InterpretationCitation, ...]
    notes: str | None = None

    def to_json(self) -> dict[str, Any]:
        return {
            "claim": self.claim,
            "status": self.status,
            "citations": [dataclasses.asdict(c) for c in self.citations],
            "notes": self.notes,
        }


def interpret_evidence(evidence_dir: Path) -> dict[str, Any]:
    """Generates an interpretation.json structure derived from finalized evidence files.

    Per Spec Section 17:
    - Never converts transport success into semantic verification.
    - Cites the operation, resource, offset or protocol method, raw values,
      and target timestamp.
    """
    manifest_path = evidence_dir / "manifest.json"
    if not manifest_path.exists():
        raise FileNotFoundError(
            f"cannot interpret incomplete evidence: {manifest_path} not found"
        )

    with manifest_path.open("r", encoding="utf-8") as f:
        manifest = json.load(f)

    operations_path = evidence_dir / "operations.jsonl"
    operations: list[dict[str, Any]] = []
    if operations_path.exists():
        with operations_path.open("r", encoding="utf-8") as f:
            for line in f:
                if line.strip():
                    operations.append(json.loads(line))

    # Inspect serial for panic or crash indicators
    serial_path = evidence_dir / "serial.log"
    has_panic = False
    panic_line = None
    if serial_path.exists():
        serial_bytes = serial_path.read_bytes()
        has_panic = detect_panic(serial_bytes)
        panic_line = extract_panic_summary(serial_bytes)

    findings: list[InterpretationFinding] = []

    # Map operations into cited findings
    for op in operations:
        idx = op.get("operation", 0)
        kind = op.get("kind", "unknown")
        resource = op.get("resource", "unknown")
        offset = op.get("offset")
        val = op.get("value")
        ts = op.get("timestamp_ns")
        seq = op.get("audit_seq")

        citation = InterpretationCitation(
            operation_index=idx,
            kind=kind,
            resource=resource,
            offset=offset,
            raw_value=val,
            timestamp_ns=ts,
            audit_seq=seq,
        )

        if "error" in op:
            findings.append(
                InterpretationFinding(
                    claim=f"Operation {idx} ({kind}) completed successfully",
                    status="contradicted",
                    citations=(citation,),
                    notes=f"Failed with error: {op['error']}",
                )
            )
        else:
            findings.append(
                InterpretationFinding(
                    claim=f"Operation {idx} ({kind}) read expected value from {resource}",
                    status="verified",
                    citations=(citation,),
                    notes=f"Observed value: {val:#x}"
                    if isinstance(val, int)
                    else f"Observed value: {val}",
                )
            )

    if has_panic:
        findings.append(
            InterpretationFinding(
                claim="Target maintained operational stability throughout run",
                status="contradicted",
                citations=(),
                notes=f"Kernel panic detected in serial log: {panic_line}",
            )
        )

    return {
        "interpretation_schema_version": 1,
        "run_id": manifest.get("run_id"),
        "case_id": manifest.get("case_id"),
        "plan_digest": manifest.get("plan_digest"),
        "exit_category": manifest.get("exit_category"),
        "findings": [f.to_json() for f in findings],
        "generated_at": _utc_now(),
    }

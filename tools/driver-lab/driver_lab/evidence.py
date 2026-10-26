# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Evidence recording: per-run directories, atomic hashed artifacts, and a
manifest written last.

A run's evidence directory is never reused. Every artifact is written to a
temporary file, flushed, atomically renamed, and hashed. The manifest is
written only at finalization, so its presence marks a complete bundle; a
run whose evidence cannot be finalized is not successful.
"""

import datetime
import enum
import hashlib
import json
import os
import re
from collections.abc import Iterable, Mapping
from pathlib import Path

EVIDENCE_SCHEMA_VERSION = 1

_NAME_PATTERN = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")

NOT_APPLICABLE = "not_applicable"


class EvidenceError(Exception):
    """Evidence could not be recorded or finalized."""


def _utc_now() -> str:
    return datetime.datetime.now(datetime.UTC).isoformat()


def _json_default(obj: object) -> object:
    if isinstance(obj, enum.Enum):
        return obj.value
    raise TypeError(
        f"Object of type {type(obj).__name__} is not JSON serializable"
    )


class EvidenceRecorder:
    """Collects one run's evidence artifacts and finalizes the manifest."""

    def __init__(self, root: Path, run_id: str) -> None:
        if not _NAME_PATTERN.fullmatch(run_id) or ".." in run_id:
            raise EvidenceError(f"invalid run id: {run_id!r}")
        self.directory = root / run_id
        try:
            self.directory.mkdir(parents=True, exist_ok=False)
        except FileExistsError as error:
            raise EvidenceError(
                f"evidence directory already exists (no reuse): {self.directory}"
            ) from error
        self._files: dict[str, object] = {}
        self._finalized = False
        self._started_at = _utc_now()

    def _check_name(self, name: str) -> None:
        if self._finalized:
            raise EvidenceError("evidence is finalized; no further writes")
        if not _NAME_PATTERN.fullmatch(name) or ".." in name:
            raise EvidenceError(f"invalid artifact name: {name!r}")
        if name == "manifest.json":
            raise EvidenceError("manifest.json is written only by finalize()")
        if name in self._files:
            raise EvidenceError(f"artifact already recorded: {name}")

    def _write_atomic(self, name: str, data: bytes) -> None:
        temp_path = self.directory / (name + ".tmp")
        fd = os.open(temp_path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        try:
            os.write(fd, data)
            os.fsync(fd)
        finally:
            os.close(fd)
        os.replace(temp_path, self.directory / name)

    def write_bytes(self, name: str, data: bytes) -> None:
        """Atomically writes one artifact and records its hash and size."""
        self._check_name(name)
        self._write_atomic(name, data)
        self._files[name] = {
            "sha256": hashlib.sha256(data).hexdigest(),
            "bytes": len(data),
        }

    def write_json(self, name: str, payload: object) -> None:
        """Writes one artifact as canonical, sorted, indented JSON."""
        text = (
            json.dumps(payload, sort_keys=True, indent=2, default=_json_default)
            + "\n"
        )
        self.write_bytes(name, text.encode())

    def write_jsonl(
        self, name: str, rows: Iterable[Mapping[str, object]]
    ) -> None:
        """Writes one artifact as JSON lines, one row per line."""
        lines = "".join(
            json.dumps(
                row,
                sort_keys=True,
                separators=(",", ":"),
                default=_json_default,
            )
            + "\n"
            for row in rows
        )
        self.write_bytes(name, lines.encode())

    def mark_not_applicable(self, name: str) -> None:
        """Marks an artifact as not applicable rather than fabricating it."""
        self._check_name(name)
        self._files[name] = NOT_APPLICABLE

    def recorded(self, name: str) -> bool:
        """Whether `name` was written or marked not applicable."""
        return name in self._files

    def finalize(
        self,
        exit_category: int,
        manifest_extra: Mapping[str, object] | None = None,
    ) -> Path:
        """Writes the manifest last and seals the recorder."""
        if self._finalized:
            raise EvidenceError("evidence is already finalized")
        manifest: dict[str, object] = {
            "evidence_schema_version": EVIDENCE_SCHEMA_VERSION,
            "started_at": self._started_at,
            "finalized_at": _utc_now(),
            "exit_category": exit_category,
            "files": self._files,
        }
        if manifest_extra:
            overlap = set(manifest_extra) & set(manifest)
            if overlap:
                raise EvidenceError(
                    f"manifest extras collide with core fields: {sorted(overlap)}"
                )
            manifest.update(manifest_extra)
        data = (json.dumps(manifest, sort_keys=True, indent=2) + "\n").encode()
        self._write_atomic("manifest.json", data)
        self._finalized = True
        return self.directory / "manifest.json"

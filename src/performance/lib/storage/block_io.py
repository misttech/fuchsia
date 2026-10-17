# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Extracts and publishes cumulative block/storage I/O rate metrics from Inspect."""

import json
import logging
import os
import pathlib
import zipfile
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Any, Protocol

from perf_publish import publish
from reporting import metrics

_LOGGER: logging.Logger = logging.getLogger(__name__)

# Maps block driver Inspect tree name (metadata.name) to root child node name.
_BLOCK_DRIVER_NODES: dict[str, str] = {
    "ftl": "ftl",
    "sdmmc": "sdmmc_core",
    "ufs": "ufs",
    "virtio-block": "virtio_block",
}

_BLOCK_OPERATIONS: tuple[tuple[str, str], ...] = (
    ("read", "Read"),
    ("write", "Write"),
    ("flush", "Flush"),
    ("trim", "Trim"),
)

STORAGE_INSPECT_SELECTORS: list[str] = [
    "bootstrap/*-drivers*:[name=ftl]root",
    "bootstrap/*-drivers*:[name=sdmmc]root",
    "bootstrap/*-drivers*:[name=ufs]root",
    "bootstrap/*-drivers*:[name=virtio-block]root",
]


class InspectDataProvider(Protocol):
    """Protocol for a Honeydew device object that can query Inspect data."""

    def get_inspect_data(
        self,
        *,
        selectors: list[str] | None = None,
    ) -> Any:
        """Returns an InspectDataCollection for the requested selectors."""


@dataclass(frozen=True)
class InspectEntry:
    """Normalized Inspect entry from a snapshot zip or Honeydew query."""

    moniker: str | None
    name: str | None
    timestamp_ns: int
    root: Mapping[str, Any]

    @classmethod
    def from_dict(cls, entry: Mapping[str, Any]) -> "InspectEntry | None":
        """Parses a raw JSON Inspect entry dictionary into an `InspectEntry`."""
        metadata = entry.get("metadata")
        if not isinstance(metadata, Mapping):
            return None
        payload = entry.get("payload")
        if not isinstance(payload, Mapping):
            return None
        root = payload.get("root")
        if not isinstance(root, Mapping):
            return None
        timestamp = metadata.get("timestamp", 0)
        name = metadata.get("name")
        moniker = entry.get("moniker")
        return cls(
            moniker=str(moniker) if moniker is not None else None,
            name=str(name) if name is not None else None,
            timestamp_ns=int(timestamp or 0),
            root=root,
        )


def load_inspect_entries(
    dut: InspectDataProvider | None = None,
    snapshot_dirs: Sequence[str | os.PathLike[str]] = (),
) -> list[InspectEntry]:
    """Loads Inspect entries from the newest `Snapshot_*.zip` in `snapshot_dirs`,
    falling back to `dut.get_inspect_data()` if no snapshot zip is found.

    Checking multiple directories (e.g. `[self._teardown_class_artifacts, self.log_path]`)
    supports base test classes that save teardown snapshots in either
    `<log_path>/teardown_class` or `<log_path>`.
    """
    for directory in snapshot_dirs:
        if not directory:
            continue
        dir_path = pathlib.Path(directory)
        if not dir_path.exists():
            continue
        snapshot_zips = sorted(dir_path.glob("Snapshot_*.zip"))
        for snapshot_zip in reversed(snapshot_zips):
            try:
                with zipfile.ZipFile(snapshot_zip, "r") as zf:
                    if "inspect.json" in zf.namelist():
                        data = json.loads(zf.read("inspect.json"))
                        if isinstance(data, list):
                            entries = [
                                parsed
                                for item in data
                                if isinstance(item, Mapping)
                                and (parsed := InspectEntry.from_dict(item))
                                is not None
                            ]
                            if entries:
                                return entries
            except Exception as e:
                _LOGGER.warning(
                    "Failed to read inspect.json from %s: %s", snapshot_zip, e
                )

    if dut is None:
        return []

    try:
        collection = dut.get_inspect_data(selectors=STORAGE_INSPECT_SELECTORS)
        entries = []
        for item in collection.data:
            payload = item.payload
            root = payload.get("root") if isinstance(payload, Mapping) else None
            if not isinstance(root, Mapping):
                continue
            entries.append(
                InspectEntry(
                    moniker=getattr(item, "moniker", None),
                    name=getattr(item.metadata, "name", None),
                    timestamp_ns=int(item.metadata.timestamp.nanoseconds()),
                    root=root,
                )
            )
        return entries
    except Exception as e:
        _LOGGER.warning("Failed to query fallback storage inspect data: %s", e)
        return []


def _extract_hardware_block_rates(
    entry: InspectEntry,
) -> dict[str, tuple[float, float]] | None:
    """Attempts to extract `{op_label: (bytes_per_sec, ops_per_sec)}` for Read,
    Write, Flush, and Trim from a hardware block driver `InspectEntry` (`ftl`,
    `sdmmc_core`, `ufs`, or `virtio_block`).
    """
    if entry.timestamp_ns <= 0 or entry.name not in _BLOCK_DRIVER_NODES:
        return None
    uptime_sec = float(entry.timestamp_ns) / 1e9
    node_name = _BLOCK_DRIVER_NODES[entry.name]
    node = entry.root.get(node_name)
    if not isinstance(node, Mapping):
        return None
    if all(
        f"block.{op_key}.bytes" in node and f"block.{op_key}.count" in node
        for op_key, _ in _BLOCK_OPERATIONS
    ):
        return {
            op_label: (
                int(node[f"block.{op_key}.bytes"]) / uptime_sec,
                int(node[f"block.{op_key}.count"]) / uptime_sec,
            )
            for op_key, op_label in _BLOCK_OPERATIONS
        }

    return None


def _normalize_entries(
    inspect_entries: Sequence[InspectEntry | Mapping[str, Any]],
) -> list[InspectEntry]:
    """Converts a sequence of `InspectEntry` or raw dict entries into a
    deterministically ordered list of `InspectEntry` objects.
    """
    normalized: list[InspectEntry] = []
    for entry in inspect_entries:
        if isinstance(entry, InspectEntry):
            normalized.append(entry)
        elif isinstance(entry, Mapping):
            parsed = InspectEntry.from_dict(entry)
            if parsed is not None:
                normalized.append(parsed)
    return sorted(
        normalized,
        key=lambda e: (e.name or "", e.moniker or "", e.timestamp_ns),
    )


def extract_storage_io_metrics(
    inspect_entries: Sequence[InspectEntry | Mapping[str, Any]],
    label_prefix: str | None = None,
) -> list[metrics.TestCaseResult]:
    """Extracts cumulative block/storage I/O rate metrics (`Block{Read,Write,Flush,Trim}{Bytes,Ops}PerSec`)
    from hardware block driver Inspect entries (`ftl`, `sdmmc_core`, `ufs`,
    `virtio_block`).

    Target devices have a single primary block storage driver. To guarantee
    deterministic behavior independent of the input order in `inspect_entries`,
    entries are sorted by `(name, moniker, timestamp_ns)` and any active block
    driver with non-zero I/O operations takes precedence over idle instances.
    """
    rates: dict[str, tuple[float, float]] | None = None

    # Target devices only have one primary block storage driver, but entries in
    # `inspect.json` are not guaranteed to be ordered. Sort entries first, and
    # prefer an active driver (non-zero total I/O ops) over an idle one.
    for entry in _normalize_entries(inspect_entries):
        candidate = _extract_hardware_block_rates(entry)
        if candidate is None:
            continue
        if rates is None:
            rates = candidate
        if sum(ops for _, ops in candidate.values()) > 0:
            rates = candidate
            break

    if rates is None:
        return []

    prefix = f"{label_prefix}/" if label_prefix else ""
    results: list[metrics.TestCaseResult] = []
    for op_key, op_label in _BLOCK_OPERATIONS:
        bytes_per_sec, ops_per_sec = rates[op_label]
        results.extend(
            [
                metrics.TestCaseResult(
                    label=f"{prefix}Block{op_label}BytesPerSec",
                    unit=metrics.Unit.bytesPerSecond,
                    values=[bytes_per_sec],
                    doc=f"Cumulative block {op_key} bytes per second of device uptime.",
                    direction=metrics.Direction.smallerIsBetter,
                ),
                metrics.TestCaseResult(
                    label=f"{prefix}Block{op_label}OpsPerSec",
                    unit=metrics.Unit.count,
                    values=[ops_per_sec],
                    doc=f"Cumulative block {op_key} operations per second of device uptime.",
                    direction=metrics.Direction.smallerIsBetter,
                ),
            ]
        )
    return results


def capture(
    dut: InspectDataProvider | None = None,
    snapshot_dirs: Sequence[str | os.PathLike[str]] = (),
    label_prefix: str | None = None,
    inspect_entries: Sequence[InspectEntry | Mapping[str, Any]] | None = None,
) -> metrics.Report:
    """Captures cumulative block/storage I/O rate metrics as a `metrics.Report`.

    Can be combined with trace or memory metrics via `report.extend(storage.capture(dut=self.dut))`.
    """
    entries = (
        inspect_entries
        if inspect_entries is not None
        else load_inspect_entries(dut=dut, snapshot_dirs=snapshot_dirs)
    )
    structured = extract_storage_io_metrics(entries, label_prefix=label_prefix)
    description = metrics.describe_callable(capture)
    return metrics.Report(
        structured,
        {},
        [
            metrics.MetricsProcessorDescription(
                doc=description["doc"],
                code_path=description["code_path"],
                line_no=description["line_no"],
                metrics=[tcr.describe() for tcr in structured],
            )
        ],
    )


def publish_teardown_storage_io_metrics(
    test_instance: Any,
    test_suite: str,
    expected_metric_names_filename: str | None = None,
    label_prefix: str | None = None,
    runtime_deps_dir: str | os.PathLike[str] | None = None,
    test_data_module: Any | None = None,
) -> list[metrics.TestCaseResult]:
    """One-line helper for `FuchsiaBaseTest` subclasses' `teardown_class()` that
    loads the teardown snapshot, extracts storage I/O rates, writes
    `<teardown_class_artifacts>/<test_suite>-benchmark.fuchsiaperf.json`, and
    publishes it via `publish.publish_fuchsiaperf()`.
    """
    try:
        snapshot_dirs: list[str] = []
        for attr in ("_teardown_class_artifacts", "log_path"):
            val = getattr(test_instance, attr, None)
            if val:
                snapshot_dirs.append(str(val))

        inspect_entries = load_inspect_entries(
            dut=getattr(test_instance, "dut", None),
            snapshot_dirs=snapshot_dirs,
        )
        resolved_prefix = (
            test_instance.__class__.__name__
            if label_prefix is None
            else label_prefix
        )
        storage_metrics = extract_storage_io_metrics(
            inspect_entries, label_prefix=resolved_prefix
        )
        if not storage_metrics:
            _LOGGER.warning(
                "No block/storage I/O inspect metrics found in teardown snapshot."
            )
            return []

        output_dir = getattr(
            test_instance, "_teardown_class_artifacts", None
        ) or getattr(test_instance, "log_path")
        os.makedirs(output_dir, exist_ok=True)
        fuchsiaperf_json_path = (
            pathlib.Path(output_dir)
            / f"{test_suite}-benchmark.fuchsiaperf.json"
        )
        metrics.TestCaseResult.write_fuchsiaperf_json(
            results=storage_metrics,
            test_suite=test_suite,
            output_path=fuchsiaperf_json_path,
        )

        publish_kwargs: dict[str, Any] = {
            "fuchsia_perf_file_paths": [fuchsiaperf_json_path],
            "expected_metric_names_filename": (
                expected_metric_names_filename or f"{test_suite}.txt"
            ),
        }
        if runtime_deps_dir is not None:
            publish_kwargs["runtime_deps_dir"] = runtime_deps_dir
        if test_data_module is not None:
            publish_kwargs["test_data_module"] = test_data_module

        publish.publish_fuchsiaperf(**publish_kwargs)
        return storage_metrics
    except Exception as e:
        _LOGGER.warning("Failed to publish storage I/O metrics: %s", e)
        return []

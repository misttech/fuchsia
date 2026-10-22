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

    Logs a warning and returns `[]` on any snapshot read or Inspect query error
    rather than raising.
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
            name = getattr(item.metadata, "name", None)
            if name is None:
                for driver_name, node_name in _BLOCK_DRIVER_NODES.items():
                    if isinstance(root.get(node_name), Mapping):
                        name = driver_name
                        break
            entries.append(
                InspectEntry(
                    moniker=getattr(item, "moniker", None),
                    name=name,
                    timestamp_ns=int(item.metadata.timestamp.nanoseconds()),
                    root=root,
                )
            )
        return entries
    except Exception as e:
        _LOGGER.warning("Failed to query fallback storage inspect data: %s", e)
        return []


def _resolve_driver_and_node(
    entry: InspectEntry,
) -> tuple[str, str, Mapping[str, Any]] | None:
    """Resolves `(driver_name, node_name, node_dict)` from an `InspectEntry`.

    Entries loaded from `Snapshot_*.zip` have `entry.name` set to the Inspect
    tree name (e.g. `"ftl"`), whereas entries constructed from `fuchsia_inspect`
    without `metadata.name` may have `entry.name=None`. In both cases,
    `entry.root` contains the driver's unique child node (`"ftl"`,
    `"sdmmc_core"`, `"ufs"`, or `"virtio_block"`).
    """
    if entry.timestamp_ns <= 0:
        return None
    if entry.name in _BLOCK_DRIVER_NODES:
        node_name = _BLOCK_DRIVER_NODES[entry.name]
        node = entry.root.get(node_name)
        if isinstance(node, Mapping):
            return entry.name, node_name, node
    for driver_name, node_name in _BLOCK_DRIVER_NODES.items():
        node = entry.root.get(node_name)
        if isinstance(node, Mapping):
            return driver_name, node_name, node
    return None


def _has_block_op_counters(node: Mapping[str, Any]) -> bool:
    """Returns True if `node` has `.bytes` and `.count` counters for all block ops."""
    return all(
        f"block.{op_key}.bytes" in node and f"block.{op_key}.count" in node
        for op_key, _ in _BLOCK_OPERATIONS
    )


def _extract_hardware_block_rates(
    entry: InspectEntry,
) -> dict[str, tuple[float, float]] | None:
    """Attempts to extract `{op_label: (bytes_per_sec, ops_per_sec)}` for Read,
    Write, Flush, and Trim from a hardware block driver `InspectEntry` (`ftl`,
    `sdmmc_core`, `ufs`, or `virtio_block`).
    """
    resolved = _resolve_driver_and_node(entry)
    if resolved is None:
        return None
    _, _, node = resolved
    if not _has_block_op_counters(node):
        return None
    uptime_sec = float(entry.timestamp_ns) / 1e9
    return {
        op_label: (
            int(node[f"block.{op_key}.bytes"]) / uptime_sec,
            int(node[f"block.{op_key}.count"]) / uptime_sec,
        )
        for op_key, op_label in _BLOCK_OPERATIONS
    }


def _normalize_entries(
    inspect_entries: Sequence[InspectEntry | Mapping[str, Any]],
) -> list[InspectEntry]:
    """Converts a sequence of `InspectEntry` or raw dict entries into a
    deterministically ordered list of `InspectEntry` objects.
    """
    normalized: list[InspectEntry] = []
    for entry in inspect_entries:
        parsed: InspectEntry | None = None
        if isinstance(entry, InspectEntry):
            parsed = entry
        elif isinstance(entry, Mapping):
            parsed = InspectEntry.from_dict(entry)
        if parsed is None:
            continue
        if parsed.name is None:
            resolved = _resolve_driver_and_node(parsed)
            if resolved is not None:
                parsed = InspectEntry(
                    moniker=parsed.moniker,
                    name=resolved[0],
                    timestamp_ns=parsed.timestamp_ns,
                    root=parsed.root,
                )
        normalized.append(parsed)
    return sorted(
        normalized,
        key=lambda e: (e.name or "", e.moniker or "", e.timestamp_ns),
    )


def diff_inspect_entries(
    start_entries: Sequence[InspectEntry | Mapping[str, Any]],
    end_entries: Sequence[InspectEntry | Mapping[str, Any]],
) -> list[InspectEntry]:
    """Computes per-interval delta `InspectEntry` items between `start_entries` and `end_entries`.

    For each hardware block driver entry in `end_entries`, subtracts the baseline
    cumulative bytes/ops from `start_entries` and sets `timestamp_ns` to
    `end.timestamp_ns - start.timestamp_ns`. Prefers active storage drivers
    (non-zero cumulative operations since boot) over idle secondary block
    interfaces (such as SDIO Wi-Fi controllers) so zero-I/O test windows still
    produce `0.0` rate metrics for the active storage device.
    """
    valid_end: list[tuple[InspectEntry, str, Mapping[str, Any], int]] = []
    for entry in _normalize_entries(end_entries):
        resolved = _resolve_driver_and_node(entry)
        if resolved is None:
            continue
        driver_name, node_name, node = resolved
        if not _has_block_op_counters(node):
            continue
        total_ops = sum(
            int(node[f"block.{op_key}.count"])
            for op_key, _ in _BLOCK_OPERATIONS
        )
        normalized_end = InspectEntry(
            moniker=entry.moniker,
            name=driver_name,
            timestamp_ns=entry.timestamp_ns,
            root=entry.root,
        )
        valid_end.append((normalized_end, node_name, node, total_ops))

    if any(total_ops > 0 for _, _, _, total_ops in valid_end):
        valid_end = [item for item in valid_end if item[3] > 0]

    start_by_key: dict[tuple[str | None, str | None], InspectEntry] = {}
    start_by_name: dict[str | None, InspectEntry] = {}
    for s_entry in _normalize_entries(start_entries):
        resolved = _resolve_driver_and_node(s_entry)
        if resolved is None:
            continue
        s_driver_name, _, s_node = resolved
        if not _has_block_op_counters(s_node):
            continue
        normalized_start = InspectEntry(
            moniker=s_entry.moniker,
            name=s_driver_name,
            timestamp_ns=s_entry.timestamp_ns,
            root=s_entry.root,
        )
        start_by_key[(s_driver_name, s_entry.moniker)] = normalized_start
        s_total_ops = sum(
            int(s_node[f"block.{op_key}.count"])
            for op_key, _ in _BLOCK_OPERATIONS
        )
        if s_driver_name not in start_by_name or s_total_ops > 0:
            start_by_name[s_driver_name] = normalized_start

    delta_entries: list[InspectEntry] = []
    for end_entry, node_name, end_node, _ in valid_end:
        start_entry = start_by_key.get(
            (end_entry.name, end_entry.moniker)
        ) or start_by_name.get(end_entry.name)
        if (
            start_entry is None
            or end_entry.timestamp_ns <= start_entry.timestamp_ns
        ):
            delta_entries.append(end_entry)
            continue
        start_node = start_entry.root.get(node_name)
        if not isinstance(start_node, Mapping):
            delta_entries.append(end_entry)
            continue
        if any(
            int(end_node[f"block.{op_key}.bytes"])
            < int(start_node[f"block.{op_key}.bytes"])
            or int(end_node[f"block.{op_key}.count"])
            < int(start_node[f"block.{op_key}.count"])
            for op_key, _ in _BLOCK_OPERATIONS
        ):
            delta_entries.append(end_entry)
            continue

        delta_node = dict(end_node)
        for op_key, _ in _BLOCK_OPERATIONS:
            delta_node[f"block.{op_key}.bytes"] = int(
                end_node[f"block.{op_key}.bytes"]
            ) - int(start_node[f"block.{op_key}.bytes"])
            delta_node[f"block.{op_key}.count"] = int(
                end_node[f"block.{op_key}.count"]
            ) - int(start_node[f"block.{op_key}.count"])
        delta_entries.append(
            InspectEntry(
                moniker=end_entry.moniker,
                name=end_entry.name,
                timestamp_ns=end_entry.timestamp_ns - start_entry.timestamp_ns,
                root={**end_entry.root, node_name: delta_node},
            )
        )
    return delta_entries


def extract_storage_io_metrics(
    inspect_entries: Sequence[InspectEntry | Mapping[str, Any]],
    label_prefix: str | None = None,
    *,
    baseline_entries: Sequence[InspectEntry | Mapping[str, Any]] | None = None,
) -> list[metrics.TestCaseResult]:
    """Extracts block/storage I/O rate metrics (`Block{Read,Write,Flush,Trim}{Bytes,Ops}PerSec`)
    from hardware block driver Inspect entries (`ftl`, `sdmmc_core`, `ufs`,
    `virtio_block`).

    When `baseline_entries` is provided, computes per-interval delta rates
    between `baseline_entries` and `inspect_entries` via `diff_inspect_entries`.
    Otherwise, computes cumulative rates over device uptime.

    Target devices have a single primary block storage driver. To guarantee
    deterministic behavior independent of the input order in `inspect_entries`,
    entries are sorted by `(name, moniker, timestamp_ns)` and any active block
    driver with non-zero I/O operations takes precedence over idle instances.
    """
    if baseline_entries is not None:
        inspect_entries = diff_inspect_entries(
            baseline_entries, inspect_entries
        )

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
    baseline_entries: Sequence[InspectEntry | Mapping[str, Any]] | None = None,
) -> metrics.Report:
    """Captures block/storage I/O rate metrics as a `metrics.Report`.

    Can be combined with trace or memory metrics via `report.extend(storage.capture(dut=self.dut))`.
    """
    entries = (
        inspect_entries
        if inspect_entries is not None
        else load_inspect_entries(dut=dut, snapshot_dirs=snapshot_dirs)
    )
    structured = extract_storage_io_metrics(
        entries,
        label_prefix=label_prefix,
        baseline_entries=baseline_entries,
    )
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
    baseline_entries: Sequence[InspectEntry | Mapping[str, Any]] | None = None,
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
            inspect_entries,
            label_prefix=resolved_prefix,
            baseline_entries=baseline_entries,
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

# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for storage.block_io."""

import json
import pathlib
import tempfile
import unittest
import zipfile
from typing import Any
from unittest.mock import Mock, patch

import storage
from reporting import metrics


def _expected_metrics(
    prefix: str | None,
    read_bytes: float,
    read_ops: float,
    write_bytes: float,
    write_ops: float,
    flush_bytes: float,
    flush_ops: float,
    trim_bytes: float,
    trim_ops: float,
) -> list[metrics.TestCaseResult]:
    label_prefix = f"{prefix}/" if prefix else ""
    return [
        metrics.TestCaseResult(
            label=f"{label_prefix}BlockReadBytesPerSec",
            unit=metrics.Unit.bytesPerSecond,
            values=(read_bytes,),
            doc="Cumulative block read bytes per second of device uptime.",
            direction=metrics.Direction.smallerIsBetter,
        ),
        metrics.TestCaseResult(
            label=f"{label_prefix}BlockReadOpsPerSec",
            unit=metrics.Unit.count,
            values=(read_ops,),
            doc="Cumulative block read operations per second of device uptime.",
            direction=metrics.Direction.smallerIsBetter,
        ),
        metrics.TestCaseResult(
            label=f"{label_prefix}BlockWriteBytesPerSec",
            unit=metrics.Unit.bytesPerSecond,
            values=(write_bytes,),
            doc="Cumulative block write bytes per second of device uptime.",
            direction=metrics.Direction.smallerIsBetter,
        ),
        metrics.TestCaseResult(
            label=f"{label_prefix}BlockWriteOpsPerSec",
            unit=metrics.Unit.count,
            values=(write_ops,),
            doc="Cumulative block write operations per second of device uptime.",
            direction=metrics.Direction.smallerIsBetter,
        ),
        metrics.TestCaseResult(
            label=f"{label_prefix}BlockFlushBytesPerSec",
            unit=metrics.Unit.bytesPerSecond,
            values=(flush_bytes,),
            doc="Cumulative block flush bytes per second of device uptime.",
            direction=metrics.Direction.smallerIsBetter,
        ),
        metrics.TestCaseResult(
            label=f"{label_prefix}BlockFlushOpsPerSec",
            unit=metrics.Unit.count,
            values=(flush_ops,),
            doc="Cumulative block flush operations per second of device uptime.",
            direction=metrics.Direction.smallerIsBetter,
        ),
        metrics.TestCaseResult(
            label=f"{label_prefix}BlockTrimBytesPerSec",
            unit=metrics.Unit.bytesPerSecond,
            values=(trim_bytes,),
            doc="Cumulative block trim bytes per second of device uptime.",
            direction=metrics.Direction.smallerIsBetter,
        ),
        metrics.TestCaseResult(
            label=f"{label_prefix}BlockTrimOpsPerSec",
            unit=metrics.Unit.count,
            values=(trim_ops,),
            doc="Cumulative block trim operations per second of device uptime.",
            direction=metrics.Direction.smallerIsBetter,
        ),
    ]


class BlockIoTest(unittest.TestCase):
    def test_extract_ftl_metrics(self) -> None:
        inspect_entries = [
            {
                "metadata": {
                    "name": "ftl",
                    "timestamp": 100_000_000_000,  # 100s
                },
                "payload": {
                    "root": {
                        "ftl": {
                            "block.read.bytes": 1000 * 4096,
                            "block.read.count": 500,
                            "block.write.bytes": 510 * 4096,
                            "block.write.count": 250,
                            "block.flush.bytes": 10 * 4096,
                            "block.flush.count": 20,
                            "block.trim.bytes": 100 * 4096,
                            "block.trim.count": 10,
                        }
                    }
                },
            }
        ]
        results = storage.extract_storage_io_metrics(
            inspect_entries, label_prefix="FtlTest"
        )
        self.assertEqual(
            results,
            _expected_metrics(
                prefix="FtlTest",
                read_bytes=(1000 * 4096) / 100.0,
                read_ops=5.0,
                write_bytes=(510 * 4096) / 100.0,
                write_ops=2.5,
                flush_bytes=(10 * 4096) / 100.0,
                flush_ops=0.2,
                trim_bytes=(100 * 4096) / 100.0,
                trim_ops=0.1,
            ),
        )

    def test_extract_sdmmc_metrics(self) -> None:
        inspect_entries = [
            {
                "metadata": {
                    "name": "sdmmc",
                    "timestamp": 50_000_000_000,  # 50s
                },
                "payload": {
                    "root": {
                        "sdmmc_core": {
                            "block.read.bytes": 2_000_000,
                            "block.read.count": 400,
                            "block.write.bytes": 1_000_000,
                            "block.write.count": 200,
                            "block.flush.bytes": 0,
                            "block.flush.count": 50,
                            "block.trim.bytes": 500_000,
                            "block.trim.count": 25,
                        }
                    }
                },
            }
        ]
        results = storage.extract_storage_io_metrics(
            inspect_entries, label_prefix="EmmcTest"
        )
        self.assertEqual(
            results,
            _expected_metrics(
                prefix="EmmcTest",
                read_bytes=40_000.0,
                read_ops=8.0,
                write_bytes=20_000.0,
                write_ops=4.0,
                flush_bytes=0.0,
                flush_ops=1.0,
                trim_bytes=10_000.0,
                trim_ops=0.5,
            ),
        )

    def test_extract_virtio_block_metrics(self) -> None:
        inspect_entries = [
            {
                "metadata": {
                    "name": "virtio-block",
                    "timestamp": 25_000_000_000,  # 25s
                },
                "payload": {
                    "root": {
                        "virtio_block": {
                            "block.read.bytes": 750_000,
                            "block.read.count": 150,
                            "block.write.bytes": 500_000,
                            "block.write.count": 100,
                            "block.flush.bytes": 0,
                            "block.flush.count": 25,
                            "block.trim.bytes": 250_000,
                            "block.trim.count": 50,
                        }
                    }
                },
            }
        ]
        results = storage.extract_storage_io_metrics(
            inspect_entries, label_prefix="VirtioTest"
        )
        self.assertEqual(
            results,
            _expected_metrics(
                prefix="VirtioTest",
                read_bytes=30_000.0,
                read_ops=6.0,
                write_bytes=20_000.0,
                write_ops=4.0,
                flush_bytes=0.0,
                flush_ops=1.0,
                trim_bytes=10_000.0,
                trim_ops=2.0,
            ),
        )

    def test_extract_ufs_metrics_via_capture(self) -> None:
        inspect_entries: list[dict[str, Any]] = [
            {
                "metadata": {
                    "name": "ufs",
                    "timestamp": 100_000_000_000,
                },
                "payload": {
                    "root": {
                        "ufs": {
                            "block.read.bytes": 4_000_000,
                            "block.read.count": 1000,
                            "block.write.bytes": 2_000_000,
                            "block.write.count": 500,
                            "block.flush.bytes": 0,
                            "block.flush.count": 100,
                            "block.trim.bytes": 1_000_000,
                            "block.trim.count": 50,
                        }
                    }
                },
            },
        ]
        report = storage.capture(inspect_entries=inspect_entries)
        self.assertEqual(
            report.structured,
            _expected_metrics(
                prefix=None,
                read_bytes=40_000.0,
                read_ops=10.0,
                write_bytes=20_000.0,
                write_ops=5.0,
                flush_bytes=0.0,
                flush_ops=1.0,
                trim_bytes=10_000.0,
                trim_ops=0.5,
            ),
        )

    def test_load_inspect_entries_from_snapshot_zip_and_publish(self) -> None:
        with tempfile.TemporaryDirectory() as tmp_dir:
            log_path = pathlib.Path(tmp_dir)
            teardown_dir = log_path / "teardown_class"
            snapshot_zip_path = log_path / "Snapshot_dut_123.zip"
            inspect_payload = [
                {
                    "metadata": {
                        "name": "sdmmc",
                        "timestamp": 10_000_000_000,  # 10s
                    },
                    "payload": {
                        "root": {
                            "sdmmc_core": {
                                "block.read.bytes": 81_920,
                                "block.read.count": 20,
                                "block.write.bytes": 40_960,
                                "block.write.count": 10,
                                "block.flush.bytes": 0,
                                "block.flush.count": 5,
                                "block.trim.bytes": 4096,
                                "block.trim.count": 1,
                            }
                        }
                    },
                }
            ]
            with zipfile.ZipFile(snapshot_zip_path, "w") as zf:
                zf.writestr("inspect.json", json.dumps(inspect_payload))

            class FakeTest:
                def __init__(self) -> None:
                    self.log_path = str(log_path)
                    self._teardown_class_artifacts = str(teardown_dir)
                    self.dut = Mock()

            fake_test = FakeTest()
            with patch(
                "storage.block_io.publish.publish_fuchsiaperf"
            ) as mock_publish:
                published = storage.publish_teardown_storage_io_metrics(
                    fake_test,
                    test_suite="fuchsia.storage.io",
                    runtime_deps_dir="runtime_deps",
                )

            self.assertEqual(len(published), 8)
            expected_json_path = (
                teardown_dir / "fuchsia.storage.io-benchmark.fuchsiaperf.json"
            )
            self.assertTrue(expected_json_path.exists())
            mock_publish.assert_called_once_with(
                fuchsia_perf_file_paths=[expected_json_path],
                expected_metric_names_filename="fuchsia.storage.io.txt",
                runtime_deps_dir="runtime_deps",
            )

    def test_deterministic_selection_across_multiple_entries(self) -> None:
        idle_entry = storage.InspectEntry(
            moniker="bootstrap/boot-drivers:dev.sdmmc.0",
            name="sdmmc",
            timestamp_ns=50_000_000_000,
            root={
                "sdmmc_core": {
                    "block.read.bytes": 0,
                    "block.read.count": 0,
                    "block.write.bytes": 0,
                    "block.write.count": 0,
                    "block.flush.bytes": 0,
                    "block.flush.count": 0,
                    "block.trim.bytes": 0,
                    "block.trim.count": 0,
                }
            },
        )
        active_entry = storage.InspectEntry(
            moniker="bootstrap/boot-drivers:dev.sdmmc.1",
            name="sdmmc",
            timestamp_ns=50_000_000_000,
            root={
                "sdmmc_core": {
                    "block.read.bytes": 2_000_000,
                    "block.read.count": 400,
                    "block.write.bytes": 1_000_000,
                    "block.write.count": 200,
                    "block.flush.bytes": 0,
                    "block.flush.count": 50,
                    "block.trim.bytes": 500_000,
                    "block.trim.count": 25,
                }
            },
        )
        expected = _expected_metrics(
            prefix="EmmcTest",
            read_bytes=40_000.0,
            read_ops=8.0,
            write_bytes=20_000.0,
            write_ops=4.0,
            flush_bytes=0.0,
            flush_ops=1.0,
            trim_bytes=10_000.0,
            trim_ops=0.5,
        )
        self.assertEqual(
            storage.extract_storage_io_metrics(
                [idle_entry, active_entry], label_prefix="EmmcTest"
            ),
            expected,
        )
        self.assertEqual(
            storage.extract_storage_io_metrics(
                [active_entry, idle_entry], label_prefix="EmmcTest"
            ),
            expected,
        )


if __name__ == "__main__":
    unittest.main()

# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for storage.block_io."""

import asyncio
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
                            "block.read.kib": 1000 * 4,
                            "block.read.count": 500,
                            "block.write.kib": 510 * 4,
                            "block.write.count": 250,
                            "block.flush.kib": 10 * 4,
                            "block.flush.count": 20,
                            "block.trim.kib": 100 * 4,
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
                            "block.read.kib": 2000,
                            "block.read.count": 400,
                            "block.write.kib": 1000,
                            "block.write.count": 200,
                            "block.flush.kib": 0,
                            "block.flush.count": 50,
                            "block.trim.kib": 500,
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
                read_bytes=40_960.0,
                read_ops=8.0,
                write_bytes=20_480.0,
                write_ops=4.0,
                flush_bytes=0.0,
                flush_ops=1.0,
                trim_bytes=10_240.0,
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
                            "block.read.kib": 750,
                            "block.read.count": 150,
                            "block.write.kib": 500,
                            "block.write.count": 100,
                            "block.flush.kib": 0,
                            "block.flush.count": 25,
                            "block.trim.kib": 250,
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
                read_bytes=30_720.0,
                read_ops=6.0,
                write_bytes=20_480.0,
                write_ops=4.0,
                flush_bytes=0.0,
                flush_ops=1.0,
                trim_bytes=10_240.0,
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
                            "block.read.kib": 4000,
                            "block.read.count": 1000,
                            "block.write.kib": 2000,
                            "block.write.count": 500,
                            "block.flush.kib": 0,
                            "block.flush.count": 100,
                            "block.trim.kib": 1000,
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
                read_bytes=40_960.0,
                read_ops=10.0,
                write_bytes=20_480.0,
                write_ops=5.0,
                flush_bytes=0.0,
                flush_ops=1.0,
                trim_bytes=10_240.0,
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
                                "block.read.kib": 80,
                                "block.read.count": 20,
                                "block.write.kib": 40,
                                "block.write.count": 10,
                                "block.flush.kib": 0,
                                "block.flush.count": 5,
                                "block.trim.kib": 4,
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
                    "block.read.kib": 0,
                    "block.read.count": 0,
                    "block.write.kib": 0,
                    "block.write.count": 0,
                    "block.flush.kib": 0,
                    "block.flush.count": 0,
                    "block.trim.kib": 0,
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
                    "block.read.kib": 2000,
                    "block.read.count": 400,
                    "block.write.kib": 1000,
                    "block.write.count": 200,
                    "block.flush.kib": 0,
                    "block.flush.count": 50,
                    "block.trim.kib": 500,
                    "block.trim.count": 25,
                }
            },
        )
        expected = _expected_metrics(
            prefix="EmmcTest",
            read_bytes=40_960.0,
            read_ops=8.0,
            write_bytes=20_480.0,
            write_ops=4.0,
            flush_bytes=0.0,
            flush_ops=1.0,
            trim_bytes=10_240.0,
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

    def test_extract_when_name_is_none(self) -> None:
        entry = storage.InspectEntry(
            moniker="bootstrap/boot-drivers:nand.fvm",
            name=None,
            timestamp_ns=50_000_000_000,
            root={
                "ftl": {
                    "block.read.kib": 2000,
                    "block.read.count": 400,
                    "block.write.kib": 1000,
                    "block.write.count": 200,
                    "block.flush.kib": 0,
                    "block.flush.count": 50,
                    "block.trim.kib": 500,
                    "block.trim.count": 25,
                }
            },
        )
        self.assertEqual(
            storage.extract_storage_io_metrics([entry], label_prefix="FtlTest"),
            _expected_metrics(
                prefix="FtlTest",
                read_bytes=40_960.0,
                read_ops=8.0,
                write_bytes=20_480.0,
                write_ops=4.0,
                flush_bytes=0.0,
                flush_ops=1.0,
                trim_bytes=10_240.0,
                trim_ops=0.5,
            ),
        )

    def test_extract_storage_io_metrics_with_baseline_entries(self) -> None:
        start_entry = storage.InspectEntry(
            moniker="bootstrap/boot-drivers:dev.ufs",
            name=None,
            timestamp_ns=100_000_000_000,
            root={
                "ufs": {
                    "block.read.kib": 500_000,
                    "block.read.count": 20_000,
                    "block.write.kib": 200_000,
                    "block.write.count": 10_000,
                    "block.flush.kib": 0,
                    "block.flush.count": 500,
                    "block.trim.kib": 10_000,
                    "block.trim.count": 100,
                }
            },
        )
        end_entry = storage.InspectEntry(
            moniker="bootstrap/boot-drivers:dev.ufs",
            name=None,
            timestamp_ns=110_000_000_000,
            root={
                "ufs": {
                    "block.read.kib": 500_040,
                    "block.read.count": 20_010,
                    "block.write.kib": 200_080,
                    "block.write.count": 10_020,
                    "block.flush.kib": 0,
                    "block.flush.count": 505,
                    "block.trim.kib": 10_010,
                    "block.trim.count": 102,
                }
            },
        )
        self.assertEqual(
            storage.extract_storage_io_metrics(
                [end_entry],
                label_prefix="Storage/System",
                baseline_entries=[start_entry],
            ),
            _expected_metrics(
                prefix="Storage/System",
                read_bytes=4096.0,
                read_ops=1.0,
                write_bytes=8192.0,
                write_ops=2.0,
                flush_bytes=0.0,
                flush_ops=0.5,
                trim_bytes=1024.0,
                trim_ops=0.2,
            ),
        )

    def test_diff_inspect_entries_handles_zero_delta_with_idle_secondary_driver(
        self,
    ) -> None:
        idle_wifi_sdmmc = storage.InspectEntry(
            moniker="bootstrap/boot-drivers:wifi-sdio",
            name="sdmmc",
            timestamp_ns=100_000_000_000,
            root={
                "sdmmc_core": {
                    "block.read.kib": 0,
                    "block.read.count": 0,
                    "block.write.kib": 0,
                    "block.write.count": 0,
                    "block.flush.kib": 0,
                    "block.flush.count": 0,
                    "block.trim.kib": 0,
                    "block.trim.count": 0,
                }
            },
        )
        active_start = storage.InspectEntry(
            moniker="bootstrap/boot-drivers:emmc",
            name="sdmmc",
            timestamp_ns=100_000_000_000,
            root={
                "sdmmc_core": {
                    "block.read.kib": 500_000,
                    "block.read.count": 20_000,
                    "block.write.kib": 200_000,
                    "block.write.count": 10_000,
                    "block.flush.kib": 0,
                    "block.flush.count": 500,
                    "block.trim.kib": 0,
                    "block.trim.count": 0,
                }
            },
        )
        active_end = storage.InspectEntry(
            moniker="bootstrap/boot-drivers:emmc",
            name="sdmmc",
            timestamp_ns=130_000_000_000,
            root=active_start.root,
        )
        self.assertEqual(
            storage.extract_storage_io_metrics(
                [idle_wifi_sdmmc, active_end],
                label_prefix="Storage/System",
                baseline_entries=[idle_wifi_sdmmc, active_start],
            ),
            _expected_metrics(
                prefix="Storage/System",
                read_bytes=0.0,
                read_ops=0.0,
                write_bytes=0.0,
                write_ops=0.0,
                flush_bytes=0.0,
                flush_ops=0.0,
                trim_bytes=0.0,
                trim_ops=0.0,
            ),
        )

    def test_storage_io_metrics_processor_structured_and_freeform(self) -> None:
        start_entry = storage.InspectEntry(
            moniker="bootstrap/boot-drivers:dev.ufs",
            name="ufs",
            timestamp_ns=100_000_000_000,
            root={
                "ufs": {
                    "block.read.kib": 500_000,
                    "block.read.count": 20_000,
                    "block.write.kib": 200_000,
                    "block.write.count": 10_000,
                    "block.flush.kib": 0,
                    "block.flush.count": 500,
                    "block.trim.kib": 10_000,
                    "block.trim.count": 100,
                }
            },
        )
        end_entry = storage.InspectEntry(
            moniker="bootstrap/boot-drivers:dev.ufs",
            name="ufs",
            timestamp_ns=110_000_000_000,
            root={
                "ufs": {
                    "block.read.kib": 500_040,
                    "block.read.count": 20_010,
                    "block.write.kib": 200_080,
                    "block.write.count": 10_020,
                    "block.flush.kib": 0,
                    "block.flush.count": 505,
                    "block.trim.kib": 10_010,
                    "block.trim.count": 102,
                }
            },
        )
        processor = storage.StorageIoMetricsProcessor(
            start_entries=[start_entry],
            end_entries=[end_entry],
        )
        self.assertEqual(processor.event_patterns, set())
        self.assertEqual(
            processor.process_metrics(),
            [
                metrics.TestCaseResult(
                    label="Storage/System/BlockWriteOpsPerSec",
                    unit=metrics.Unit.count,
                    values=(2.0,),
                    doc="Block write operations per second of interval.",
                    direction=metrics.Direction.smallerIsBetter,
                ),
                metrics.TestCaseResult(
                    label="Storage/System/BlockFlushOpsPerSec",
                    unit=metrics.Unit.count,
                    values=(0.5,),
                    doc="Block flush operations per second of interval.",
                    direction=metrics.Direction.smallerIsBetter,
                ),
            ],
        )
        self.assertEqual(
            processor.process_freeform_metrics(),
            (
                "storage_io",
                {
                    "read": {
                        "bytes_per_sec": 4096.0,
                        "ops_per_sec": 1.0,
                    },
                    "write": {
                        "bytes_per_sec": 8192.0,
                        "ops_per_sec": 2.0,
                    },
                    "flush": {
                        "bytes_per_sec": 0.0,
                        "ops_per_sec": 0.5,
                    },
                    "trim": {
                        "bytes_per_sec": 1024.0,
                        "ops_per_sec": 0.2,
                    },
                },
            ),
        )

    def test_storage_io_metrics_processor_context_manager_with_dut(
        self,
    ) -> None:
        start_entry = storage.InspectEntry(
            moniker="bootstrap/boot-drivers:dev.ufs",
            name="ufs",
            timestamp_ns=100_000_000_000,
            root={
                "ufs": {
                    "block.read.kib": 1000,
                    "block.read.count": 100,
                    "block.write.kib": 2000,
                    "block.write.count": 200,
                    "block.flush.kib": 0,
                    "block.flush.count": 50,
                    "block.trim.kib": 0,
                    "block.trim.count": 0,
                }
            },
        )
        end_entry = storage.InspectEntry(
            moniker="bootstrap/boot-drivers:dev.ufs",
            name="ufs",
            timestamp_ns=105_000_000_000,
            root={
                "ufs": {
                    "block.read.kib": 1020,
                    "block.read.count": 105,
                    "block.write.kib": 2050,
                    "block.write.count": 215,
                    "block.flush.kib": 0,
                    "block.flush.count": 55,
                    "block.trim.kib": 0,
                    "block.trim.count": 0,
                }
            },
        )
        dut = Mock()
        with patch(
            "storage.block_io.load_inspect_entries",
            side_effect=[[start_entry], [end_entry]],
        ) as mock_load:
            with storage.StorageIoMetricsProcessor(dut) as processor:
                pass

        self.assertEqual(mock_load.call_count, 2)
        self.assertEqual(
            [m.values for m in processor.process_metrics()],
            [(3.0,), (1.0,)],
        )

    def test_storage_io_metrics_processor_empty_end_entries(self) -> None:
        start_entry = storage.InspectEntry(
            moniker="bootstrap/boot-drivers:dev.ufs",
            name="ufs",
            timestamp_ns=100_000_000_000,
            root={
                "ufs": {
                    "block.read.kib": 1000,
                    "block.read.count": 100,
                    "block.write.kib": 2000,
                    "block.write.count": 200,
                    "block.flush.kib": 0,
                    "block.flush.count": 50,
                    "block.trim.kib": 0,
                    "block.trim.count": 0,
                }
            },
        )
        processor = storage.StorageIoMetricsProcessor(
            start_entries=[start_entry], end_entries=[]
        )
        self.assertEqual(processor.process_metrics(), [])
        self.assertEqual(
            processor.process_freeform_metrics(), ("storage_io", {})
        )

    def test_storage_io_metrics_processor_async_context_manager(
        self,
    ) -> None:
        async def run_test() -> storage.StorageIoMetricsProcessor:
            async with storage.StorageIoMetricsProcessor(dut) as p:
                return p

        start_entry = storage.InspectEntry(
            moniker="bootstrap/boot-drivers:dev.ufs",
            name="ufs",
            timestamp_ns=100_000_000_000,
            root={
                "ufs": {
                    "block.read.kib": 1000,
                    "block.read.count": 100,
                    "block.write.kib": 2000,
                    "block.write.count": 200,
                    "block.flush.kib": 0,
                    "block.flush.count": 50,
                    "block.trim.kib": 0,
                    "block.trim.count": 0,
                }
            },
        )
        end_entry = storage.InspectEntry(
            moniker="bootstrap/boot-drivers:dev.ufs",
            name="ufs",
            timestamp_ns=105_000_000_000,
            root={
                "ufs": {
                    "block.read.kib": 1020,
                    "block.read.count": 105,
                    "block.write.kib": 2050,
                    "block.write.count": 215,
                    "block.flush.kib": 0,
                    "block.flush.count": 55,
                    "block.trim.kib": 0,
                    "block.trim.count": 0,
                }
            },
        )
        dut = Mock()
        with patch(
            "storage.block_io.load_inspect_entries",
            side_effect=[[start_entry], [end_entry]],
        ) as mock_load:
            processor = asyncio.run(run_test())

        self.assertEqual(mock_load.call_count, 2)
        self.assertEqual(
            [m.values for m in processor.process_metrics()],
            [(3.0,), (1.0,)],
        )


if __name__ == "__main__":
    unittest.main()

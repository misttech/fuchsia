#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for ../metrics/odpm_power.py."""

import unittest

from reporting import metrics
from trace_processing import trace_model
from trace_processing.metrics import odpm_power

U = metrics.Unit
TestCaseResult = metrics.TestCaseResult


class OdpmPowerMetricsTest(unittest.TestCase):
    """ODPM power metrics tests."""

    def construct_trace_model(self) -> trace_model.Model:
        """Builds a fake trace model with ODPM power CounterEvents."""
        events: list[trace_model.Event] = [
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "other_category",
                    "name": "cpu_big_odpm_rail",
                    "ts": 1000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 1000.0},  # 1.0 W
                }
            ),
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "other_category",
                    "name": "cpu_big_odpm_rail",
                    "ts": 2000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 3000.0},  # 3.0 W
                }
            ),
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "gpu_odpm_rail",
                    "ts": 1000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 500.0},  # 0.5 W
                }
            ),
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "gpu_odpm_rail",
                    "ts": 2000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 1500.0},  # 1.5 W
                }
            ),
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "wlan_bt_odpm_rail",
                    "ts": 1000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 250.0},  # 0.25 W
                }
            ),
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "wlan_bt_odpm_rail",
                    "ts": 2000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 750.0},  # 0.75 W
                }
            ),
            # Counter event without _odpm_rail suffix should be ignored
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "non_odpm_counter",
                    "ts": 1000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 500.0},
                }
            ),
            # Non-ODPM power event (missing mW arg) should be ignored
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "other_counter_odpm_rail",
                    "ts": 1000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"Voltage": 12.0},
                }
            ),
        ]

        odpm_process = trace_model.Process(
            100,
            "odpm.cm",
            [trace_model.Thread(101, "fdf-dispatcher-thread-0", events)],
        )
        return trace_model.Model([odpm_process], {})

    def test_empty_rails_raises_value_error(self) -> None:
        """Verifies that invalid rail / all_rails / sum_rails combinations raise ValueError."""
        with self.assertRaises(ValueError):
            odpm_power.OdpmPowerMetricsProcessor(rails=[])
        with self.assertRaises(ValueError):
            odpm_power.OdpmPowerMetricsProcessor()
        with self.assertRaises(ValueError):
            odpm_power.OdpmPowerMetricsProcessor(
                rails=["cpu_big"], all_rails=True
            )
        with self.assertRaises(ValueError):
            odpm_power.OdpmPowerMetricsProcessor(sum_rails={"empty_group": []})

    def test_trace_import_filter_modes(self) -> None:
        """Verifies trace import filtering uses _odpm_rail event patterns and no category filter."""
        processor = odpm_power.OdpmPowerMetricsProcessor(
            rails=["gpu", "cpu_big"]
        )
        self.assertEqual(processor.event_patterns, {r".*_odpm_rail"})
        self.assertEqual(processor.category_names, set())

        sum_rails_processor = odpm_power.OdpmPowerMetricsProcessor(
            rails=["wlan_bt"],
            sum_rails={"compute": ["cpu_big", "gpu"]},
        )
        self.assertEqual(sum_rails_processor.event_patterns, {r".*_odpm_rail"})
        self.assertEqual(sum_rails_processor.category_names, set())

        all_rails_processor = odpm_power.OdpmPowerMetricsProcessor(
            all_rails=True
        )
        self.assertEqual(all_rails_processor.event_patterns, {r".*_odpm_rail"})
        self.assertEqual(all_rails_processor.category_names, set())

        predicate_sum_processor = odpm_power.OdpmPowerMetricsProcessor(
            sum_rails={"all_rails_except_battery": lambda r: r != "battery"},
        )
        self.assertEqual(
            predicate_sum_processor.event_patterns, {r".*_odpm_rail"}
        )
        self.assertEqual(predicate_sum_processor.category_names, set())

    def test_list_rails(self) -> None:
        """Verifies OdpmPowerMetricsProcessor.list_rails discovers available ODPM rails in a model."""
        model = self.construct_trace_model()
        self.assertEqual(
            odpm_power.OdpmPowerMetricsProcessor.list_rails(model),
            ["cpu_big", "gpu", "wlan_bt"],
        )

    def test_process_metrics_selected_rails(self) -> None:
        """Verifies metrics calculation only for selected rails in order."""
        model = self.construct_trace_model()
        processor = odpm_power.OdpmPowerMetricsProcessor(
            rails=["gpu", "cpu_big"]
        )
        results = processor.process_metrics(model)

        expected = [
            TestCaseResult(
                label="Power_rail_gpu",
                unit=U.watts,
                values=[0.5, 1.5],
                doc="ODPM power usage samples for rail gpu",
            ),
            TestCaseResult(
                label="Power_rail_cpu_big",
                unit=U.watts,
                values=[1.0, 3.0],
                doc="ODPM power usage samples for rail cpu_big",
            ),
        ]
        self.assertEqual(results, expected)

    def test_process_metrics_all_rails(self) -> None:
        """Verifies all_rails=True reports metrics for all available rails in sorted order."""
        model = self.construct_trace_model()
        processor = odpm_power.OdpmPowerMetricsProcessor(all_rails=True)
        results = processor.process_metrics(model)

        expected = [
            TestCaseResult(
                label="Power_rail_cpu_big",
                unit=U.watts,
                values=[1.0, 3.0],
                doc="ODPM power usage samples for rail cpu_big",
            ),
            TestCaseResult(
                label="Power_rail_gpu",
                unit=U.watts,
                values=[0.5, 1.5],
                doc="ODPM power usage samples for rail gpu",
            ),
            TestCaseResult(
                label="Power_rail_wlan_bt",
                unit=U.watts,
                values=[0.25, 0.75],
                doc="ODPM power usage samples for rail wlan_bt",
            ),
        ]
        self.assertEqual(results, expected)

    def test_missing_rail_skipped(self) -> None:
        """Verifies missing rails are skipped and empty result returned if none match."""
        model = self.construct_trace_model()
        processor = odpm_power.OdpmPowerMetricsProcessor(
            rails=["nonexistent_rail"]
        )
        self.assertEqual(processor.process_metrics(model), [])

    def test_sum_rails_sample_count_discrepancy(self) -> None:
        """Verifies discrepancy <= 2 truncates with a warning while > 2 raises ValueError."""
        model = self.construct_trace_model()
        thread = model.processes[0].threads[0]
        # Add 2 extra samples to cpu_big (total 4 vs gpu's 2 -> discrepancy of 2, allowed)
        for ts in (3000, 4000):
            thread.events.append(
                trace_model.CounterEvent.consume_dict(
                    {
                        "cat": "power",
                        "name": "cpu_big_odpm_rail",
                        "ts": ts,
                        "pid": 100,
                        "tid": 101,
                        "args": {"mW": 5000.0},
                    }
                )
            )

        processor = odpm_power.OdpmPowerMetricsProcessor(
            sum_rails={"compute": ["cpu_big", "gpu"]}
        )
        results = processor.process_metrics(model)
        self.assertEqual(
            results,
            [
                TestCaseResult(
                    label="Power_compute",
                    unit=U.watts,
                    values=[1.5, 4.5],
                    doc="ODPM power usage samples for rails cpu_big, gpu",
                )
            ],
        )

        # Add a 5th sample to cpu_big (total 5 vs gpu's 2 -> discrepancy of 3, raises ValueError)
        thread.events.append(
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "cpu_big_odpm_rail",
                    "ts": 5000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 5000.0},
                }
            )
        )
        with self.assertRaises(ValueError):
            processor.process_metrics(model)

    def test_process_metrics_sum_rails(self) -> None:
        """Verifies sample-by-sample sum across specific lists of rails in sum_rails."""
        model = self.construct_trace_model()
        processor = odpm_power.OdpmPowerMetricsProcessor(
            sum_rails={"compute": ["cpu_big", "gpu"]}
        )
        results = processor.process_metrics(model)

        # cpu_big (1.0, 3.0) + gpu (0.5, 1.5) = (1.5, 4.5)
        expected = [
            TestCaseResult(
                label="Power_compute",
                unit=U.watts,
                values=[1.5, 4.5],
                doc="ODPM power usage samples for rails cpu_big, gpu",
            ),
        ]
        self.assertEqual(results, expected)

    def test_process_metrics_sum_rails_callable_predicate(self) -> None:
        """Verifies sum_rails accepts a callable predicate to filter rails in a single pass."""
        model = self.construct_trace_model()
        thread = model.processes[0].threads[0]
        for ts, mw in ((1000, 9000.0), (2000, 9000.0)):
            thread.events.append(
                trace_model.CounterEvent.consume_dict(
                    {
                        "cat": "power",
                        "name": "battery_odpm_rail",
                        "ts": ts,
                        "pid": 100,
                        "tid": 101,
                        "args": {"mW": mw},
                    }
                )
            )

        processor = odpm_power.OdpmPowerMetricsProcessor(
            all_rails=True,
            sum_rails={"all_rails_except_battery": lambda r: r != "battery"},
        )
        results = processor.process_metrics(model)

        # cpu_big (1.0, 3.0) + gpu (0.5, 1.5) + wlan_bt (0.25, 0.75) = (1.75, 5.25)
        expected = [
            TestCaseResult(
                label="Power_rail_battery",
                unit=U.watts,
                values=[9.0, 9.0],
                doc="ODPM power usage samples for rail battery",
            ),
            TestCaseResult(
                label="Power_rail_cpu_big",
                unit=U.watts,
                values=[1.0, 3.0],
                doc="ODPM power usage samples for rail cpu_big",
            ),
            TestCaseResult(
                label="Power_rail_gpu",
                unit=U.watts,
                values=[0.5, 1.5],
                doc="ODPM power usage samples for rail gpu",
            ),
            TestCaseResult(
                label="Power_rail_wlan_bt",
                unit=U.watts,
                values=[0.25, 0.75],
                doc="ODPM power usage samples for rail wlan_bt",
            ),
            TestCaseResult(
                label="Power_all_rails_except_battery",
                unit=U.watts,
                values=[1.75, 5.25],
                doc="ODPM power usage samples for rails cpu_big, gpu, wlan_bt",
            ),
        ]
        self.assertEqual(results, expected)

    def test_sum_rails_callable_predicate_no_match_raises_value_error(
        self,
    ) -> None:
        """Verifies that a callable predicate in sum_rails matching no rails raises ValueError."""
        model = self.construct_trace_model()
        processor = odpm_power.OdpmPowerMetricsProcessor(
            sum_rails={"nonexistent_group": lambda r: r == "nonexistent"}
        )
        with self.assertRaisesRegex(
            ValueError,
            r"No ODPM power samples matched filter for sum_rails group 'nonexistent_group'\.",
        ):
            processor.process_metrics(model)

    def test_sum_rails_missing_rail_raises_value_error(self) -> None:
        """Verifies that a missing rail in sum_rails raises ValueError."""
        model = self.construct_trace_model()
        # One present rail ("cpu_big") and one missing rail ("cpu_little")
        processor_partial = odpm_power.OdpmPowerMetricsProcessor(
            sum_rails={"cpu_total": ["cpu_big", "cpu_little"]}
        )
        with self.assertRaises(ValueError):
            processor_partial.process_metrics(model)

        # All requested rails missing from trace
        processor_all_missing = odpm_power.OdpmPowerMetricsProcessor(
            sum_rails={"cpu_total": ["cpu_little", "cpu_mid"]}
        )
        with self.assertRaises(ValueError):
            processor_all_missing.process_metrics(model)

    def test_process_metrics_rails_callable_predicate(self) -> None:
        """Verifies a rails predicate filters the rails in sorted order."""
        model = self.construct_trace_model()
        # Reverse the order of the rails in the trace, keeping sample order.
        model.processes[0].threads[0].events.sort(
            key=lambda event: event.name, reverse=True
        )
        processor = odpm_power.OdpmPowerMetricsProcessor(
            rails=lambda r: r != "gpu"
        )
        results = processor.process_metrics(model)

        expected = [
            TestCaseResult(
                label="Power_rail_cpu_big",
                unit=U.watts,
                values=[1.0, 3.0],
                doc="ODPM power usage samples for rail cpu_big",
            ),
            TestCaseResult(
                label="Power_rail_wlan_bt",
                unit=U.watts,
                values=[0.25, 0.75],
                doc="ODPM power usage samples for rail wlan_bt",
            ),
        ]
        self.assertEqual(results, expected)

    def test_process_metrics_rails_and_sum_rails_callable_predicate(
        self,
    ) -> None:
        """Verifies one predicate can select the individual and summed rails."""
        model = self.construct_trace_model()
        thread = model.processes[0].threads[0]
        for ts in (1000, 2000):
            thread.events.append(
                trace_model.CounterEvent.consume_dict(
                    {
                        "cat": "power",
                        "name": "battery_odpm_rail",
                        "ts": ts,
                        "pid": 100,
                        "tid": 101,
                        "args": {"mW": 9000.0},
                    }
                )
            )

        non_battery_rails = lambda r: r != "battery"
        processor = odpm_power.OdpmPowerMetricsProcessor(
            rails=non_battery_rails,
            sum_rails={"all_rails_except_battery": non_battery_rails},
        )
        results = processor.process_metrics(model)

        self.assertEqual(
            [result.label for result in results],
            [
                "Power_rail_cpu_big",
                "Power_rail_gpu",
                "Power_rail_wlan_bt",
                "Power_all_rails_except_battery",
            ],
        )

    def test_trace_import_filter_rails_callable_predicate(self) -> None:
        """Verifies a callable rails predicate imports all _odpm_rail events."""
        processor = odpm_power.OdpmPowerMetricsProcessor(
            rails=lambda r: r != "battery"
        )
        self.assertEqual(processor.event_patterns, {r".*_odpm_rail"})
        self.assertEqual(processor.category_names, set())

    def test_rails_callable_predicate_and_all_rails_raises_value_error(
        self,
    ) -> None:
        """Verifies a rails predicate with all_rails=True raises ValueError."""
        with self.assertRaises(ValueError):
            odpm_power.OdpmPowerMetricsProcessor(
                rails=lambda r: r != "battery", all_rails=True
            )

    def test_bare_str_rails_raises_value_error(self) -> None:
        """Verifies a bare str in rails or sum_rails raises ValueError."""
        with self.assertRaises(ValueError):
            odpm_power.OdpmPowerMetricsProcessor(rails="gpu")
        with self.assertRaises(ValueError):
            odpm_power.OdpmPowerMetricsProcessor(sum_rails={"total": "cpu"})

    def test_aggregates_only_and_freeform_duration_weighted(self) -> None:
        # Verifies duration-weighted PowerAverage metrics and freeform breakdown.
        # Non-uniform timestamps (in microseconds):
        #   t=1000us: battery = 1000mW (1.0W), cpu_big = 200mW (0.2W),
        #     gpu = 300mW (0.3W)
        #   t=2000us (dt=1000us): battery = 2000mW (2.0W), cpu_big = 400mW (0.4W),
        #     gpu = 600mW (0.6W)
        #   t=5000us (dt=3000us): battery = 4000mW (4.0W), cpu_big = 800mW (0.8W),
        #     gpu = 1200mW (1.2W)
        # Duration-weighted battery average:
        #   (2.0W * 1000us + 4.0W * 3000us) / 4000us = 14.0 / 4.0 = 3.5W
        # Duration-weighted cpu_big average:
        #   (0.4W * 1000us + 0.8W * 3000us) / 4000us = 2.8 / 4.0 = 0.7W
        # Duration-weighted gpu average:
        #   (0.6W * 1000us + 1.2W * 3000us) / 4000us = 4.2 / 4.0 = 1.05W
        # Duration-weighted all_rails_except_battery (cpu_big + gpu) average:
        #   (1.0W * 1000us + 2.0W * 3000us) / 4000us = 7.0 / 4.0 = 1.75W
        events: list[trace_model.Event] = [
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "battery_odpm_rail",
                    "ts": 5000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 4000.0},
                }
            ),
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "battery_odpm_rail",
                    "ts": 1000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 1000.0},
                }
            ),
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "battery_odpm_rail",
                    "ts": 2000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 2000.0},
                }
            ),
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "cpu_big_odpm_rail",
                    "ts": 1000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 200.0},
                }
            ),
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "cpu_big_odpm_rail",
                    "ts": 2000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 400.0},
                }
            ),
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "cpu_big_odpm_rail",
                    "ts": 5000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 800.0},
                }
            ),
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "gpu_odpm_rail",
                    "ts": 1000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 300.0},
                }
            ),
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "gpu_odpm_rail",
                    "ts": 2000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 600.0},
                }
            ),
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "gpu_odpm_rail",
                    "ts": 5000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 1200.0},
                }
            ),
        ]
        odpm_process = trace_model.Process(
            100,
            "odpm.cm",
            [trace_model.Thread(101, "fdf-dispatcher-thread-0", events)],
        )
        model = trace_model.Model([odpm_process], {})

        processor = odpm_power.OdpmPowerMetricsProcessor(
            rails=["battery"],
            sum_rails={"all_rails_except_battery": lambda r: r != "battery"},
            aggregates_only=True,
        )
        results = processor.process_metrics(model)
        filename, freeform = processor.process_freeform_metrics(model)

        self.assertEqual(
            results,
            [
                TestCaseResult(
                    label="PowerAverage_rail_battery",
                    unit=U.watts,
                    values=[3.5],
                    doc="ODPM average power usage for rail battery",
                ),
                TestCaseResult(
                    label="PowerAverage_all_rails_except_battery",
                    unit=U.watts,
                    values=[1.75],
                    doc="ODPM average power usage for rails cpu_big, gpu",
                ),
            ],
        )
        self.assertEqual(filename, "odpm_power")
        self.assertIsInstance(freeform, dict)
        assert isinstance(freeform, dict)
        self.assertEqual(
            list(freeform.keys()),
            ["all_rails_except_battery", "battery", "cpu_big", "gpu"],
        )
        non_battery_stats = freeform["all_rails_except_battery"]
        self.assertIsInstance(non_battery_stats, dict)
        assert isinstance(non_battery_stats, dict)
        self.assertAlmostEqual(non_battery_stats["average"], 1.75)
        self.assertAlmostEqual(non_battery_stats["min"], 0.5)
        self.assertAlmostEqual(non_battery_stats["max"], 2.0)
        battery_stats = freeform["battery"]
        self.assertIsInstance(battery_stats, dict)
        assert isinstance(battery_stats, dict)
        self.assertAlmostEqual(battery_stats["average"], 3.5)
        self.assertAlmostEqual(battery_stats["min"], 1.0)
        self.assertAlmostEqual(battery_stats["max"], 4.0)
        cpu_stats = freeform["cpu_big"]
        self.assertIsInstance(cpu_stats, dict)
        assert isinstance(cpu_stats, dict)
        self.assertAlmostEqual(cpu_stats["average"], 0.7)
        self.assertAlmostEqual(cpu_stats["min"], 0.2)
        self.assertAlmostEqual(cpu_stats["max"], 0.8)
        gpu_stats = freeform["gpu"]
        self.assertIsInstance(gpu_stats, dict)
        assert isinstance(gpu_stats, dict)
        self.assertAlmostEqual(gpu_stats["average"], 1.05)
        self.assertAlmostEqual(gpu_stats["min"], 0.3)
        self.assertAlmostEqual(gpu_stats["max"], 1.2)

    def test_aggregates_only_single_sample(self) -> None:
        """Verifies single-sample fallback for aggregate metrics."""
        single_event = trace_model.CounterEvent.consume_dict(
            {
                "cat": "power",
                "name": "battery_odpm_rail",
                "ts": 1000,
                "pid": 100,
                "tid": 101,
                "args": {"mW": 2500.0},
            }
        )
        single_model = trace_model.Model(
            [
                trace_model.Process(
                    100,
                    "odpm.cm",
                    [trace_model.Thread(101, "t0", [single_event])],
                )
            ],
            {},
        )
        processor = odpm_power.OdpmPowerMetricsProcessor(
            rails=["battery"],
            aggregates_only=True,
        )
        results = processor.process_metrics(single_model)
        _, freeform = processor.process_freeform_metrics(single_model)

        self.assertEqual(len(results), 1)
        self.assertAlmostEqual(results[0].values[0], 2.5)
        self.assertEqual(
            freeform,
            {"battery": {"average": 2.5, "min": 2.5, "max": 2.5}},
        )

    def test_aggregates_only_zero_duration_samples(self) -> None:
        # Verifies arithmetic mean fallback when samples share identical timestamps.
        events: list[trace_model.Event] = [
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "battery_odpm_rail",
                    "ts": 1000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 1000.0},
                }
            ),
            trace_model.CounterEvent.consume_dict(
                {
                    "cat": "power",
                    "name": "battery_odpm_rail",
                    "ts": 1000,
                    "pid": 100,
                    "tid": 101,
                    "args": {"mW": 3000.0},
                }
            ),
        ]
        model = trace_model.Model(
            [
                trace_model.Process(
                    100,
                    "odpm.cm",
                    [trace_model.Thread(101, "t0", events)],
                )
            ],
            {},
        )
        processor = odpm_power.OdpmPowerMetricsProcessor(
            rails=["battery"],
            aggregates_only=True,
        )
        results = processor.process_metrics(model)
        _, freeform = processor.process_freeform_metrics(model)

        self.assertEqual(len(results), 1)
        self.assertAlmostEqual(results[0].values[0], 2.0)
        self.assertEqual(
            freeform,
            {"battery": {"average": 2.0, "min": 1.0, "max": 3.0}},
        )

    def test_aggregates_only_empty_model(self) -> None:
        """Verifies missing rail handling on an empty trace model."""
        processor = odpm_power.OdpmPowerMetricsProcessor(
            rails=["battery"],
            aggregates_only=True,
        )
        empty_model = trace_model.Model([], {})

        self.assertEqual(processor.process_metrics(empty_model), [])
        self.assertEqual(
            processor.process_freeform_metrics(empty_model),
            ("odpm_power", {}),
        )

    def test_freeform_metrics_skips_missing_sum_rails(self) -> None:
        # Verifies process_freeform_metrics skips sum_rails groups with missing rails.
        single_event = trace_model.CounterEvent.consume_dict(
            {
                "cat": "power",
                "name": "battery_odpm_rail",
                "ts": 1000,
                "pid": 100,
                "tid": 101,
                "args": {"mW": 2500.0},
            }
        )
        model = trace_model.Model(
            [
                trace_model.Process(
                    100,
                    "odpm.cm",
                    [trace_model.Thread(101, "t0", [single_event])],
                )
            ],
            {},
        )
        processor = odpm_power.OdpmPowerMetricsProcessor(
            rails=["battery"],
            sum_rails={
                "explicit_group": ["battery", "cpu_big"],
                "predicate_group": lambda r: r != "battery",
            },
            aggregates_only=True,
        )

        self.assertEqual(
            processor.process_freeform_metrics(model),
            (
                "odpm_power",
                {"battery": {"average": 2.5, "min": 2.5, "max": 2.5}},
            ),
        )

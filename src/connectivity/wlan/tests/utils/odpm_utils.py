# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import contextlib
import logging
import os
import pathlib
import weakref
from typing import AsyncGenerator

from honeydew.affordances.tracing import tracing as tracing_affordance
from honeydew.transports.ffx import ffx as ffx_transport
from honeydew.transports.ffx import types as ffx_types
from reporting import metrics
from trace_processing import trace_importing, trace_model
from trace_processing.metrics import odpm_power

_LOGGER = logging.getLogger(__name__)

ODPM_MONIKER = "bootstrap/boot-drivers:odpm-5228000"
ODPM_WLAN_RAIL = "wlan_bt"
ODPM_TRACE_CATEGORIES = ["kernel:meta", "power"]
# Number of hardware polls the ODPM driver accumulates and averages before
# emitting a single data point to Fuchsia trace (`*_odpm_rail` CounterEvent)
# and Inspect. Setting this to 1 emits a trace event on every hardware poll.
DEFAULT_POLLS_PER_SERIES_ENTRY = 1
DEFAULT_POLL_INTERVAL_MS = 100

# Track Tracing instances (one per FuchsiaDevice / test class) that have already
# configured and logged the ODPM polling rate so it only runs the first time it
# is called per test class and skips configuration after that.
_configured_polling_instances: weakref.WeakSet[
    tracing_affordance.Tracing
] = weakref.WeakSet()


def _get_ffx_from_tracing(
    tracing: tracing_affordance.Tracing,
) -> ffx_transport.FFX | None:
    """Extracts the FFX transport from a Honeydew Tracing affordance if available."""
    ffx = getattr(tracing, "_ffx", None)
    if ffx is not None:
        return ffx
    fc_transport = getattr(tracing, "_fc_transport", None)
    dut = getattr(fc_transport, "_device_address_change", None)
    return getattr(dut, "ffx", None)


def _configure_polling_once(
    tracing: tracing_affordance.Tracing,
    polls_per_series_entry: int = DEFAULT_POLLS_PER_SERIES_ENTRY,
    poll_interval_ms: int = DEFAULT_POLL_INTERVAL_MS,
) -> None:
    """Configures and logs the ODPM driver polling rate.

    This will only run the first time it is called per test class, and skip
    configuration after that.
    """
    if tracing in _configured_polling_instances:
        return
    _configured_polling_instances.add(tracing)

    ffx = _get_ffx_from_tracing(tracing)
    if ffx is None:
        return

    cmd = (
        "odpm_util config set "
        f"--polls-per-series-entry {polls_per_series_entry} "
        f"--poll-interval-ms {poll_interval_ms}"
    )
    try:
        output = ffx.run(
            [
                "component",
                "explore",
                ODPM_MONIKER,
                "--command",
                cmd,
            ],
            machine=ffx_types.MachineFormat.RAW,
        )
        _LOGGER.debug(f"ODPM polling configuration:\n{output.strip()}")
    except Exception as e:
        _LOGGER.warning(
            f"Unable to configure ODPM polling on {ODPM_MONIKER}: {e}"
        )


async def start_trace(tracing: tracing_affordance.Tracing) -> None:
    """Starts a background trace session for ODPM."""
    _configure_polling_once(tracing)
    _LOGGER.info("Starting ODPM background trace...")
    try:
        if not tracing.is_session_initialized():
            tracing.initialize(categories=ODPM_TRACE_CATEGORIES)
        if not tracing.is_active():
            await tracing.start()
    except Exception as e:
        _LOGGER.warning(f"Failed to start ODPM background trace: {e}")


async def stop_trace(
    tracing: tracing_affordance.Tracing,
    trace_output_dir: str | os.PathLike[str],
) -> str | None:
    """Stops the ODPM background trace and returns the trace file path."""
    _LOGGER.info("Stopping ODPM background trace...")
    try:
        if tracing.is_active():
            await tracing.stop()
        if tracing.is_session_initialized():
            return await tracing.terminate_and_download(
                directory=str(trace_output_dir),
                trace_file="odpm_trace.fxt",
            )
    except Exception as e:
        _LOGGER.warning(f"Failed to stop ODPM background trace: {e}")
    return None


def get_odpm_measurements(
    trace_path: str | os.PathLike[str],
    rail: str = ODPM_WLAN_RAIL,
) -> list[metrics.TestCaseResult]:
    """Parses an ODPM trace file from a single test run and returns its power and energy metrics.

    Both returned `TestCaseResult` objects (`Power_wlan` and
    `TotalEnergy_Joules_wlan`) are generated together for a single test run.
    In `Power_wlan`, `values` contains the time-series power samples (in Watts)
    collected over that single test run, while `TotalEnergy_Joules_wlan`
    contains a single value for the total energy (in Joules) consumed during
    that same test run.

    Example returned results for a single test:
        [
            metrics.TestCaseResult(
                label="Power_wlan",
                unit=metrics.Unit.watts,
                values=[1.25, 2.40, 1.85, 2.10],
                doc="ODPM power usage samples for rails wlan_bt",
            ),
            metrics.TestCaseResult(
                label="TotalEnergy_Joules_wlan",
                unit=metrics.Unit.count,
                values=[19.0],
                doc="Total energy consumed in Joules (MeanPower * trace duration)",
            ),
        ]
    """
    _LOGGER.info("Collecting ODPM measurements from trace...")
    if not os.path.exists(trace_path):
        _LOGGER.warning(f"Trace file does not exist: {trace_path}")
        return []

    model = trace_importing.create_model_from_trace_file_path(
        trace_path, without_json_conversion=True
    )
    processor = odpm_power.OdpmPowerMetricsProcessor(sum_rails={"wlan": [rail]})
    results = list(processor.process_metrics(model))

    # Calculate trace duration using the first and last ODPM events
    odpm_events = [
        e
        for e in model.all_events()
        if isinstance(e, trace_model.CounterEvent)
        and e.name.endswith("_odpm_rail")
    ]
    timestamps = [
        e.start.to_epoch_delta().to_nanoseconds() for e in odpm_events
    ]
    if timestamps and results:
        duration_sec = (max(timestamps) - min(timestamps)) / 1e9

        power_result = next(
            (r for r in results if r.label == "Power_wlan"),
            None,
        )
        if power_result and power_result.values:
            mean_power_val = sum(power_result.values) / len(power_result.values)
            if mean_power_val > 0.0:
                # Append the calculated total energy (in Joules) to `results`
                # alongside the `Power_wlan` (Watts) metric returned by
                # `processor.process_metrics(model)` above.
                results.append(
                    metrics.TestCaseResult(
                        label="TotalEnergy_Joules_wlan",
                        unit=metrics.Unit.count,
                        values=[mean_power_val * duration_sec],
                        doc="Total energy consumed in Joules (MeanPower * trace duration)",
                    )
                )
    return results


async def stop_trace_and_save_metrics(
    tracing: tracing_affordance.Tracing,
    test_case_path: str | os.PathLike[str],
    test_name: str,
    rail: str = ODPM_WLAN_RAIL,
    save_raw_trace: bool = False,
) -> list[metrics.TestCaseResult]:
    """Stops ODPM background trace, processes metrics, and writes fuchsiaperf JSON."""
    trace_path = await stop_trace(tracing, trace_output_dir=test_case_path)
    if not trace_path:
        return []

    odpm_metrics: list[metrics.TestCaseResult] = []
    try:
        odpm_metrics = get_odpm_measurements(
            trace_path,
            rail=rail,
        )
    except Exception as e:
        _LOGGER.warning(f"Failed to parse ODPM metrics: {e}")
    finally:
        # The temporary "odpm_trace.fxt" file is created uniquely by stop_trace()
        # specifically for ODPM power calculation. Delete it after metrics
        # processing to save disk space on infra test bots.
        if not save_raw_trace and os.path.exists(trace_path):
            try:
                os.remove(trace_path)
            except Exception as e:
                _LOGGER.warning(f"Failed to remove temporary trace file: {e}")

    if odpm_metrics:
        try:
            metrics.TestCaseResult.write_fuchsiaperf_json(
                odpm_metrics,
                test_suite=f"fuchsia.wlan.{test_name}",
                output_path=pathlib.Path(
                    os.path.join(test_case_path, "power_odpm.fuchsiaperf.json")
                ),
            )
        except Exception as e:
            _LOGGER.warning(f"Failed to write fuchsiaperf JSON: {e}")

    return odpm_metrics


@contextlib.asynccontextmanager
async def record_odpm_metrics(
    tracing: tracing_affordance.Tracing,
    test_case_path: str | os.PathLike[str],
    test_name: str,
    rail: str = ODPM_WLAN_RAIL,
    save_raw_trace: bool = False,
) -> AsyncGenerator[None, None]:
    """Context manager that records ODPM trace metrics for a specific block."""
    await start_trace(tracing)
    try:
        yield
    finally:
        await stop_trace_and_save_metrics(
            tracing,
            test_case_path=test_case_path,
            test_name=test_name,
            rail=rail,
            save_raw_trace=save_raw_trace,
        )

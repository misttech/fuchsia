#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from __future__ import annotations

import io
import json
import signal
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr
from pathlib import Path
from typing import Any
from unittest import mock

# Ensure fuchsia root is in sys.path
_FUCHSIA_ROOT = Path(__file__).resolve().parent.parent.parent
if str(_FUCHSIA_ROOT) not in sys.path:
    sys.path.insert(0, str(_FUCHSIA_ROOT))

from build.scripts import graceful_timeout, managed_process

EXIT_TIMEOUT: int = graceful_timeout.EXIT_TIMEOUT
EXIT_USAGE_ERROR: int = graceful_timeout.EXIT_USAGE_ERROR
EXIT_SIGKILL: int = 128 + int(signal.SIGKILL)
EXIT_SIGINT: int = 128 + int(signal.SIGINT)


class FakeManagedProcess:
    """Fake process implementing ManagedProcess interface for deterministic unit tests."""

    def __init__(
        self,
        pid: int = 1234,
        pgid: int = 1234,
        returncode: int | None = 0,
    ) -> None:
        self.pid = pid
        self.pgid = pgid
        self.returncode = returncode
        self.signals: list[tuple[int, managed_process.ProcessTarget]] = []
        self.poll_responses: list[int | None] = []
        self.wait_responses: list[Any] = [0]
        self.wait_calls = 0

    def poll(self) -> int | None:
        if self.poll_responses:
            return self.poll_responses.pop(0)
        return None

    def wait(self, timeout: float | None = None) -> int:
        self.wait_calls += 1
        if self.wait_responses:
            resp = self.wait_responses.pop(0)
            if isinstance(resp, Exception):
                raise resp
            self.poll_responses = [resp]
            return resp
        return self.returncode if self.returncode is not None else 0

    def send_signal(
        self, sig: int, target: managed_process.ProcessTarget
    ) -> None:
        self.signals.append((sig, target))


def make_test_schedule(
    *steps: tuple[str | int, float | str, str],
    final: tuple[str | int, str] = ("SIGKILL", "kill"),
) -> graceful_timeout.Schedule:
    """Convenience factory for building Schedule instances in tests."""
    return graceful_timeout.Schedule(
        steps=[
            graceful_timeout.ScheduleStep(signal=sig, duration=dur, label=lbl)
            for sig, dur, lbl in steps
        ],
        final=graceful_timeout.FinalStep(signal=final[0], label=final[1]),
    )


class FakeClock:
    """A deterministic monotonic clock for simulating time steps."""

    def __init__(self, start: float = 1000.0, step: float = 0.0) -> None:
        self.current = start
        self.step = step

    def __call__(self) -> float:
        val = self.current
        self.current += self.step
        return val


def run_test_scenario(
    schedule: graceful_timeout.Schedule,
    wait_responses: list[Any],
    *,
    hard_timeout: float = 100.0,
    time_step: float = 85.0,
    pid: int = 1234,
    preserve_status: bool = False,
    timeout_exit_code: int = EXIT_TIMEOUT,
) -> tuple[
    int, list[tuple[int, managed_process.ProcessTarget]], list[str], bool
]:
    """Runs a simulated schedule scenario and returns key execution results."""
    last_resp = wait_responses[-1] if wait_responses else 0
    returncode = last_resp if isinstance(last_resp, int) else 0
    proc = FakeManagedProcess(pid=pid, returncode=returncode)
    proc.wait_responses = list(wait_responses)
    mock_diag = mock.Mock()
    hook = graceful_timeout.DiagnosticHook(
        ["pstree", "{pid}", "{label}"], run_fn=mock_diag
    )
    clock = FakeClock(start=1000.0, step=time_step)

    runner = graceful_timeout.GracefulTimeoutRunner(
        proc,
        schedule,
        hard_timeout=hard_timeout,
        diagnostic_hook=hook,
        preserve_status=preserve_status,
        timeout_exit_code=timeout_exit_code,
        time_fn=clock,
    )
    result = runner.run()
    rc = result.returncode
    snapshot_labels = [call.args[0][2] for call in mock_diag.call_args_list]
    return rc, proc.signals, snapshot_labels, runner.schedule_triggered


class ParseDurationTest(unittest.TestCase):
    def test_parse_duration_to_seconds(self) -> None:
        test_cases = [
            ("30", 30.0),
            ("30s", 30.0),
            ("4m", 240.0),
            ("1.5h", 5400.0),
            ("1d", 86400.0),
            (10, 10.0),
            (2.5, 2.5),
        ]
        for val, expected in test_cases:
            with self.subTest(val=val):
                self.assertEqual(
                    graceful_timeout.parse_duration_to_seconds(val), expected
                )

    def test_parse_duration_invalid_raises_value_error(self) -> None:
        invalid_inputs = ["", "invalid", "10xyz", "s", "m", "h", "min"]
        for val in invalid_inputs:
            with self.subTest(val=val):
                with self.assertRaises(ValueError):
                    graceful_timeout.parse_duration_to_seconds(val)


class ParseSignalTest(unittest.TestCase):
    def test_parse_signal(self) -> None:
        test_cases = [
            ("SIGINT", signal.SIGINT),
            ("INT", signal.SIGINT),
            ("SIGKILL", signal.SIGKILL),
            ("TERM", signal.SIGTERM),
            ("2", signal.SIGINT),
            (2, signal.SIGINT),
        ]
        for val, expected in test_cases:
            with self.subTest(val=val):
                self.assertEqual(graceful_timeout.parse_signal(val), expected)

    def test_parse_signal_invalid_raises_value_error(self) -> None:
        invalid_signals = ["INVALID_SIG", "SIG_DFL", "0", 0, 9999, ""]
        for val in invalid_signals:
            with self.subTest(val=val):
                with self.assertRaises(ValueError):
                    graceful_timeout.parse_signal(val)


class PartitionArgsTest(unittest.TestCase):
    def test_partition_args_generator(self) -> None:
        segments = list(
            graceful_timeout.partition_args(
                ["a", "b", "---", "c", "---", "d", "---", "e"],
                separator="---",
                max_splits=2,
            )
        )
        self.assertEqual(segments, [["a", "b"], ["c"], ["d", "---", "e"]])

    def test_partition_args_custom_separator(self) -> None:
        segments = list(
            graceful_timeout.partition_args(
                ["a", "b", "@@", "c", "d", "@@", "e"],
                separator="@@",
            )
        )
        self.assertEqual(segments, [["a", "b"], ["c", "d"], ["e"]])


class ScheduleTest(unittest.TestCase):
    def test_schedule_steps_and_final_from_dict(self) -> None:
        step = graceful_timeout.ScheduleStep.from_dict(
            {
                "signal": "SIGINT",
                "duration": "4m",
                "target": "leader",
                "label": "drain",
            }
        )
        self.assertEqual(step.signal, "SIGINT")
        self.assertEqual(step.signal_enum, signal.SIGINT)
        self.assertEqual(step.duration, "4m")
        self.assertEqual(step.duration_seconds, 240.0)
        self.assertEqual(step.target, managed_process.ProcessTarget.LEADER)
        self.assertEqual(step.label, "drain")

        final = graceful_timeout.FinalStep.from_dict(
            {
                "signal": "SIGKILL",
                "target": "group",
                "label": "kill",
            }
        )
        self.assertEqual(final.signal, "SIGKILL")
        self.assertEqual(final.signal_enum, signal.SIGKILL)
        self.assertEqual(final.target, managed_process.ProcessTarget.GROUP)
        self.assertEqual(final.label, "kill")

    def test_schedule_dataclass_and_json(self) -> None:
        sched_data: graceful_timeout.JSONObject = {
            "steps": [
                {
                    "signal": "SIGINT",
                    "duration": "3m",
                    "target": "leader",
                    "label": "drain",
                },
                {
                    "signal": "SIGINT",
                    "duration": "1m",
                    "target": "leader",
                    "label": "stop",
                },
            ],
            "final": {
                "signal": "SIGKILL",
                "target": "group",
                "label": "kill",
            },
        }
        schedule = graceful_timeout.Schedule.from_dict(sched_data)
        self.assertEqual(len(schedule.steps), 2)
        self.assertEqual(schedule.total_grace_seconds(), 240.0)

        events = schedule.to_events(hard_timeout=600.0, warn_stream=None)
        self.assertEqual(len(events), 3)
        self.assertEqual(events[0].deadline_offset, 360.0)
        self.assertEqual(events[0].signal, signal.SIGINT)
        self.assertEqual(events[0].target, managed_process.ProcessTarget.LEADER)
        self.assertEqual(events[0].label, "drain")

        self.assertEqual(events[1].deadline_offset, 540.0)
        self.assertEqual(events[1].signal, signal.SIGINT)
        self.assertEqual(events[1].target, managed_process.ProcessTarget.LEADER)
        self.assertEqual(events[1].label, "stop")

        self.assertEqual(events[2].deadline_offset, 600.0)
        self.assertEqual(events[2].signal, signal.SIGKILL)
        self.assertEqual(events[2].target, managed_process.ProcessTarget.GROUP)
        self.assertEqual(events[2].label, "kill")

        json_str = json.dumps(sched_data)
        sched_from_json = graceful_timeout.parse_schedule_json(json_str)
        self.assertEqual(len(sched_from_json.steps), 2)
        self.assertEqual(sched_from_json.total_grace_seconds(), 240.0)

    def test_schedule_deserialization_invalid_raises_value_error(self) -> None:
        test_cases = [
            (
                "non-dict schedule",
                lambda: graceful_timeout.Schedule.from_dict("not_a_dict"),
            ),
            (
                "non-dict step",
                lambda: graceful_timeout.ScheduleStep.from_dict("not_a_dict"),
            ),
            (
                "step missing duration and scale",
                lambda: graceful_timeout.ScheduleStep.from_dict(
                    {"signal": "SIGINT"}
                ),
            ),
            (
                "step missing signal",
                lambda: graceful_timeout.ScheduleStep.from_dict(
                    {"duration": "10s"}
                ),
            ),
            (
                "malformed JSON string",
                lambda: graceful_timeout.parse_schedule_json("invalid json"),
            ),
        ]
        for name, parse_fn in test_cases:
            with self.subTest(name=name):
                with self.assertRaises(ValueError):
                    parse_fn()

    def test_schedule_step_scaled_resolution(self) -> None:
        step = graceful_timeout.ScheduleStep(
            signal="SIGINT",
            scale=0.01,
            min_duration="5s",
            max_duration="35s",
            label="drain",
        )
        self.assertEqual(
            step.resolve_duration(hard_timeout=100.0), 5.0
        )  # clamped to min
        self.assertEqual(
            step.resolve_duration(hard_timeout=1000.0), 10.0
        )  # 1000 * 0.01
        self.assertEqual(
            step.resolve_duration(hard_timeout=5000.0), 35.0
        )  # clamped to max
        self.assertEqual(step.resolve_duration(), 5.0)  # fallback to min

    def test_schedule_step_invalid_configuration_raises_value_error(
        self,
    ) -> None:
        test_cases = [
            (
                "both duration and scale specified",
                {
                    "signal": "SIGINT",
                    "duration": "10s",
                    "scale": 0.01,
                    "min_duration": "5s",
                    "max_duration": "35s",
                },
                "cannot specify both",
            ),
            (
                "scale missing max_duration",
                {
                    "signal": "SIGINT",
                    "scale": 0.01,
                    "min_duration": "5s",
                },
                None,
            ),
            (
                "min_duration exceeds max_duration",
                {
                    "signal": "SIGINT",
                    "scale": 0.01,
                    "min_duration": "40s",
                    "max_duration": "10s",
                },
                "cannot exceed max_duration",
            ),
            (
                "scale greater than 1.0",
                {
                    "signal": "SIGINT",
                    "scale": 1.5,
                    "min_duration": "5s",
                    "max_duration": "10s",
                },
                None,
            ),
        ]
        for name, kwargs, expected_msg in test_cases:
            with self.subTest(name=name):
                with self.assertRaises(ValueError) as cm:
                    graceful_timeout.ScheduleStep(**kwargs)
                if expected_msg:
                    self.assertIn(expected_msg, str(cm.exception))

    def test_schedule_step_description(self) -> None:
        step = graceful_timeout.ScheduleStep(
            signal="SIGINT",
            duration="10s",
            label="stop",
            description="Stop inflow of new tasks.",
        )
        self.assertEqual(step.description, "Stop inflow of new tasks.")
        event = step.to_event(100.0)
        self.assertEqual(event.description, "Stop inflow of new tasks.")

        # From dict
        data = {
            "signal": "SIGINT",
            "duration": "10s",
            "label": "cancel",
            "description": "Immediate cancel",
        }
        step2 = graceful_timeout.ScheduleStep.from_dict(data)
        self.assertEqual(step2.description, "Immediate cancel")

    def test_load_schedule_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            sched_data: graceful_timeout.JSONObject = {
                "final": {"signal": "SIGKILL"}
            }
            sched_file = Path(tmpdir) / "schedule.json"
            sched_file.write_text(json.dumps(sched_data), encoding="utf-8")
            sched = graceful_timeout.load_schedule_file(sched_file)
            events = sched.to_events(hard_timeout=10.0)
            self.assertEqual(len(events), 1)
            self.assertEqual(events[0].deadline_offset, 10.0)
            self.assertEqual(events[0].signal, signal.SIGKILL)

    def test_to_events_none_hard_timeout(self) -> None:
        schedule = graceful_timeout.Schedule(
            steps=[
                graceful_timeout.ScheduleStep(signal="SIGINT", duration="10s")
            ],
            final=graceful_timeout.FinalStep(signal="SIGKILL"),
        )
        events = schedule.to_events(hard_timeout=None)
        self.assertEqual(len(events), 2)
        self.assertEqual(events[0].deadline_offset, 0.0)
        self.assertEqual(events[0].signal, signal.SIGINT)
        self.assertEqual(events[1].deadline_offset, 10.0)
        self.assertEqual(events[1].signal, signal.SIGKILL)

    def test_to_events_hard_timeout_less_than_or_equal_grace_raises_value_error(
        self,
    ) -> None:
        schedule = graceful_timeout.Schedule(
            steps=[
                graceful_timeout.ScheduleStep(signal="SIGINT", duration="10s"),
            ],
            final=graceful_timeout.FinalStep(signal="SIGKILL"),
        )
        self.assertEqual(schedule.total_grace_seconds(), 10.0)

        with self.assertRaises(ValueError) as cm:
            schedule.to_events(hard_timeout=10.0)
        self.assertIn("must be strictly greater", str(cm.exception))

        with self.assertRaises(ValueError) as cm:
            schedule.to_events(hard_timeout=5.0)
        self.assertIn("must be strictly greater", str(cm.exception))

    def test_to_events_warning_on_high_grace_fraction(self) -> None:
        schedule = graceful_timeout.Schedule(
            steps=[
                graceful_timeout.ScheduleStep(signal="SIGINT", duration="30s"),
            ],
            final=graceful_timeout.FinalStep(signal="SIGKILL"),
        )
        # 30s grace > 25% of 100s -> emits warning to warn_stream
        warn_buf = io.StringIO()
        schedule.to_events(hard_timeout=100.0, warn_stream=warn_buf)
        self.assertIn("Warning: schedule grace period", warn_buf.getvalue())
        self.assertIn("exceeds 25%", warn_buf.getvalue())

        # 30s grace <= 25% of 120s -> no warning
        quiet_buf = io.StringIO()
        schedule.to_events(hard_timeout=120.0, warn_stream=quiet_buf)
        self.assertEqual(quiet_buf.getvalue(), "")

    def test_to_events_with_explicit_grace_period_proportional_scaling(
        self,
    ) -> None:
        schedule = graceful_timeout.Schedule(
            steps=[
                graceful_timeout.ScheduleStep(
                    signal="SIGINT",
                    scale=0.01,
                    min_duration="10s",
                    max_duration="30s",
                    label="stop",
                ),
                graceful_timeout.ScheduleStep(
                    signal="SIGINT",
                    scale=0.01,
                    min_duration="10s",
                    max_duration="30s",
                    label="cancel",
                ),
            ],
            final=graceful_timeout.FinalStep(signal="SIGKILL"),
        )
        self.assertEqual(schedule.min_total_grace_seconds, 20.0)
        self.assertEqual(schedule.max_total_grace_seconds, 60.0)

        # Allocate exactly 40s (halfway between 20s and 60s -> each step becomes 20s)
        events = schedule.to_events(hard_timeout=100.0, grace_period=40.0)
        self.assertEqual(len(events), 3)
        self.assertEqual(events[0].deadline_offset, 60.0)  # 100 - 40
        self.assertEqual(events[1].deadline_offset, 80.0)  # 60 + 20
        self.assertEqual(events[2].deadline_offset, 100.0)  # final kill

    def test_to_events_grace_period_less_than_min_raises_value_error(
        self,
    ) -> None:
        schedule = graceful_timeout.Schedule(
            steps=[
                graceful_timeout.ScheduleStep(
                    signal="SIGINT",
                    scale=0.01,
                    min_duration="15s",
                    max_duration="45s",
                ),
            ],
            final=graceful_timeout.FinalStep(signal="SIGKILL"),
        )
        with self.assertRaises(ValueError) as cm:
            schedule.to_events(hard_timeout=100.0, grace_period=10.0)
        self.assertIn("cannot be less than", str(cm.exception))

    def test_to_events_grace_period_exceeds_max_clamped(self) -> None:
        schedule = graceful_timeout.Schedule(
            steps=[
                graceful_timeout.ScheduleStep(
                    signal="SIGINT",
                    scale=0.01,
                    min_duration="10s",
                    max_duration="20s",
                ),
            ],
            final=graceful_timeout.FinalStep(signal="SIGKILL"),
        )
        # 50s exceeds max 20s -> clamped to 20s
        events = schedule.to_events(hard_timeout=100.0, grace_period=50.0)
        self.assertEqual(events[0].deadline_offset, 80.0)  # 100 - 20
        self.assertEqual(events[1].deadline_offset, 100.0)


class DiagnosticHookTest(unittest.TestCase):
    def test_diagnostic_hook_template_expansion_and_idempotence(self) -> None:
        proc = FakeManagedProcess(pid=999, pgid=1234)
        mock_run = mock.Mock()
        hook = graceful_timeout.DiagnosticHook(
            ["echo", "{pid}", "{pgid}", "{label}"],
            diag_margin=5.0,
            run_fn=mock_run,
        )
        hook.run(proc, label="stop")
        mock_run.assert_called_once_with(
            ["echo", "999", "1234", "stop"],
            stdout=sys.stderr,
            stderr=sys.stderr,
            timeout=5.0,
        )
        # Verify idempotence for the same label
        hook.run(proc, label="stop")
        self.assertEqual(mock_run.call_count, 1)

        # Runs for a new label
        hook.run(proc, label="kill")
        self.assertEqual(mock_run.call_count, 2)
        self.assertEqual(
            mock_run.call_args[0][0],
            ["echo", "999", "1234", "kill"],
        )


class GracefulTimeoutRunnerTest(unittest.TestCase):
    def test_runner_direct_execution(self) -> None:
        process = managed_process.ManagedProcess.spawn(
            [sys.executable, "-c", "pass"]
        )
        schedule = graceful_timeout.Schedule(
            final=graceful_timeout.FinalStep(signal=signal.SIGKILL)
        )
        runner = graceful_timeout.GracefulTimeoutRunner(
            process, schedule, hard_timeout=10.0
        )
        self.assertEqual(runner.run().returncode, 0)

    def test_runner_normal_exit_zero(self) -> None:
        proc = FakeManagedProcess(returncode=0)
        proc.poll_responses = [None, 0]
        runner = graceful_timeout.GracefulTimeoutRunner(
            proc, graceful_timeout.Schedule()
        )
        result = runner.run()
        self.assertEqual(result.returncode, 0)
        self.assertFalse(runner.schedule_triggered)

    def test_runner_normal_exit_nonzero(self) -> None:
        proc = FakeManagedProcess(returncode=42)
        proc.poll_responses = [None, 42]
        runner = graceful_timeout.GracefulTimeoutRunner(
            proc, graceful_timeout.Schedule()
        )
        result = runner.run()
        self.assertEqual(result.returncode, 42)
        self.assertFalse(runner.schedule_triggered)

    def test_runner_timeout_schedule_escalation_with_snapshot(self) -> None:
        schedule = make_test_schedule(("SIGINT", 10.0, "stop"))
        wait_responses = [
            subprocess.TimeoutExpired(cmd="test", timeout=40.0),
            subprocess.TimeoutExpired(cmd="test", timeout=50.0),
            137,
        ]
        rc, signals, snapshots, triggered = run_test_scenario(
            schedule, wait_responses, hard_timeout=50.0, time_step=45.0
        )

        self.assertEqual(rc, EXIT_TIMEOUT)
        self.assertTrue(triggered)
        self.assertEqual(
            signals,
            [
                (signal.SIGINT, managed_process.ProcessTarget.LEADER),
                (signal.SIGKILL, managed_process.ProcessTarget.GROUP),
            ],
        )
        self.assertEqual(snapshots, ["stop", "kill"])

    def test_runner_snapshots_only_at_first_and_final_events(self) -> None:
        schedule = make_test_schedule(
            ("SIGINT", 10.0, "stop"),
            ("SIGINT", 10.0, "cancel"),
        )
        wait_responses = [
            subprocess.TimeoutExpired(cmd="test", timeout=80.0),
            subprocess.TimeoutExpired(cmd="test", timeout=90.0),
            subprocess.TimeoutExpired(cmd="test", timeout=100.0),
            EXIT_SIGKILL,
        ]
        rc, _, snapshots, triggered = run_test_scenario(
            schedule, wait_responses, hard_timeout=100.0, time_step=85.0
        )

        self.assertEqual(rc, EXIT_TIMEOUT)
        self.assertTrue(triggered)
        # Snapshots only ran for first (stop) and final (kill), skipping intermediate (cancel)
        self.assertEqual(snapshots, ["stop", "kill"])

    def test_runner_exits_during_intermediate_phase(self) -> None:
        """Verify runner halts escalation when process terminates in an intermediate phase."""
        schedule = make_test_schedule(
            ("SIGINT", 10.0, "stop"),
            ("SIGINT", 10.0, "cancel"),
        )
        wait_responses = [
            subprocess.TimeoutExpired(cmd="test", timeout=80.0),
            0,
        ]
        rc, signals, snapshots, triggered = run_test_scenario(
            schedule, wait_responses, hard_timeout=100.0, time_step=85.0
        )

        self.assertEqual(rc, EXIT_TIMEOUT)
        self.assertTrue(triggered)
        # Only the first signal was sent; escalation halted when child exited
        self.assertEqual(
            signals,
            [(signal.SIGINT, managed_process.ProcessTarget.LEADER)],
        )
        # Only the start snapshot ran, final snapshot was bypassed
        self.assertEqual(snapshots, ["stop"])

    def test_runner_exits_during_intermediate_phase_preserve_status(
        self,
    ) -> None:
        """Verify runner preserves child exit code when process exits in an intermediate phase."""
        schedule = make_test_schedule(
            ("SIGINT", 10.0, "stop"),
            ("SIGINT", 10.0, "cancel"),
        )
        wait_responses = [
            subprocess.TimeoutExpired(cmd="test", timeout=80.0),
            EXIT_SIGINT,
        ]
        rc, signals, _, triggered = run_test_scenario(
            schedule,
            wait_responses,
            hard_timeout=100.0,
            time_step=85.0,
            preserve_status=True,
        )

        self.assertEqual(rc, EXIT_SIGINT)
        self.assertTrue(triggered)
        self.assertEqual(
            signals,
            [(signal.SIGINT, managed_process.ProcessTarget.LEADER)],
        )

    def test_runner_timeout_preserve_status(self) -> None:
        proc = FakeManagedProcess(returncode=EXIT_SIGINT)
        proc.poll_responses = [None, None, EXIT_SIGINT]
        proc.wait_responses = [
            subprocess.TimeoutExpired(cmd="test", timeout=10.0),
            EXIT_SIGINT,
        ]
        schedule = graceful_timeout.Schedule(
            final=graceful_timeout.FinalStep(signal=signal.SIGINT)
        )

        runner = graceful_timeout.GracefulTimeoutRunner(
            proc,
            schedule,
            hard_timeout=10.0,
            timeout_exit_code=EXIT_TIMEOUT,
            preserve_status=True,
        )
        result = runner.run()
        self.assertEqual(result.returncode, EXIT_SIGINT)
        self.assertTrue(result.timed_out)

    def test_runner_external_sigint_handling(self) -> None:
        proc = FakeManagedProcess(pid=4321, returncode=EXIT_SIGINT)
        proc.poll_responses = [None, None, EXIT_SIGINT]
        proc.wait_responses = [
            graceful_timeout._SignalInterruption(signal.SIGINT),
            EXIT_SIGINT,
        ]
        schedule = graceful_timeout.Schedule(
            steps=[
                graceful_timeout.ScheduleStep(
                    signal=signal.SIGINT, duration=10.0
                )
            ],
            final=graceful_timeout.FinalStep(signal=signal.SIGKILL),
        )

        runner = graceful_timeout.GracefulTimeoutRunner(
            proc,
            schedule,
            hard_timeout=50.0,
            timeout_exit_code=EXIT_TIMEOUT,
        )

        result = runner.run()

        self.assertEqual(result.returncode, EXIT_SIGINT)
        self.assertTrue(runner.schedule_triggered)
        self.assertEqual(result.interrupted_signal, signal.SIGINT)
        self.assertFalse(result.timed_out)
        self.assertEqual(
            proc.signals,
            [(signal.SIGINT, managed_process.ProcessTarget.LEADER)],
        )

    def test_runner_external_sigterm_handling(self) -> None:
        proc = FakeManagedProcess(pid=4321, returncode=143)
        proc.poll_responses = [None, None, 143]
        proc.wait_responses = [
            graceful_timeout._SignalInterruption(signal.SIGTERM),
            143,
        ]
        schedule = graceful_timeout.Schedule(
            final=graceful_timeout.FinalStep(
                signal=signal.SIGTERM,
                target=managed_process.ProcessTarget.GROUP,
            )
        )

        runner = graceful_timeout.GracefulTimeoutRunner(
            proc,
            schedule,
            hard_timeout=10.0,
            timeout_exit_code=124,
        )

        result = runner.run()

        self.assertEqual(result.returncode, 143)
        self.assertTrue(runner.schedule_triggered)
        self.assertEqual(result.interrupted_signal, signal.SIGTERM)
        self.assertFalse(result.timed_out)
        self.assertEqual(
            proc.signals,
            [(signal.SIGTERM, managed_process.ProcessTarget.GROUP)],
        )

    def test_step_context_passed_to_hook(self) -> None:
        received_contexts: list[graceful_timeout.StepContext] = []

        def test_hook(ctx: graceful_timeout.StepContext) -> None:
            received_contexts.append(ctx)

        proc = FakeManagedProcess(pid=9999, returncode=0)
        proc.poll_responses = [None, 0]
        proc.wait_responses = [
            subprocess.TimeoutExpired(cmd="test", timeout=10.0),
            0,
        ]
        schedule = graceful_timeout.Schedule(
            final=graceful_timeout.FinalStep(
                signal=signal.SIGKILL, label="terminal_kill"
            )
        )
        runner = graceful_timeout.GracefulTimeoutRunner(
            proc,
            schedule,
            hard_timeout=10.0,
            diagnostic_hook=test_hook,
        )
        runner.run()
        self.assertEqual(len(received_contexts), 1)
        self.assertEqual(received_contexts[0].label, "terminal_kill")
        self.assertEqual(received_contexts[0].pid, 9999)

    def test_negative_returncode_converted_to_posix(self) -> None:
        proc = FakeManagedProcess(pid=1234, returncode=-9)  # SIGKILL from OS
        proc.poll_responses = [None, -9]
        schedule = graceful_timeout.Schedule()
        runner = graceful_timeout.GracefulTimeoutRunner(proc, schedule)
        result = runner.run()
        self.assertEqual(result.returncode, 137)  # 128 - (-9) = 137

    def test_signal_initiated_escalation_without_hard_timeout(self) -> None:
        proc = FakeManagedProcess(pid=5555, returncode=0)
        proc.poll_responses = [None, None, 0]
        proc.wait_responses = [
            graceful_timeout._SignalInterruption(signal.SIGINT),
            0,
        ]
        schedule = graceful_timeout.Schedule(
            steps=[
                graceful_timeout.ScheduleStep(
                    signal=signal.SIGINT,
                    scale=0.1,
                    min_duration="15s",
                    max_duration="45s",
                    label="drain",
                )
            ],
            final=graceful_timeout.FinalStep(signal=signal.SIGKILL),
        )
        # Without hard_timeout, an external signal initiates the schedule
        runner = graceful_timeout.GracefulTimeoutRunner(
            proc,
            schedule,
            hard_timeout=None,
        )
        result = runner.run()
        self.assertEqual(result.returncode, EXIT_SIGINT)
        self.assertEqual(result.interrupted_signal, signal.SIGINT)
        self.assertFalse(result.timed_out)
        self.assertEqual(
            proc.signals,
            [(signal.SIGINT, managed_process.ProcessTarget.LEADER)],
        )


class MainCommandSectionsTest(unittest.TestCase):
    def test_main_command_sections_from_argv(self) -> None:
        test_cases = [
            (
                "snapshot and wrapped commands",
                [
                    "--timeout",
                    "10s",
                    "---",
                    "pstree",
                    "{pid}",
                    "---",
                    "ninja",
                    "-C",
                    "out",
                ],
                graceful_timeout.MainCommandSections(
                    options=["--timeout", "10s"],
                    snapshot_command=["pstree", "{pid}"],
                    wrapped_command=["ninja", "-C", "out"],
                ),
            ),
            (
                "wrapped command only",
                ["--timeout", "10s", "---", "ninja", "-C", "out"],
                graceful_timeout.MainCommandSections(
                    options=["--timeout", "10s"],
                    snapshot_command=[],
                    wrapped_command=["ninja", "-C", "out"],
                ),
            ),
            (
                "wrapped command with internal double-dash",
                ["--timeout", "10s", "---", "cargo", "test", "--", "filter"],
                graceful_timeout.MainCommandSections(
                    options=["--timeout", "10s"],
                    snapshot_command=[],
                    wrapped_command=["cargo", "test", "--", "filter"],
                ),
            ),
            (
                "options only",
                ["--timeout", "10s"],
                graceful_timeout.MainCommandSections(
                    options=["--timeout", "10s"],
                    snapshot_command=[],
                    wrapped_command=[],
                ),
            ),
        ]
        for name, argv, expected in test_cases:
            with self.subTest(name=name):
                sections = graceful_timeout.MainCommandSections.from_argv(argv)
                self.assertEqual(sections, expected)

    def test_from_argv_preserves_separator_in_wrapped_command(self) -> None:
        sections = graceful_timeout.MainCommandSections.from_argv(
            [
                "--timeout",
                "10s",
                "---",
                "pstree",
                "{pid}",
                "---",
                "ninja",
                "---",
                "target",
            ]
        )
        self.assertEqual(sections.options, ["--timeout", "10s"])
        self.assertEqual(sections.snapshot_command, ["pstree", "{pid}"])
        self.assertEqual(sections.wrapped_command, ["ninja", "---", "target"])

    def test_create_runner_validates_timeout_early(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            sched_path = Path(tmpdir) / "sched.json"
            sched_path.write_text(
                json.dumps(
                    {
                        "steps": [{"signal": "SIGINT", "duration": "20s"}],
                        "final": {"signal": "SIGKILL"},
                    }
                ),
                encoding="utf-8",
            )
            sections = graceful_timeout.MainCommandSections(
                options=[
                    "--schedule-file",
                    str(sched_path),
                    "--timeout",
                    "10s",
                ],
                wrapped_command=["true"],
            )
            with self.assertRaises(ValueError) as cm:
                sections.create_runner()
            self.assertIn("must be strictly greater", str(cm.exception))


class MainCliTest(unittest.TestCase):
    def test_main_runs_runner(self) -> None:
        """Verify main() parses real args, loads schedule, creates runner, and executes."""
        with tempfile.TemporaryDirectory() as tmpdir:
            sched_path = Path(tmpdir) / "sched.json"
            sched_path.write_text(
                json.dumps({"final": {"signal": "SIGKILL"}}), encoding="utf-8"
            )

            mock_proc = mock.Mock(spec=managed_process.ManagedProcess)
            mock_proc.poll.return_value = 42
            mock_proc.returncode = 42
            mock_proc.wait.return_value = 42

            with mock.patch.object(
                managed_process.ManagedProcess,
                "spawn",
                return_value=mock_proc,
            ) as mock_spawn:
                ret = graceful_timeout.main(
                    [
                        "--schedule-file",
                        str(sched_path),
                        "---",
                        "echo",
                        "hello",
                    ]
                )

            self.assertEqual(ret, 42)
            mock_spawn.assert_called_once_with(["echo", "hello"])

    @mock.patch.object(
        graceful_timeout.MainCommandSections,
        "from_argv",
        side_effect=ValueError("bad sections"),
    )
    def test_main_handles_partition_error(
        self, mock_from_argv: mock.Mock
    ) -> None:
        stderr_buf = io.StringIO()
        with redirect_stderr(stderr_buf):
            ret = graceful_timeout.main(["---", "---"])
        self.assertEqual(ret, EXIT_USAGE_ERROR)
        self.assertIn("bad sections", stderr_buf.getvalue())


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Progressive process timeout scheduler with staged signal escalation.

Dual-use: importable library and standalone CLI.
Supports selective signal targets (leader vs group), diagnostic snapshot command
execution at milestone events (initial stop and pre-kill fallback), configurable
exit codes, and external signal handling.

CLI Usage:
    graceful_timeout.py --schedule-file <path> [options] --- <snapshot_cmd...> --- <wrapped_cmd...>
    graceful_timeout.py --schedule-file <path> [options] --- <wrapped_cmd...>
"""

import argparse
import json
import os
import signal
import subprocess
import sys
import time
from collections.abc import Callable, Iterator, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from signal import Signals
from typing import Any, Self, TextIO

# Ensure the fuchsia root is in sys.path so we can import build python modules
_FUCHSIA_DIR = Path(__file__).resolve().parent.parent.parent
if str(_FUCHSIA_DIR) not in sys.path:
    sys.path.insert(0, str(_FUCHSIA_DIR))

from build.python.modules.serialization import serialization
from build.scripts import managed_process, signal_utils

_SCRIPT: Path = Path(__file__)


def msg(text: str, file: TextIO | None = None) -> None:
    """Print a message prefixed with the script's basename."""
    if file is None:
        file = sys.stderr
    print(f"[{_SCRIPT.name}] {text}", file=file)


SUBCOMMAND_SEPARATOR = "---"

# Standard process exit codes
EXIT_TIMEOUT: int = 124  # Standard GNU timeout exit code
EXIT_USAGE_ERROR: int = 2  # Standard command line usage/syntax error

# JSON type aliases
JSONPrimitive = str | int | float | bool | None
JSONValue = JSONPrimitive | dict[str, Any] | list[Any]
JSONObject = dict[str, JSONValue]
JSONArray = list[JSONValue]


@dataclass(frozen=True)
class StepContext:
    """Runtime context passed to milestone diagnostic hooks."""

    label: str
    pid: int
    pgid: int | None = None
    signal: Signals | int | None = None


StepHook = Callable[[StepContext], None]


@dataclass(frozen=True)
class TimeoutResult:
    """Result of running a command under ScheduledTimeoutRunner."""

    returncode: int
    timed_out: bool = False
    interrupted_signal: int | None = None


class _SignalInterruption(Exception):
    """Internal exception raised by signal handlers to abort wait loops.

    Under PEP 475, Python's waitpid restarts on EINTR unless the signal handler
    raises an exception. This exception breaks the wait loop immediately.
    """

    def __init__(self, signum: int) -> None:
        super().__init__(f"Signal {signum} received")
        self.signum = signum


@dataclass(frozen=True)
class SignalEvent:
    """A scheduled signal dispatch along the timeout timeline."""

    deadline_offset: float
    signal: Signals | int
    target: managed_process.ProcessTarget = managed_process.ProcessTarget.LEADER
    label: str = ""
    description: str = ""


def parse_duration_to_seconds(val: str | int | float) -> float:
    """Parses human duration strings like '90m', '4m', '10s', '1.5h', '3600' or numbers."""
    if isinstance(val, (int, float)):
        return float(val)
    val = str(val).strip().lower()
    if not val:
        raise ValueError("Empty duration")
    multipliers = {
        "s": 1.0,
        "sec": 1.0,
        "seconds": 1.0,
        "m": 60.0,
        "min": 60.0,
        "minutes": 60.0,
        "h": 3600.0,
        "hr": 3600.0,
        "hours": 3600.0,
        "d": 86400.0,
        "days": 86400.0,
    }
    for unit, factor in sorted(multipliers.items(), key=lambda x: -len(x[0])):
        if val.endswith(unit):
            num_part = val[: -len(unit)].strip()
            if not num_part:
                raise ValueError(f"Invalid duration '{val}': missing number")
            return float(num_part) * factor
    return float(val)


def parse_signal(sig_val: str | int) -> Signals:
    """Parses a signal specification (e.g. 'SIGINT', 'INT', '2', 2) into Signals."""
    if isinstance(sig_val, int):
        return Signals(sig_val)
    sig_str = str(sig_val).strip()
    if sig_str.isdigit():
        return Signals(int(sig_str))
    name = sig_str.upper()
    if not name.startswith("SIG"):
        name = "SIG" + name
    try:
        return Signals[name]
    except KeyError:
        raise ValueError(f"Unknown signal: {sig_val}") from None


@dataclass(frozen=True)
class BaseStep:
    """Base class for schedule steps specifying a signal, target, and label."""

    signal: int | str
    target: managed_process.ProcessTarget = managed_process.ProcessTarget.LEADER
    label: str = ""
    description: str = ""

    @property
    def signal_enum(self) -> Signals:
        """Returns the parsed Signals enum."""
        return parse_signal(self.signal)

    def to_event(self, offset: float) -> SignalEvent:
        """Converts this step into a SignalEvent at the given deadline offset."""
        return SignalEvent(
            deadline_offset=offset,
            signal=self.signal_enum,
            target=self.target,
            label=self.label,
            description=self.description,
        )


@dataclass(frozen=True)
class FinalStep(BaseStep):
    """The terminal step in a schedule (defaults to SIGKILL to process group)."""

    signal: int | str = "SIGKILL"
    target: managed_process.ProcessTarget = managed_process.ProcessTarget.GROUP
    label: str = "kill"

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> Self:
        """Parses a final step dictionary into a FinalStep using serialization."""
        if not isinstance(data, dict):
            raise ValueError(
                f"Expected final step object, got {type(data).__name__}"
            )
        try:
            return serialization.instance_from_dict(cls, data)
        except (TypeError, KeyError, ValueError) as e:
            raise ValueError(f"Failed to parse FinalStep: {e}") from e


@dataclass(frozen=True)
class ScheduleStep(BaseStep):
    """An intermediate escalation step.

    Must be configured in one of two modes:
      1. Fixed duration: specifies 'duration' (e.g. '10s' or 10.0).
      2. Scaled duration: specifies 'scale' (fraction of hard_timeout),
         'min_duration', and 'max_duration'.
    """

    duration: int | float | str | None = None
    scale: float | None = None
    min_duration: int | float | str | None = None
    max_duration: int | float | str | None = None

    def __post_init__(self) -> None:
        has_fixed = self.duration is not None
        has_scaled = (
            self.scale is not None
            or self.min_duration is not None
            or self.max_duration is not None
        )
        if has_fixed and has_scaled:
            raise ValueError(
                f"Step '{self.label}' cannot specify both 'duration' and scaled options (scale/min_duration/max_duration)."
            )
        if not has_fixed and not has_scaled:
            raise ValueError(
                f"Step '{self.label}' must specify either 'duration' or ('scale', 'min_duration', 'max_duration')."
            )
        if has_scaled:
            if self.scale is None:
                raise ValueError(f"Scaled step '{self.label}' missing 'scale'.")
            if not (0.0 < self.scale < 1.0):
                raise ValueError(
                    f"Scaled step '{self.label}' scale must be between 0 and 1, got {self.scale}."
                )
            if self.min_duration is None or self.max_duration is None:
                raise ValueError(
                    f"Scaled step '{self.label}' must specify both 'min_duration' and 'max_duration'."
                )
            min_sec = parse_duration_to_seconds(self.min_duration)
            max_sec = parse_duration_to_seconds(self.max_duration)
            if min_sec < 0:
                raise ValueError(
                    f"Step '{self.label}' min_duration cannot be negative."
                )
            if min_sec > max_sec:
                raise ValueError(
                    f"Step '{self.label}' min_duration ({min_sec:g}s) cannot exceed max_duration ({max_sec:g}s)."
                )
        else:
            if self.duration is None:
                raise ValueError(
                    f"Step '{self.label}' must specify either duration or scale."
                )
            if parse_duration_to_seconds(self.duration) < 0:
                raise ValueError(
                    f"Step '{self.label}' duration cannot be negative."
                )

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> Self:
        """Parses a step dictionary into a ScheduleStep using serialization."""
        if not isinstance(data, dict):
            raise ValueError(f"Expected step object, got {type(data).__name__}")
        if "signal" not in data:
            raise ValueError("Step missing required 'signal' field")
        try:
            return serialization.instance_from_dict(cls, data)
        except (TypeError, KeyError, ValueError) as e:
            raise ValueError(f"Failed to parse ScheduleStep: {e}") from e

    @property
    def duration_seconds(self) -> float:
        """Returns the resolved duration in seconds (falling back to min_duration if scaled)."""
        return self.resolve_duration()

    def resolve_duration(self, hard_timeout: float | None = None) -> float:
        """Calculates effective duration in seconds, scaling with hard_timeout if configured."""
        if self.scale is not None:
            assert (
                self.min_duration is not None and self.max_duration is not None
            )
            min_sec = parse_duration_to_seconds(self.min_duration)
            max_sec = parse_duration_to_seconds(self.max_duration)
            if hard_timeout is not None:
                scaled = hard_timeout * self.scale
                return min(max(scaled, min_sec), max_sec)
            return min_sec
        assert self.duration is not None
        return parse_duration_to_seconds(self.duration)


@dataclass(frozen=True)
class Schedule:
    """Represents a structured escalation schedule with steps and a final event."""

    steps: list[ScheduleStep] = field(default_factory=list)
    final: FinalStep = field(default_factory=FinalStep)

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> Self:
        """Instantiates a Schedule from a JSON object using serialization."""
        if not isinstance(data, dict):
            raise ValueError(
                f"Expected schedule object, got {type(data).__name__}"
            )
        try:
            return serialization.instance_from_dict(cls, data)
        except (TypeError, KeyError, ValueError) as e:
            raise ValueError(f"Failed to deserialize schedule: {e}") from e

    @property
    def min_total_grace_seconds(self) -> float:
        """Returns the minimum possible grace period (sum of min durations)."""
        total = 0.0
        for s in self.steps:
            if s.min_duration is not None:
                total += parse_duration_to_seconds(s.min_duration)
            elif s.duration is not None:
                total += parse_duration_to_seconds(s.duration)
        return total

    @property
    def max_total_grace_seconds(self) -> float:
        """Returns the maximum possible grace period (sum of max durations)."""
        total = 0.0
        for s in self.steps:
            if s.max_duration is not None:
                total += parse_duration_to_seconds(s.max_duration)
            elif s.duration is not None:
                total += parse_duration_to_seconds(s.duration)
        return total

    def total_grace_seconds(self, hard_timeout: float | None = None) -> float:
        """Returns the sum of all intermediate step durations, resolved for hard_timeout."""
        return sum(s.resolve_duration(hard_timeout) for s in self.steps)

    def to_events(
        self,
        hard_timeout: float | None = None,
        grace_period: float | None = None,
        warn_stream: TextIO | None = sys.stderr,
    ) -> list[SignalEvent]:
        """Resolves the schedule into an ordered timeline of SignalEvents.

        Args:
            hard_timeout: Optional hard timeout budget in seconds.
            grace_period: Optional explicit total grace period in seconds. When
                unspecified, the grace period is dynamically calculated from the
                schedule steps (either using fixed step durations or scaling with
                hard_timeout within each step's min_duration and max_duration bounds).
                When specified, intermediate step durations are scaled proportionally
                to fit the requested grace period, clamped to the schedule's maximum
                possible grace period (max_total_grace_seconds).
            warn_stream: Stream to write configuration warnings to (defaults to sys.stderr).

        Raises:
            ValueError: If hard_timeout is less than or equal to total_grace_seconds,
                or if requested grace_period is less than the schedule's minimum
                required grace period (min_total_grace_seconds).
        """
        if grace_period is not None:
            grace_sec = float(grace_period)
            min_grace = self.min_total_grace_seconds
            max_grace = self.max_total_grace_seconds
            if grace_sec < min_grace:
                raise ValueError(
                    f"Requested grace period ({grace_sec:g}s) cannot be less than "
                    f"the schedule's minimum required grace period ({min_grace:g}s)."
                )
            effective_grace = min(grace_sec, max_grace)

            step_durations: list[float] = []
            if max_grace > min_grace:
                alpha = (effective_grace - min_grace) / (max_grace - min_grace)
                for s in self.steps:
                    if s.min_duration is not None:
                        s_min = parse_duration_to_seconds(s.min_duration)
                    elif s.duration is not None:
                        s_min = parse_duration_to_seconds(s.duration)
                    else:
                        s_min = 0.0

                    if s.max_duration is not None:
                        s_max = parse_duration_to_seconds(s.max_duration)
                    elif s.duration is not None:
                        s_max = parse_duration_to_seconds(s.duration)
                    else:
                        s_max = 0.0

                    step_durations.append(s_min + alpha * (s_max - s_min))
            else:
                step_durations = [s.resolve_duration() for s in self.steps]
            total_grace = effective_grace
        else:
            total_grace = self.total_grace_seconds(hard_timeout)
            step_durations = [
                s.resolve_duration(hard_timeout) for s in self.steps
            ]

        if hard_timeout is not None:
            if hard_timeout <= total_grace:
                raise ValueError(
                    f"Hard timeout ({hard_timeout:g}s) must be strictly greater than "
                    f"the schedule's total grace period ({total_grace:g}s)."
                )
            if warn_stream is not None and total_grace > 0.25 * hard_timeout:
                normal_time = hard_timeout - total_grace
                msg(
                    f"Warning: schedule grace period ({total_grace:g}s) exceeds 25% "
                    f"of hard timeout ({hard_timeout:g}s). Process will only have "
                    f"{normal_time:g}s of normal execution time.",
                    file=warn_stream,
                )

        events: list[SignalEvent] = []
        if hard_timeout is None:
            # Without a hard timeout, resolve step offsets starting from 0.0
            curr = 0.0
            for s, duration in zip(self.steps, step_durations):
                events.append(s.to_event(curr))
                curr += duration
            events.append(self.final.to_event(curr))
            return events

        curr = hard_timeout - total_grace
        for s, duration in zip(self.steps, step_durations):
            events.append(s.to_event(curr))
            curr += duration

        events.append(self.final.to_event(hard_timeout))
        return events


def parse_schedule_json(content: str) -> Schedule:
    """Parses a JSON string into a Schedule object using serialization."""
    try:
        return serialization.json_loads(Schedule, content)
    except (TypeError, KeyError, ValueError, json.JSONDecodeError) as e:
        raise ValueError(f"Failed to parse schedule JSON: {e}") from e


def load_schedule_file(path: Path) -> Schedule:
    """Loads a Schedule from a JSON file using serialization."""
    try:
        with open(path, "r", encoding="utf-8") as f:
            return serialization.json_load(Schedule, f)
    except (TypeError, KeyError, ValueError, json.JSONDecodeError) as e:
        raise ValueError(f"Failed to load schedule from {path}: {e}") from e


class DiagnosticHook:
    """Executes a diagnostic command at schedule milestone steps."""

    def __init__(
        self,
        command: Sequence[str],
        diag_margin: float = 10.0,
        run_fn: Callable[..., Any] = subprocess.run,
        verbose: bool = False,
    ) -> None:
        self.command = list(command)
        self.diag_margin = diag_margin
        self._run_fn = run_fn
        self.verbose = verbose
        self._executed_labels: set[str] = set()

    def _log(self, message: str) -> None:
        if self.verbose:
            msg(message)

    def __call__(self, ctx: StepContext) -> None:
        """Executes the diagnostic hook command for the given StepContext."""
        if ctx.label in self._executed_labels or not self.command:
            return

        self._executed_labels.add(ctx.label)
        pgid_str = str(ctx.pgid) if ctx.pgid is not None else ""
        expanded_cmd = [
            arg.replace("{pid}", str(ctx.pid))
            .replace("{pgid}", pgid_str)
            .replace("{label}", ctx.label)
            for arg in self.command
        ]

        self._log(
            f"Running diagnostic hook [{ctx.label}]: {' '.join(expanded_cmd)}"
        )
        try:
            self._run_fn(
                expanded_cmd,
                stdout=sys.stderr,
                stderr=sys.stderr,
                timeout=max(1.0, self.diag_margin),
            )
        except Exception as e:
            self._log(f"Diagnostic hook execution failed [{ctx.label}]: {e}")

    def run(
        self, target: managed_process.ManagedProcess, label: str = ""
    ) -> None:
        """Executes the diagnostic hook command against a ManagedProcess."""
        if target.poll() is not None:
            return
        self(
            StepContext(
                label=label,
                pid=target.pid,
                pgid=target.pgid,
            )
        )


def make_subprocess_hook(
    command_template: Sequence[str],
    diag_margin: float = 10.0,
    verbose: bool = False,
) -> StepHook:
    """Convenience factory returning a StepHook executing an isolated command."""
    return DiagnosticHook(
        command_template, diag_margin=diag_margin, verbose=verbose
    )


class GracefulTimeoutRunner:
    """Manages child execution according to an escalation schedule."""

    def __init__(
        self,
        process: managed_process.ManagedProcess,
        schedule: Schedule,
        *,
        hard_timeout: float | None = None,
        grace_period: float | None = None,
        diagnostic_hook: StepHook | None = None,
        timeout_exit_code: int = EXIT_TIMEOUT,
        preserve_status: bool = False,
        verbose: bool = False,
        time_fn: Callable[[], float] = time.monotonic,
    ) -> None:
        self.process = process
        self.schedule = schedule
        self.hard_timeout = hard_timeout
        self.grace_period = grace_period
        self.events = (
            schedule.to_events(
                hard_timeout=hard_timeout,
                grace_period=grace_period,
                warn_stream=None,
            )
            if hard_timeout is not None
            else []
        )
        self.diagnostic_hook = diagnostic_hook
        self.timeout_exit_code = timeout_exit_code
        self.preserve_status = preserve_status
        self.verbose = verbose

        self._time_fn = time_fn
        self.schedule_triggered = False
        self.timed_out = False
        self.interrupted_signal: int | None = None
        self.executed_labels: list[str] = []

    def _log(self, message: str) -> None:
        if self.verbose:
            msg(message)

    def _send_signal(
        self, sig: int, target: managed_process.ProcessTarget
    ) -> None:
        self._log(f"Sending signal {sig} to {target.value}")
        self.process.send_signal(sig, target)

    def _run_diagnostic_hook(
        self, label: str, sig: Signals | int | None = None
    ) -> None:
        if self.diagnostic_hook:
            ctx = StepContext(
                label=label,
                pid=self.process.pid,
                pgid=self.process.pgid,
                signal=sig,
            )
            try:
                self.diagnostic_hook(ctx)
            except Exception as e:
                self._log(f"Diagnostic hook execution failed [{label}]: {e}")

    def _handle_event_expired(
        self, event_index: int, event: SignalEvent
    ) -> None:
        """Executes actions for a triggered timeout milestone."""
        self.schedule_triggered = True
        self.timed_out = True
        self.executed_labels.append(event.label)
        desc = f" ({event.description})" if event.description else ""
        self._log(
            f"Event deadline reached: {event.label}{desc} (offset: {event.deadline_offset}s)"
        )
        is_first = event_index == 0
        is_final = event_index == len(self.events) - 1
        if is_first or is_final:
            self._run_diagnostic_hook(event.label, event.signal)
        self._send_signal(event.signal, event.target)

    def _advance_timeline_on_signal(
        self,
        event_index: int,
        target_times: list[float],
        signum: int = signal.SIGINT,
    ) -> int:
        """Dispatches the next event in the schedule immediately and shifts remaining target times."""
        if not self.events:
            # When an external signal arrives without a pre-set hard timeout,
            # initiate the escalation schedule using minimum step durations.
            self.events = self.schedule.to_events(
                hard_timeout=None,
                grace_period=self.grace_period,
                warn_stream=None,
            )
            event_index = 0

        if event_index >= len(self.events):
            # All scheduled events have already been dispatched (including final kill).
            # Restore default handler and re-signal self so the runner exits immediately.
            signal.signal(signum, signal.SIG_DFL)
            os.kill(os.getpid(), signum)
            return event_index

        self.schedule_triggered = True
        if self.interrupted_signal is None:
            self.interrupted_signal = signum

        current_event = self.events[event_index]
        self.executed_labels.append(current_event.label)
        desc = (
            f" ({current_event.description})"
            if current_event.description
            else ""
        )
        self._log(
            f"External signal triggered schedule advance: {current_event.label}{desc}"
        )
        is_first = event_index == 0
        is_final = event_index == len(self.events) - 1
        if is_first or is_final:
            self._run_diagnostic_hook(current_event.label, current_event.signal)
        self._send_signal(current_event.signal, current_event.target)

        now = self._time_fn()
        base_offset = current_event.deadline_offset

        target_times.clear()
        for event in self.events:
            delta = event.deadline_offset - base_offset
            target_times.append(now + delta)

        return event_index + 1

    def _reap_process(self) -> None:
        """Ensures the child process is terminated cleanly upon loop exit."""
        if self.process.poll() is not None:
            return
        if self.schedule.final.label not in self.executed_labels:
            self._run_diagnostic_hook(
                self.schedule.final.label, self.schedule.final.signal_enum
            )
        self._send_signal(
            self.schedule.final.signal_enum, self.schedule.final.target
        )
        try:
            self.process.wait(timeout=5.0)
        except subprocess.TimeoutExpired:
            self._send_signal(
                signal.SIGKILL, managed_process.ProcessTarget.GROUP
            )
            self.process.wait()

    def _resolve_returncode(self) -> TimeoutResult:
        """Determines the final process exit code and result metadata."""
        raw_rc = (
            self.process.returncode
            if self.process.returncode is not None
            else 1
        )
        # In Python subprocess.Popen, a child process terminated by signal N has
        # returncode == -N. In standard POSIX shells, signal terminations are reported
        # as 128 + N. Therefore, 128 - (-N) == 128 + N.
        posix_rc = 128 - raw_rc if raw_rc < 0 else raw_rc

        if self.timed_out and not self.preserve_status:
            rc = self.timeout_exit_code
        elif self.interrupted_signal is not None and not self.preserve_status:
            rc = 128 + int(self.interrupted_signal)
        else:
            rc = posix_rc

        return TimeoutResult(
            returncode=rc,
            timed_out=self.timed_out,
            interrupted_signal=self.interrupted_signal,
        )

    def _run_loop(self, target_times: list[float]) -> None:
        """Runs the wait loop, escalating along the timeout timeline as deadlines expire."""
        event_index = 0
        while self.process.poll() is None:
            if event_index < len(self.events) and target_times:
                current_event = self.events[event_index]
                timeout = max(0.0, target_times[event_index] - self._time_fn())
            else:
                current_event = None
                timeout = None

            try:
                self.process.wait(timeout=timeout)
                break
            except subprocess.TimeoutExpired:
                if current_event:
                    self._handle_event_expired(event_index, current_event)
                    event_index += 1
            except _SignalInterruption as e:
                event_index = self._advance_timeline_on_signal(
                    event_index, target_times, signum=e.signum
                )

    def run(self) -> TimeoutResult:
        """Executes the command and manages the progressive timeout timeline.

        Note:
            Requires execution on the main thread because Python restricts
            signal handler registration (signal.signal) to the main thread.
        """
        start_time = self._time_fn()
        target_times = [
            start_time + event.deadline_offset for event in self.events
        ]

        def _signal_handler(signum: int, frame: Any) -> None:
            raise _SignalInterruption(signum)

        with signal_utils.scoped_signal_handlers(
            [signal.SIGINT, signal.SIGTERM], _signal_handler
        ):
            try:
                self._run_loop(target_times)
            finally:
                self._reap_process()

        return self._resolve_returncode()


def _main_argparser() -> argparse.ArgumentParser:
    """Builds argument parser for the graceful_timeout CLI."""
    parser = argparse.ArgumentParser(
        description="Run a command under a progressive timeout schedule."
    )
    parser.add_argument(
        "--schedule-file",
        type=Path,
        required=True,
        help="Path to JSON file defining the signal event schedule.",
    )
    parser.add_argument(
        "--timeout",
        "--hard",
        dest="timeout",
        type=str,
        default=None,
        help="Total command timeout budget (e.g. 90m, 5400s). Intermediate steps trigger before this deadline.",
    )
    parser.add_argument(
        "--diag-margin",
        type=str,
        default="10s",
        help="Maximum execution timeout for diagnostic snapshot commands (default: 10s).",
    )
    parser.add_argument(
        "--grace-period",
        type=str,
        default=None,
        help="Explicit total grace period budget to allocate across escalation "
        "steps (e.g. '60s', '60'). When omitted, the grace period is calculated "
        "dynamically from the schedule steps.",
    )
    parser.add_argument(
        "--timeout-exit-code",
        type=int,
        default=EXIT_TIMEOUT,
        help=f"Exit code to return if timeout sequence triggered (default: {EXIT_TIMEOUT}).",
    )
    parser.add_argument(
        "--preserve-status",
        action="store_true",
        help="Preserve child exit status even if timeout was triggered.",
    )
    parser.add_argument(
        "-v",
        "--verbose",
        action="store_true",
        help="Log timeout stages and signals to stderr.",
    )
    return parser


@dataclass(frozen=True)
class MainCommandSections:
    """Represents the partitioned sections of scheduled_timeout invocation.

    CLI invocations are partitioned by the '---' sub-command separator:
        [options] --- [snapshot_command...] --- <wrapped_command...>
        [options] --- <wrapped_command...>

    The three sections are:
        options: CLI flags controlling the timeout wrapper (e.g. --schedule-file,
            --timeout, --diag-margin, --timeout-exit-code, --preserve-status, -v).
        snapshot_command: Optional diagnostic command executed at the first
            and final schedule milestones before sending escalation signals
            while the target process tree is still alive (e.g. ['pstree.py', '{pid}', '--output', 'snap_{label}.json']).
            Supports '{pid}' and '{label}' template placeholders.
        wrapped_command: The primary payload command being executed and managed
            under the progressive timeout schedule (e.g. ['ninja', '-C', 'out/default']).
            Any trailing arguments or internal '--' delimiters are preserved verbatim.
    """

    options: list[str]
    wrapped_command: list[str]
    snapshot_command: list[str] = field(default_factory=list)

    @classmethod
    def from_segments(cls, segments: Sequence[Sequence[str]]) -> Self:
        """Constructs MainCommandSections from partitioned argument segments.

        Args:
            segments: Sequence of argument lists (options, [snapshot_cmd], wrapped_cmd).

        Returns:
            MainCommandSections instance.

        Raises:
            ValueError: If more than 3 segments are provided.
        """
        seg_list = [list(s) for s in segments]
        if len(seg_list) == 1:
            return cls(options=seg_list[0], wrapped_command=[])
        elif len(seg_list) == 2:
            return cls(
                options=seg_list[0],
                wrapped_command=seg_list[1],
            )
        elif len(seg_list) == 3:
            return cls(
                options=seg_list[0],
                snapshot_command=seg_list[1],
                wrapped_command=seg_list[2],
            )
        else:
            raise ValueError(
                f"Too many command sections ({len(seg_list)} found). "
                "Expected at most 3 sections: [options], [snapshot_cmd], <wrapped_cmd>"
            )

    @classmethod
    def from_argv(
        cls,
        argv: Sequence[str],
        separator: str = SUBCOMMAND_SEPARATOR,
    ) -> Self:
        """Partitions an argument list by separator and constructs MainCommandSections."""
        segments = list(partition_args(argv, separator, max_splits=2))
        return cls.from_segments(segments)

    def parse_args(self) -> argparse.Namespace:
        """Parses options and validates command presence."""
        parser = _main_argparser()
        if not self.wrapped_command and any(
            arg in self.options for arg in ("-h", "--help")
        ):
            parser.parse_args(self.options)

        if not self.wrapped_command:
            parser.error(
                f"No wrapped command specified. Usage: graceful_timeout [options] {SUBCOMMAND_SEPARATOR} [snapshot_cmd...] {SUBCOMMAND_SEPARATOR} <wrapped_cmd...>"
            )

        return parser.parse_args(self.options)

    def create_runner(self) -> GracefulTimeoutRunner:
        """Constructs a GracefulTimeoutRunner from parsed options and schedule file."""
        args = self.parse_args()
        hard_timeout = (
            parse_duration_to_seconds(args.timeout) if args.timeout else None
        )
        grace_period = (
            parse_duration_to_seconds(args.grace_period)
            if args.grace_period
            else None
        )
        diag_margin = parse_duration_to_seconds(args.diag_margin)
        diag_hook = (
            DiagnosticHook(
                self.snapshot_command,
                diag_margin=diag_margin,
                verbose=args.verbose,
            )
            if self.snapshot_command
            else None
        )
        schedule = load_schedule_file(args.schedule_file)
        # Validate schedule against hard_timeout and grace_period early before spawning child process
        schedule.to_events(
            hard_timeout, grace_period=grace_period, warn_stream=None
        )

        process = managed_process.ManagedProcess.spawn(self.wrapped_command)
        return GracefulTimeoutRunner(
            process,
            schedule,
            hard_timeout=hard_timeout,
            grace_period=grace_period,
            diagnostic_hook=diag_hook,
            timeout_exit_code=args.timeout_exit_code,
            preserve_status=args.preserve_status,
            verbose=args.verbose,
        )


def partition_args(
    args: Sequence[str],
    separator: str,
    max_splits: int | None = None,
) -> Iterator[list[str]]:
    """Yields lists of arguments partitioned by separator.

    The separator token is removed from all yielded results.

    Args:
        args: Sequence of argument strings.
        separator: Separator token to partition by.
        max_splits: Maximum number of splits to perform (None for unlimited).
    """
    current: list[str] = []
    splits = 0
    for arg in args:
        if arg == separator and (max_splits is None or splits < max_splits):
            yield current
            current = []
            splits += 1
        else:
            current.append(arg)
    yield current


def main(argv: Sequence[str] | None = None) -> int:
    """CLI entrypoint."""
    if argv is None:
        argv = sys.argv[1:]

    try:
        sections = MainCommandSections.from_argv(argv)
        runner = sections.create_runner()
    except ValueError as e:
        msg(f"Error: {e}", file=sys.stderr)
        return EXIT_USAGE_ERROR

    return runner.run().returncode


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""ODPM power trace metrics."""

import collections
import itertools
import logging
import statistics
import typing
from collections.abc import (
    Callable,
    Iterable,
    Mapping,
    MutableSequence,
    Sequence,
)

from reporting import metrics
from trace_processing import (
    trace_metrics,
    trace_model,
    trace_time,
    trace_utils,
)

_LOGGER: logging.Logger = logging.getLogger(__name__)
_RAIL_EVENT_SUFFIX: str = "_odpm_rail"
_ALL_RAIL_EVENTS_PATTERN: str = f".*{_RAIL_EVENT_SUFFIX}"
_POWER_ARG_KEY: str = "mW"
_WATTS_PER_MILLIWATT: float = 1e-3

# Rails are polled rapidly in-sequence at the end of a poll interval. If trace
# start and stop occur during the poll sequence, we could legitimately have
# a discrepancy of up to 2 between the measurement lengths. A larger discrepancy
# indicates unexpected behavior.
_MAX_SAMPLE_COUNT_DISCREPANCY: int = 2


def _normalize_rail_name(rail: str) -> str:
    """Strips the `_odpm_rail` event suffix if present."""
    return rail.removesuffix(_RAIL_EVENT_SUFFIX)


def _extract_samples_by_rail(
    model: trace_model.Model,
) -> dict[str, list[tuple[trace_time.TimePoint, float]]]:
    """Extracts timestamp-sorted (timestamp, watts) samples per ODPM rail."""
    counter_events = trace_utils.filter_events(
        model.all_events(),
        type=trace_model.CounterEvent,
    )
    samples_by_rail: dict[
        str, list[tuple[trace_time.TimePoint, float]]
    ] = collections.defaultdict(list)
    for event in counter_events:
        if not event.name.endswith(_RAIL_EVENT_SUFFIX):
            continue
        rail = event.name.removesuffix(_RAIL_EVENT_SUFFIX)
        mw_val = event.args.get(_POWER_ARG_KEY)
        if isinstance(mw_val, (int, float)):
            samples_by_rail[rail].append(
                (event.start, float(mw_val) * _WATTS_PER_MILLIWATT)
            )
    for samples in samples_by_rail.values():
        samples.sort(key=lambda s: s[0])
    return samples_by_rail


def _compute_series_stats(
    samples: Sequence[tuple[trace_time.TimePoint, float]],
) -> dict[str, float]:
    """Computes duration-weighted average, min, and max for a sample series."""
    powers = [power_w for _, power_w in samples]
    min_w = min(powers)
    max_w = max(powers)
    if len(samples) < 2:
        return {"average": powers[0], "min": min_w, "max": max_w}

    total_duration_ns = (samples[-1][0] - samples[0][0]).to_nanoseconds()
    if total_duration_ns <= 0:
        return {
            "average": statistics.mean(powers),
            "min": min_w,
            "max": max_w,
        }

    weighted_energy_w_ns = sum(
        curr_power_w * (curr_ts - prev_ts).to_nanoseconds()
        for (prev_ts, _), (curr_ts, curr_power_w) in itertools.pairwise(samples)
    )
    return {
        "average": weighted_energy_w_ns / total_duration_ns,
        "min": min_w,
        "max": max_w,
    }


class OdpmPowerMetricsProcessor(trace_metrics.MetricsProcessor):
    """Computes power consumption metrics from ODPM trace events.

    Given a trace containing ODPM power samples (CounterEvents named
    "<rail>_odpm_rail" with power readings in "mW" args), computes per-rail
    power usage metrics in Watts for the selected rails.

    When `aggregates_only` is False, metrics for individual rails are labeled
    `Power_rail_<rail>` and metrics for `sum_rails` groups are labeled
    `Power_<group_name>`, containing raw sample series. When `aggregates_only`
    is True, single-value duration-weighted average metrics are emitted as
    `PowerAverage_rail_<rail>` and `PowerAverage_<group_name>`.
    """

    FREEFORM_METRICS_FILENAME: str = "odpm_power"

    def __init__(
        self,
        rails: Iterable[str] | Callable[[str], bool] = (),
        sum_rails: (
            Mapping[str, Iterable[str] | Callable[[str], bool]] | None
        ) = None,
        all_rails: bool = False,
        aggregates_only: bool = False,
    ) -> None:
        """Constructor.

        Args:
            rails: Iterable of ODPM rail names to report metrics for (e.g.,
                ["cpu_big", "cpu_mid", "cpu_little", "gpu"]). Can be empty if
                `all_rails` or `sum_rails` is specified.
                Alternatively, a callable filter predicate
                `Callable[[str], bool]` selecting rails present in the trace,
                reported in sorted order (e.g., lambda r: r != "battery").
            sum_rails: Optional mapping from metric suffix name to either an
                iterable of ODPM rail names or a callable filter predicate
                `Callable[[str], bool]` selecting rail names to sum
                sample-by-sample across each poll cycle (e.g., {"cpu_total":
                ["cpu_big", "cpu_mid", "cpu_little"]} or
                {"all_rails_except_battery": lambda r: r != "battery"}).
                Reports metrics with suffix `<group_name>`.
            all_rails: When True, reports metrics for all ODPM rails present in
                the trace in sorted order. Mutually exclusive with `rails`.
            aggregates_only: When True, emits a single duration-weighted
                `PowerAverage_<suffix>` metric per rail or `sum_rails` group
                instead of multi-sample `Power_<suffix>` series.

        Raises:
            ValueError: If none of `rails`, `all_rails`, or `sum_rails` are
                specified, if a `sum_rails` entry has an empty rail list, if
                `rails` or a `sum_rails` entry is a bare str, or if both
                `rails` and `all_rails` are specified.
        """
        if isinstance(rails, str):
            raise ValueError(
                "`rails` must be an iterable of ODPM rail names or a callable, "
                f"not the str {rails!r}; did you mean [{rails!r}]?"
            )
        # Preserve caller-specified order while deduplicating.
        self._rails: tuple[str, ...] | Callable[[str], bool] = (
            rails
            if callable(rails)
            else tuple(dict.fromkeys(_normalize_rail_name(r) for r in rails))
        )
        self._sum_rails: dict[str, tuple[str, ...] | Callable[[str], bool]] = {}
        for name, rail_spec in (sum_rails or {}).items():
            if callable(rail_spec):
                self._sum_rails[name] = rail_spec
            elif isinstance(rail_spec, str):
                raise ValueError(
                    f"sum_rails group '{name}' must be an iterable of ODPM "
                    f"rail names or a callable, not the str {rail_spec!r}; "
                    f"did you mean [{rail_spec!r}]?"
                )
            else:
                normalized_rails = tuple(
                    dict.fromkeys(_normalize_rail_name(r) for r in rail_spec)
                )
                if not normalized_rails:
                    raise ValueError(
                        f"sum_rails group '{name}' must specify at least one ODPM rail."
                    )
                self._sum_rails[name] = normalized_rails
        self._all_rails: bool = all_rails
        self._aggregates_only: bool = aggregates_only
        if self._rails and self._all_rails:
            raise ValueError("Cannot specify both `rails` and `all_rails`.")
        if not self._rails and not self._all_rails and not self._sum_rails:
            raise ValueError(
                "Must specify at least one ODPM rail, all_rails=True, or sum_rails to report."
            )

    @staticmethod
    def list_rails(model: trace_model.Model) -> list[str]:
        """Returns a sorted list of ODPM rail names present in the trace model.

        Args:
            model: In-memory representation of a trace.

        Returns:
            Sorted list of unique rail names that have ODPM power counter events.
        """
        return sorted(_extract_samples_by_rail(model).keys())

    @property
    @typing.override
    def event_patterns(self) -> set[str]:
        """Patterns describing the trace events needed to generate these metrics."""
        return {_ALL_RAIL_EVENTS_PATTERN}

    def _results_for_series(
        self,
        metric_suffix: str,
        samples: Sequence[tuple[trace_time.TimePoint, float]],
        target_description: str,
    ) -> list[metrics.TestCaseResult]:
        """Builds TestCaseResults for a power series."""
        if self._aggregates_only:
            stats = _compute_series_stats(samples)
            return [
                metrics.TestCaseResult(
                    label=f"PowerAverage_{metric_suffix}",
                    unit=metrics.Unit.watts,
                    values=[stats["average"]],
                    doc=f"ODPM average power usage for {target_description}",
                )
            ]
        return [
            metrics.TestCaseResult(
                label=f"Power_{metric_suffix}",
                unit=metrics.Unit.watts,
                values=[power_w for _, power_w in samples],
                doc=f"ODPM power usage samples for {target_description}",
            )
        ]

    def _sum_rail_samples(
        self,
        metric_suffix: str,
        rails_to_sum: Sequence[str],
        samples_by_rail: Mapping[str, list[tuple[trace_time.TimePoint, float]]],
    ) -> list[tuple[trace_time.TimePoint, float]]:
        """Validates sample count alignment and computes sample-by-sample sums across rails."""
        lengths = {r: len(samples_by_rail[r]) for r in rails_to_sum}
        min_len = min(lengths.values())
        max_len = max(lengths.values())
        if max_len - min_len > _MAX_SAMPLE_COUNT_DISCREPANCY:
            raise ValueError(
                f"Included ODPM rails for '{metric_suffix}' have sample counts "
                f"differing by more than {_MAX_SAMPLE_COUNT_DISCREPANCY} "
                f"(min={min_len}, max={max_len}): {lengths}"
            )
        if max_len > min_len:
            _LOGGER.warning(
                "Included ODPM rails for '%s' have different sample counts "
                "(min=%d, max=%d); truncating to %d samples: %s",
                metric_suffix,
                min_len,
                max_len,
                min_len,
                lengths,
            )

        # The `zip` will align entries across the sample vectors in order, discarding excess
        # samples at the end. Strictly speaking, it would be better to align based on timestamp
        # and discard entries that are part of a partial poll sequence. (See description of
        # `_MAX_SAMPLE_COUNT_DISCREPANCY`.) However, the extra complexity is likely not
        # worthwhile.
        return [
            (cycle_samples[0][0], sum(power_w for _, power_w in cycle_samples))
            for cycle_samples in zip(
                *(samples_by_rail[r] for r in rails_to_sum)
            )
        ]

    @typing.override
    def process_metrics(
        self, model: trace_model.Model
    ) -> MutableSequence[metrics.TestCaseResult]:
        """Calculates per-rail and summed ODPM power metrics from the trace model.

        Args:
             model: In-memory representation of a system trace containing ODPM
                 power counter events.

        Returns:
            List of TestCaseResult objects for the selected rails and any
            `sum_rails` groups.
        """
        all_samples_by_rail = _extract_samples_by_rail(model)

        collect_all_rails = (
            self._all_rails
            or callable(self._rails)
            or any(callable(spec) for spec in self._sum_rails.values())
        )
        if collect_all_rails:
            samples_by_rail = all_samples_by_rail
        else:
            selected_rails_set = set(
                () if callable(self._rails) else self._rails
            )
            for rail_spec in self._sum_rails.values():
                if not callable(rail_spec):
                    selected_rails_set.update(rail_spec)
            samples_by_rail = {
                rail: samples
                for rail, samples in all_samples_by_rail.items()
                if rail in selected_rails_set
            }

        if not samples_by_rail and not self._sum_rails:
            available = [] if self._all_rails else sorted(all_samples_by_rail)
            _LOGGER.warning(
                "No ODPM power events found for selected rails %s (all_rails=%s). Available rails in trace: %s",
                self._rails if callable(self._rails) else list(self._rails),
                self._all_rails,
                available,
            )
            return []

        results: list[metrics.TestCaseResult] = []
        rails_to_report: Iterable[str]
        if self._all_rails:
            rails_to_report = sorted(samples_by_rail.keys())
        elif callable(self._rails):
            rails_to_report = [
                r for r in sorted(samples_by_rail.keys()) if self._rails(r)
            ]
        else:
            rails_to_report = self._rails
        for rail in rails_to_report:
            samples = samples_by_rail.get(rail)
            if not samples:
                _LOGGER.warning(
                    "No ODPM power samples found for requested rail '%s'.", rail
                )
                continue

            results.extend(
                self._results_for_series(
                    metric_suffix=f"rail_{rail}",
                    samples=samples,
                    target_description=f"rail {rail}",
                )
            )

        for group_name, group_spec in self._sum_rails.items():
            if callable(group_spec):
                group_rails: Sequence[str] = [
                    r for r in sorted(samples_by_rail.keys()) if group_spec(r)
                ]
                if not group_rails:
                    raise ValueError(
                        f"No ODPM power samples matched filter for sum_rails group "
                        f"'{group_name}'. Available rails in trace: "
                        f"{sorted(all_samples_by_rail)}"
                    )
            else:
                group_rails = group_spec
                missing_rails = [
                    r for r in group_rails if not samples_by_rail.get(r)
                ]
                if missing_rails:
                    raise ValueError(
                        f"Missing ODPM power samples for rail(s) {missing_rails} required by "
                        f"sum_rails group '{group_name}'. Available rails in trace: "
                        f"{sorted(all_samples_by_rail)}"
                    )
            summed_samples = self._sum_rail_samples(
                metric_suffix=group_name,
                rails_to_sum=group_rails,
                samples_by_rail=samples_by_rail,
            )
            results.extend(
                self._results_for_series(
                    metric_suffix=group_name,
                    samples=summed_samples,
                    target_description=f"rails {', '.join(group_rails)}",
                )
            )

        return results

    @typing.override
    def process_freeform_metrics(
        self, model: trace_model.Model
    ) -> tuple[str, metrics.JSON]:
        """Computes duration-weighted average, min, and max power for ODPM rails.

        The metrics are computed for all individual rails and sum_rails groups.

        Args:
            model: In-memory representation of a system trace.

        Returns:
            Tuple of (filename, JSON dictionary mapping each rail or sum_rails
            group name to {"average": <watts>, "min": <watts>, "max": <watts>}).
        """
        samples_by_rail = _extract_samples_by_rail(model)
        breakdown: dict[str, metrics.JSON] = {
            rail: _compute_series_stats(samples)
            for rail, samples in samples_by_rail.items()
            if samples
        }
        for group_name, group_spec in self._sum_rails.items():
            if callable(group_spec):
                group_rails: Sequence[str] = [
                    r for r in sorted(samples_by_rail.keys()) if group_spec(r)
                ]
                if not group_rails:
                    continue
            else:
                group_rails = group_spec
                if any(not samples_by_rail.get(r) for r in group_rails):
                    continue
            summed_samples = self._sum_rail_samples(
                metric_suffix=group_name,
                rails_to_sum=group_rails,
                samples_by_rail=samples_by_rail,
            )
            if summed_samples:
                breakdown[group_name] = _compute_series_stats(summed_samples)
        return self.FREEFORM_METRICS_FILENAME, dict(sorted(breakdown.items()))

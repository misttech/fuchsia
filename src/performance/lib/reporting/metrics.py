# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Metrics reporting helpers."""

import dataclasses
from typing import MutableMapping, MutableSequence, Self, TypedDict

from trace_processing import trace_metrics


class SuiteDocumentation(TypedDict):
    """Documentation for a full test suite."""

    test_suite: str
    test_class: str
    doc: str
    code_path: str
    line_no: int
    metrics_processors: list[trace_metrics.MetricsProcessorDescription]


@dataclasses.dataclass
class Report:
    """Metrics data for a test suite."""

    structured: MutableSequence[trace_metrics.TestCaseResult]
    """Named single-value (or list-of-values) metrics."""
    freeform: MutableMapping[str, trace_metrics.JSON]

    def extend(self, other: Self) -> Self:
        self.structured.extend(other.structured)
        self.freeform.update(other.freeform)
        return self


@dataclasses.dataclass(frozen=True)
class DocumentedReport:
    """Metrics data, and accompanying documentation, for a test suite."""

    data: Report
    """Structured and freeform measurements."""
    docs: SuiteDocumentation
    """Metadata which can be used to generate test suite documentation."""

    def extend(self, report: Report) -> Self:
        self.data.extend(report)
        self.docs["metrics_processors"].append(
            trace_metrics.MetricsProcessorDescription(
                doc="",
                code_path="",
                line_no=0,
                metrics=[tcr.describe() for tcr in report.structured],
            )
        )
        return self

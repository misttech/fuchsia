# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Metrics reporting helpers."""

import dataclasses
import inspect as py_inspect
from typing import (
    Any,
    Callable,
    MutableMapping,
    MutableSequence,
    Self,
    TypedDict,
)

from trace_processing import trace_metrics


class SuiteDescription(TypedDict):
    """Description of a test suite class."""

    test_suite: str
    test_class: str
    doc: str
    code_path: str
    line_no: int


class SuiteDocumentation(SuiteDescription):
    """Documentation for a test suite, including code that generates metrics."""

    metrics_processors: list[trace_metrics.MetricsProcessorDescription]


@dataclasses.dataclass
class Report:
    """Metrics data for a test suite."""

    structured: MutableSequence[trace_metrics.TestCaseResult]
    """Named single-value (or list-of-values) metrics."""
    freeform: MutableMapping[str, trace_metrics.JSON]
    """Mapping of strings to arbitrary JSON data."""
    metrics_processors: list[
        trace_metrics.MetricsProcessorDescription
    ] = dataclasses.field(default_factory=list)
    """Descriptions of the code that captured the metrics in this report."""

    def extend(self, other: Self) -> Self:
        self.structured.extend(other.structured)
        self.freeform.update(other.freeform)
        self.metrics_processors.extend(other.metrics_processors)
        return self

    def generate_docs(self, desc: SuiteDescription) -> SuiteDocumentation:
        return SuiteDocumentation(
            **desc, metrics_processors=self.metrics_processors
        )


class CallableDescription(TypedDict):
    """Documentation for a callable used to generate metrics."""

    doc: str
    code_path: str
    line_no: int


def describe_callable(c: Callable[..., Any]) -> CallableDescription:
    docstring = py_inspect.getdoc(c)
    assert docstring, c
    return CallableDescription(
        doc=docstring,
        code_path=py_inspect.getfile(c),
        line_no=py_inspect.getsourcelines(c)[1],
    )

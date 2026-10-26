# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Minimal thread-pool unittest runner for parallel host test execution."""

import concurrent.futures
import os
import sys
import threading
import unittest
from collections.abc import Iterator
from types import TracebackType
from typing import Any

_SysExcInfoType = (
    tuple[type[BaseException], BaseException, TracebackType | None]
    | tuple[None, None, None]
)

DEFAULT_MAX_WORKERS = 4


def _iter_cases(
    suite: unittest.TestSuite | unittest.TestCase,
) -> Iterator[unittest.TestCase]:
    if isinstance(suite, unittest.TestCase):
        yield suite
    else:
        for item in suite:
            yield from _iter_cases(item)


class _CaseResult(unittest.TestResult):
    """Per-test result recorder that buffers events for thread-safe replay."""

    def __init__(self) -> None:
        self.events: list[tuple[str, tuple[Any, ...]]] = []
        super().__init__()

    def addSuccess(self, test: unittest.TestCase) -> None:
        self.events.append(("addSuccess", (test,)))

    def addError(self, test: unittest.TestCase, err: _SysExcInfoType) -> None:
        self.events.append(("addError", (test, err)))

    def addFailure(self, test: unittest.TestCase, err: _SysExcInfoType) -> None:
        self.events.append(("addFailure", (test, err)))

    def addSkip(self, test: unittest.TestCase, reason: str) -> None:
        self.events.append(("addSkip", (test, reason)))

    def addExpectedFailure(
        self, test: unittest.TestCase, err: _SysExcInfoType
    ) -> None:
        self.events.append(("addExpectedFailure", (test, err)))

    def addUnexpectedSuccess(self, test: unittest.TestCase) -> None:
        self.events.append(("addUnexpectedSuccess", (test,)))

    def addSubTest(
        self,
        test: unittest.TestCase,
        subtest: unittest.TestCase,
        err: _SysExcInfoType | None,
    ) -> None:
        self.events.append(("addSubTest", (test, subtest, err)))


def _run_case(
    case: unittest.TestCase,
    result: unittest.TestResult,
    lock: threading.Lock,
) -> None:
    case_result = _CaseResult()
    case.run(case_result)
    with lock:
        result.startTest(case)
        for method_name, args in case_result.events:
            getattr(result, method_name)(*args)
        result.stopTest(case)


class _ParallelSuite(unittest.TestSuite):
    """TestSuite wrapper that executes leaf TestCases across a thread pool."""

    def __init__(
        self,
        suite: unittest.TestSuite | unittest.TestCase,
        max_workers: int,
    ) -> None:
        super().__init__()
        self._root = suite
        self._max_workers = max_workers

    def run(
        self, result: unittest.TestResult, debug: bool = False
    ) -> unittest.TestResult:
        if debug:
            self._root.run(result)
            return result

        cases_by_class: dict[
            type[unittest.TestCase], list[unittest.TestCase]
        ] = {}
        for case in _iter_cases(self._root):
            cases_by_class.setdefault(type(case), []).append(case)

        lock = threading.Lock()
        active_classes: list[
            tuple[type[unittest.TestCase], list[unittest.TestCase]]
        ] = []
        runnable_cases: list[unittest.TestCase] = []

        for cls, cases in cases_by_class.items():
            try:
                cls.setUpClass()
            except Exception:
                result.startTest(cases[0])
                result.addError(cases[0], sys.exc_info())
                result.stopTest(cases[0])
                cls.doClassCleanups()
            else:
                active_classes.append((cls, cases))
                runnable_cases.extend(cases)

        try:
            if runnable_cases:
                with concurrent.futures.ThreadPoolExecutor(
                    max_workers=self._max_workers
                ) as executor:
                    futures = [
                        executor.submit(_run_case, case, result, lock)
                        for case in runnable_cases
                    ]
                    for future in futures:
                        future.result()
        finally:
            for cls, cases in active_classes:
                try:
                    cls.tearDownClass()
                except Exception:
                    with lock:
                        result.addError(cases[0], sys.exc_info())
                finally:
                    cls.doClassCleanups()

        return result


class ParallelTextTestRunner(unittest.TextTestRunner):
    """TextTestRunner that runs test cases concurrently in worker threads."""

    def __init__(
        self,
        *args: Any,
        max_workers: int | None = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(*args, **kwargs)
        if max_workers is None:
            env_jobs = os.environ.get("AGENT_TEST_JOBS", "")
            max_workers = (
                int(env_jobs) if env_jobs.isdigit() else DEFAULT_MAX_WORKERS
            )
        self.max_workers = max(1, max_workers)

    def run(
        self, test: unittest.TestSuite | unittest.TestCase
    ) -> unittest.TextTestResult[Any]:
        """Run the given test case or test suite."""
        if self.max_workers <= 1:
            return super().run(test)
        return super().run(_ParallelSuite(test, self.max_workers))


def parallel_main() -> None:
    """Run unittest.main() using ParallelTextTestRunner."""
    unittest.main(testRunner=ParallelTextTestRunner)

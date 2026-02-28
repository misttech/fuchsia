# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Control flow utilities: retries, timeouts, complex try-catch, etc"""

import logging
import time
from collections.abc import Callable
from datetime import timedelta
from typing import Any

from mobly import signals

from honeydew.utils.deadline import Deadline

_LOGGER: logging.Logger = logging.getLogger(__name__)

# TODO(b/402203873): Remove once in-tree is stabilized. This value is a global
# timeout enforced on all retry commands that have no deadline and is set to be
# large enough that no individual retry logic will timeout, but will definitely
# timeout before the entire test. This ensures that the test code will exit and
# attempt to do clean up, which gives us more logging in hang cases.
_GLOBAL_TASK_TIMEOUT = timedelta(seconds=900)

# TODO(b/421207005): Task Swarming will kill a whole test shard if there hasn't
# been any logging output for a configurable amount of time. As of Jun 11, 2025
# this timeout is normally set for 6 minutes. Periodically we'll emit logs even
# when suspended. Make sure to keep this well below any I/O timeouts.
_IDLE_LOGGING_PERIOD = timedelta(seconds=60)


class RetryAbortingError(Exception):
    def __init__(self, message: str) -> None:
        super().__init__(message)


class RetriableError(Exception):
    def __init__(self, message: str) -> None:
        super().__init__(message)


def retry_until_deadline(
    task: Callable[[], Any],
    deadline: Deadline,
    retry_delay: timedelta = timedelta(seconds=1),
    backoff: bool = False,
) -> Any:
    """Retries the given |task| until the deadline is due.

    If the |task| returns without error, halts. Otherwise,
    repeats after |retry_delay|. A task may also raise a permanent
    error of type RetryAbortingError to avoid more retries.

    If |backoff| is True, |retry_delay| will be doubled between each
    attempt.

    Returns:
        The value returned by |task|.

    Raises:
        *Exception: The last exception raised by the last call to |task|.
    """
    assert isinstance(deadline, Deadline)

    while True:
        try:
            return task()
        except (
            RetryAbortingError,
            TypeError,
            NameError,
            SyntaxError,
            signals.TestError,
            signals.TestFailure,
            signals.TestAbortSignal,
        ):
            raise
        except Exception as e:  # pylint: disable=broad-exception-caught
            if deadline.is_due() or deadline.remaining_duration() < retry_delay:
                raise e
            else:
                _LOGGER.info(
                    "%s failed, will retry again in %s (%s) (error=%s type=%s)",
                    _pretty_func_name(task),
                    retry_delay,
                    deadline,
                    e,
                    type(e),
                )

        sleep_for_duration(retry_delay)

        if backoff:
            retry_delay *= 2


def retry(
    task: Callable[[], Any],
    max_tries: int | None = None,
    retry_delay: timedelta = timedelta(seconds=1),
    backoff: bool = False,
) -> Any:
    """Retries the given |task| for up to |max_tries| times."""
    assert max_tries is None or max_tries > 0

    # TODO(b/402203873): Remove once in-tree is stabilized. See
    # `_GLOBAL_TASK_TIMEOUT` for an explanation.
    if max_tries is None or max_tries == 0:
        return retry_for_duration(
            task=task,
            retry_delay=retry_delay,
            backoff=backoff,
            duration=_GLOBAL_TASK_TIMEOUT,
        )

    attempts: int = 0
    task_name: str = _pretty_func_name(task)
    while True:
        try:
            return task()
        except (
            RetryAbortingError,
            TypeError,
            NameError,
            SyntaxError,
            signals.TestError,
            signals.TestFailure,
            signals.TestAbortSignal,
        ):
            raise
        except Exception as e:  # pylint: disable=broad-exception-caught
            attempts += 1
            limit = str(max_tries)
            if max_tries is None:
                limit = "<unbounded>"
            elif attempts == max_tries:
                _LOGGER.info(
                    "%s failed %s times, aborting.", task_name, attempts
                )
                raise e
            _LOGGER.info(
                "%s failed (%s out of %s), will retry again in %s (error=%s type=%s)",
                task_name,
                attempts,
                limit,
                retry_delay,
                e,
                type(e),
            )

        sleep_for_duration(retry_delay)

        if backoff:
            retry_delay *= 2


def retry_for_duration(
    task: Callable[[], Any],
    duration: timedelta,
    retry_delay: timedelta = timedelta(seconds=1),
    backoff: bool = False,
) -> Any:
    """Calls |retry_until_deadline| with a deadline based on |duration|"""
    return retry_until_deadline(
        task, Deadline.from_duration(duration), retry_delay, backoff
    )


def repeat_until_deadline(
    task: Callable[[], Any],
    deadline: Deadline,
    repeat_delay: timedelta = timedelta(seconds=1),
) -> None:
    """Repeats |task| with until deadline is reached.

    If task fails, returns immediately. Between each repeat,
    sleeps for |repeat_delay|.
    """
    count = 0
    while not deadline.is_due():
        count += 1
        _LOGGER.debug(
            "Repeating %s for the %s time", _pretty_func_name(task), count
        )
        task()
        if repeat_delay < deadline.remaining_duration():
            sleep_for_duration(repeat_delay)
        else:
            break


def repeat_for_duration(
    task: Callable[[], Any],
    duration: timedelta,
    repeat_delay: timedelta = timedelta(seconds=1),
) -> None:
    """Calls |repeat_until_deadline| with a deadline based on |duration|"""
    repeat_until_deadline(task, Deadline.from_duration(duration), repeat_delay)


def sleep_until_deadline(deadline: Deadline) -> None:
    """Sleeps until the deadline is reached.

    This function generates logs at intervals to prevent swarming from thinking
    we're frozen and timing us out.
    """
    _LOGGER.debug("Sleeping for %s...", deadline.remaining_duration())

    first_iteration = True
    while not deadline.is_due():
        if not first_iteration:
            _LOGGER.info(
                "Still sleeping... %s remaining", deadline.remaining_duration()
            )

        # Sleep for no longer than _IDLE_LOGGING_PERIOD, to ensure swarming
        # doesn't time us out.
        time.sleep(
            min(
                deadline.remaining_duration(), _IDLE_LOGGING_PERIOD
            ).total_seconds()
        )
        first_iteration = False
    _LOGGER.debug("Done sleeping!")


def sleep_for_duration(duration: timedelta) -> None:
    """Sleeps for the length of this duration.

    This function generates logs at intervals to prevent swarming from thinking
    we're frozen and timing us out.
    """
    return sleep_until_deadline(Deadline.from_duration(duration))


def _pretty_func_name(func: Callable[..., Any]) -> str:
    if hasattr(func, "__qualname__"):
        return func.__qualname__
    else:
        return str(func)

# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Tests for control_flows.py"""

import unittest
from datetime import datetime, timedelta
from unittest import mock

from mobly import signals
from parameterized import param, parameterized

from honeydew.utils.control_flows import (
    Deadline,
    RetryAbortingError,
    repeat_until_deadline,
    retry,
    retry_until_deadline,
)


class DeadlineTest(unittest.TestCase):
    def setUp(self) -> None:
        return super().setUp()

    @mock.patch("honeydew.utils.control_flows.datetime", wraps=datetime)
    def test_deadline(self, mock_datetime: mock.Mock) -> None:
        start = datetime(2024, 7, 10)
        mock_datetime.now.return_value = start
        deadline = Deadline.from_duration(timedelta(seconds=100))

        # Initial conditions
        self.assertFalse(deadline.is_due())
        self.assertEqual(deadline.elapsed_duration(), timedelta(seconds=0))
        self.assertEqual(deadline.remaining_duration(), timedelta(seconds=100))
        self.assertEqual(deadline.total_duration(), timedelta(seconds=100))

        # During the deadline
        mock_datetime.now.return_value = start + timedelta(seconds=40)
        self.assertFalse(deadline.is_due())
        self.assertEqual(deadline.elapsed_duration(), timedelta(seconds=40))
        self.assertEqual(deadline.remaining_duration(), timedelta(seconds=60))
        self.assertEqual(deadline.total_duration(), timedelta(seconds=100))

        # Almost at the deadline
        mock_datetime.now.return_value = start + timedelta(seconds=99)
        self.assertFalse(deadline.is_due())
        self.assertEqual(deadline.elapsed_duration(), timedelta(seconds=99))
        self.assertEqual(deadline.remaining_duration(), timedelta(seconds=1))
        self.assertEqual(deadline.total_duration(), timedelta(seconds=100))

        # At the deadline
        mock_datetime.now.return_value = start + timedelta(seconds=100)
        self.assertTrue(deadline.is_due())
        self.assertEqual(deadline.elapsed_duration(), timedelta(seconds=100))
        self.assertEqual(deadline.remaining_duration(), timedelta(seconds=0))
        self.assertEqual(deadline.total_duration(), timedelta(seconds=100))

        # Right after the deadline has passed
        mock_datetime.now.return_value = start + timedelta(seconds=101)
        self.assertTrue(deadline.is_due())
        self.assertEqual(deadline.elapsed_duration(), timedelta(seconds=101))
        self.assertEqual(deadline.remaining_duration(), timedelta(seconds=0))
        self.assertEqual(deadline.total_duration(), timedelta(seconds=100))


class _RetryAbortingErrorSubClass(RetryAbortingError):
    pass


class RetryTest(unittest.TestCase):
    @mock.patch("time.sleep")
    def test_retry_succeeds_on_first_try(self, mock_sleep: mock.Mock) -> None:
        mock_task = mock.Mock(return_value=1)
        retry(
            task=mock_task,
            max_tries=3,
            retry_delay=timedelta(seconds=1),
        )

        mock_task.assert_called_once()
        mock_sleep.assert_not_called()

    @parameterized.expand(
        [
            param(expected_exception=RetryAbortingError),
            param(expected_exception=_RetryAbortingErrorSubClass),
            param(expected_exception=SyntaxError),
            param(expected_exception=signals.TestError),
            param(expected_exception=signals.TestFailure),
            param(expected_exception=signals.TestAbortClass),
            param(expected_exception=signals.TestAbortAll),
        ]
    )
    @mock.patch("honeydew.utils.control_flows.sleep_for_duration")
    def test_retry_does_not_retry_some_errors(
        self,
        mock_sleep_for_duration: mock.Mock,
        expected_exception: type[Exception],
    ) -> None:
        mock_task = mock.Mock(side_effect=expected_exception("expected"))
        with self.assertRaisesRegex(
            expected_exception=expected_exception,
            expected_regex="expected",
        ):
            retry(
                task=mock_task,
                max_tries=3,
                retry_delay=timedelta(seconds=1),
            )

        mock_task.assert_called_once()
        mock_sleep_for_duration.assert_not_called()

    @mock.patch("honeydew.utils.control_flows.sleep_for_duration")
    def test_retry_succeeds_on_retry(
        self, mock_sleep_for_duration: mock.Mock
    ) -> None:
        mock_task = mock.Mock(
            side_effect=2 * [RuntimeError("Triggers a retry")] + [True]
        )
        retry(
            task=mock_task,
            max_tries=3,
            retry_delay=timedelta(seconds=1),
        )

        self.assertEqual(mock_task.call_count, 3)
        self.assertEqual(mock_sleep_for_duration.call_count, 2)

    @mock.patch("time.sleep")
    @mock.patch("honeydew.utils.control_flows.datetime", wraps=datetime)
    def test_retry_never_succeeds(
        self, mock_datetime: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        mock_datetime.now.return_value = datetime(2024, 7, 10)

        def advance_now(delta: float) -> None:
            mock_datetime.now.return_value += timedelta(seconds=delta)

        mock_sleep.side_effect = advance_now
        mock_task = mock.Mock(side_effect=RuntimeError("Triggers a retry"))
        with self.assertRaisesRegex(
            expected_exception=RuntimeError,
            expected_regex="Triggers a retry",
        ):
            retry(
                task=mock_task,
                max_tries=3,
                retry_delay=timedelta(seconds=1),
            )

        self.assertEqual(mock_task.call_count, 3)
        self.assertEqual(mock_sleep.call_count, 2)
        mock_sleep.assert_has_calls([mock.call(1), mock.call(1)])

    @mock.patch("time.sleep")
    @mock.patch("honeydew.utils.control_flows.datetime", wraps=datetime)
    def test_retry_never_succeeds_with_backoff(
        self, mock_datetime: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        mock_datetime.now.return_value = datetime(2024, 7, 10)

        def advance_now(delta: float) -> None:
            mock_datetime.now.return_value += timedelta(seconds=delta)

        mock_sleep.side_effect = advance_now
        mock_task = mock.Mock(side_effect=RuntimeError("Triggers a retry"))
        with self.assertRaisesRegex(
            expected_exception=RuntimeError,
            expected_regex="Triggers a retry",
        ):
            retry(
                task=mock_task,
                max_tries=5,
                retry_delay=timedelta(seconds=1),
                backoff=True,
            )

        self.assertEqual(mock_task.call_count, 5)
        self.assertEqual(mock_sleep.call_count, 4)
        mock_sleep.assert_has_calls(
            [mock.call(duration) for duration in [1, 2, 4, 8]]
        )

    @mock.patch("time.sleep")
    @mock.patch("honeydew.utils.control_flows.datetime", wraps=datetime)
    def test_retry_with_deadline_hits_backoff_cap(
        self, mock_datetime: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        mock_datetime.now.return_value = datetime(2024, 7, 10)

        def advance_now(delta: float) -> None:
            mock_datetime.now.return_value += timedelta(seconds=delta)

        mock_sleep.side_effect = advance_now
        mock_task = mock.Mock(side_effect=RuntimeError("Triggers a retry"))
        with self.assertRaisesRegex(
            expected_exception=RuntimeError,
            expected_regex="Triggers a retry",
        ):
            retry(
                task=mock_task,
                max_tries=5,
                retry_delay=timedelta(seconds=25),
                backoff=True,
            )

        self.assertEqual(mock_task.call_count, 5)

        expected_sleep_durations = [25, 50, 60, 40, 60, 60, 60, 20]

        expected_sleep_calls = [
            mock.call(float(duration)) for duration in expected_sleep_durations
        ]
        mock_sleep.assert_has_calls(expected_sleep_calls)

    @mock.patch("time.sleep")
    def test_retry_until_deadline_succeeds_on_first_try(
        self, mock_sleep: mock.Mock
    ) -> None:
        deadline = Deadline.from_duration(timedelta(seconds=100))
        mock_task = mock.Mock(return_value=1)
        retry_until_deadline(
            task=mock_task,
            deadline=deadline,
            retry_delay=timedelta(seconds=1),
        )

        mock_task.assert_called_once()
        mock_sleep.assert_not_called()

    @parameterized.expand(
        [
            param(expected_exception=RetryAbortingError),
            param(expected_exception=_RetryAbortingErrorSubClass),
            param(expected_exception=SyntaxError),
            param(expected_exception=signals.TestFailure),
        ]
    )
    @mock.patch("honeydew.utils.control_flows.sleep_for_duration")
    def test_retry_until_deadline_does_not_retry_some_errors(
        self,
        mock_sleep: mock.Mock,
        expected_exception: type[Exception],
    ) -> None:
        mock_task = mock.Mock(side_effect=expected_exception("expected"))
        mock_sleep.side_effect = AssertionError("sleep should not be called")
        with self.assertRaisesRegex(
            expected_exception=expected_exception,
            expected_regex="expected",
        ):
            retry_until_deadline(
                task=mock_task,
                deadline=Deadline.from_duration(timedelta(seconds=10)),
                retry_delay=timedelta(seconds=1),
            )

        mock_task.assert_called_once()
        mock_sleep.assert_not_called()

    @mock.patch("honeydew.utils.control_flows.sleep_for_duration")
    def test_retry_until_deadline_succeeds_on_retry(
        self, mock_sleep: mock.Mock
    ) -> None:
        mock_task = mock.Mock(
            side_effect=2 * [RuntimeError("Triggers a retry")] + [True]
        )
        retry_until_deadline(
            task=mock_task,
            deadline=Deadline.from_duration(timedelta(seconds=10)),
            retry_delay=timedelta(seconds=1),
        )

        self.assertEqual(mock_task.call_count, 3)
        self.assertEqual(mock_sleep.call_count, 2)

    @mock.patch("time.sleep")
    @mock.patch("honeydew.utils.control_flows.datetime", wraps=datetime)
    def test_retry_with_sleep(
        self, mock_datetime: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        mock_datetime.now.return_value = datetime(2024, 7, 10)

        def advance_now(delta: float) -> None:
            mock_datetime.now.return_value += timedelta(seconds=delta)

        mock_sleep.side_effect = advance_now

        mock_task = mock.Mock(
            side_effect=[RuntimeError("Triggers a retry"), True]
        )
        deadline = Deadline.from_duration(timedelta(seconds=100))

        expected_sleep_duration = 45
        retry_until_deadline(
            task=mock_task,
            deadline=deadline,
            retry_delay=timedelta(seconds=expected_sleep_duration),
        )

        self.assertEqual(mock_task.call_count, 2)
        self.assertEqual(mock_sleep.call_count, 1)
        mock_sleep.assert_called_once_with(expected_sleep_duration)

    @mock.patch("time.sleep")
    @mock.patch("honeydew.utils.control_flows.datetime", wraps=datetime)
    def test_retry_with_backoff(
        self, mock_datetime: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        mock_datetime.now.return_value = datetime(2024, 7, 10)

        def advance_now(delta: float) -> None:
            mock_datetime.now.return_value += timedelta(seconds=delta)

        mock_sleep.side_effect = advance_now

        mock_task = mock.Mock(
            side_effect=4 * [RuntimeError("Triggers a retry")] + [True]
        )
        deadline = Deadline.from_duration(timedelta(seconds=100))
        retry_until_deadline(
            task=mock_task,
            deadline=deadline,
            retry_delay=timedelta(seconds=1),
            backoff=True,
        )

        self.assertEqual(mock_task.call_count, 5)
        self.assertEqual(mock_sleep.call_count, 4)

        expected_sleep_durations = [1, 2, 4, 8]
        expected_sleep_calls = [
            mock.call(duration) for duration in expected_sleep_durations
        ]
        mock_sleep.assert_has_calls(expected_sleep_calls)

    @mock.patch("time.sleep")
    @mock.patch("honeydew.utils.control_flows.datetime", wraps=datetime)
    def test_retry_hits_backoff_cap(
        self, mock_datetime: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        mock_datetime.now.return_value = datetime(2024, 7, 10)

        def advance_now(delta: float) -> None:
            mock_datetime.now.return_value += timedelta(seconds=delta)

        mock_sleep.side_effect = advance_now

        mock_task = mock.Mock(
            side_effect=4 * [RuntimeError("Triggers a retry")] + [True]
        )
        deadline = Deadline.from_duration(
            timedelta(seconds=25 + 50 + 100 + 200)
        )
        retry_until_deadline(
            task=mock_task,
            deadline=deadline,
            retry_delay=timedelta(seconds=25),
            backoff=True,
        )

        self.assertEqual(mock_task.call_count, 5)

        expected_sleep_durations = [
            25,
            50,
            60,
            40,
            60,
            60,
            60,
            20,
        ]
        expected_sleep_calls = [
            mock.call(float(duration)) for duration in expected_sleep_durations
        ]
        mock_sleep.assert_has_calls(expected_sleep_calls)

    @mock.patch("honeydew.utils.control_flows.sleep_for_duration")
    @mock.patch("honeydew.utils.control_flows.datetime", wraps=datetime)
    def test_retry_until_deadline_never_succeeds(
        self, mock_datetime: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        mock_task = mock.Mock(side_effect=RuntimeError("Triggers a retry"))
        mock_datetime.now.return_value = datetime(2024, 7, 10)
        deadline = Deadline.from_duration(timedelta(seconds=10))

        # Script datetime.now() to jump past the deadline after the task triggers twice
        def sleep_func(d: timedelta) -> None:
            if mock_task.call_count == 2:
                mock_datetime.now.return_value += timedelta(seconds=20)

        mock_sleep.side_effect = sleep_func

        with self.assertRaisesRegex(
            expected_exception=RuntimeError,
            expected_regex="Triggers a retry",
        ):
            retry_until_deadline(
                task=mock_task,
                deadline=deadline,
                retry_delay=timedelta(seconds=1),
            )

        self.assertEqual(mock_task.call_count, 3)
        self.assertEqual(mock_sleep.call_count, 2)


class RepeatTest(unittest.TestCase):
    @mock.patch("honeydew.utils.control_flows.sleep_for_duration")
    @mock.patch("honeydew.utils.control_flows.datetime", wraps=datetime)
    def test_repeat_until_deadline(
        self, mock_datetime: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        mock_task = mock.Mock()
        mock_datetime.now.return_value = datetime(2024, 7, 10)
        deadline = Deadline.from_duration(timedelta(seconds=10))

        def advance_now(delta: timedelta) -> None:
            mock_datetime.now.return_value += delta

        mock_sleep.side_effect = advance_now

        repeat_until_deadline(
            task=mock_task,
            deadline=deadline,
            repeat_delay=timedelta(seconds=1),
        )

        self.assertEqual(mock_task.call_count, 10)
        self.assertEqual(mock_sleep.call_count, 9)

    @mock.patch("honeydew.utils.control_flows.sleep_for_duration")
    def test_repeat_until_deadline_when_already_due(
        self, mock_sleep: mock.Mock
    ) -> None:
        mock_task = mock.Mock()
        deadline = Deadline.from_duration(timedelta(seconds=0))

        repeat_until_deadline(
            task=mock_task,
            deadline=deadline,
            repeat_delay=timedelta(seconds=1),
        )

        mock_task.assert_not_called()
        mock_sleep.assert_not_called()

    @mock.patch("honeydew.utils.control_flows.sleep_for_duration")
    @mock.patch("honeydew.utils.control_flows.datetime", wraps=datetime)
    def test_repeat_until_deadline_raises_exception(
        self, mock_datetime: mock.Mock, mock_sleep: mock.Mock
    ) -> None:
        mock_task = mock.Mock(
            side_effect=4 * [None] + [RuntimeError("Expected")]
        )
        mock_datetime.now.return_value = datetime(2024, 7, 10)
        deadline = Deadline.from_duration(timedelta(seconds=10))

        def advance_now(delta: timedelta) -> None:
            mock_datetime.now.return_value += delta

        mock_sleep.side_effect = advance_now

        with self.assertRaisesRegex(RuntimeError, "Expected"):
            repeat_until_deadline(
                task=mock_task,
                deadline=deadline,
                repeat_delay=timedelta(seconds=1),
            )

        self.assertEqual(mock_task.call_count, 5)
        self.assertEqual(mock_sleep.call_count, 4)


if __name__ == "__main__":
    unittest.main()

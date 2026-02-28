# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Tests for deadline.py"""

import unittest
from datetime import datetime, timedelta
from unittest import mock

from honeydew.utils.deadline import Deadline


class DeadlineTest(unittest.TestCase):
    def setUp(self) -> None:
        return super().setUp()

    @mock.patch("honeydew.utils.deadline.datetime", wraps=datetime)
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


if __name__ == "__main__":
    unittest.main()

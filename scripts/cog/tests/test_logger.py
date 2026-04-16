# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest
from unittest.mock import patch

import logger


class TestLogger(unittest.TestCase):
    @patch("builtins.print")
    def test_emit_status(self, mock_print: unittest.mock.MagicMock) -> None:
        # Enable status updates and verify that emit_status() prints to stdout.
        logger.init_logger(enable_status_updates=True)
        logger.emit_status("test message")
        mock_print.assert_called_once_with("STATUS_UPDATE:test message")

        # Disable status updates and verify that emit_status() does not print.
        mock_print.reset_mock()
        logger.init_logger(enable_status_updates=False)
        logger.emit_status("another message")
        mock_print.assert_not_called()

        # Re-enable status updates to ensure it still works.
        mock_print.reset_mock()
        logger.init_logger(enable_status_updates=True)
        logger.emit_status("third message")
        mock_print.assert_called_once_with("STATUS_UPDATE:third message")

    def test_set_level(self) -> None:
        import logging

        logger.init_logger(level=logging.WARNING)
        self.assertEqual(logger.get_log_level(), logging.WARNING)

        with logger.set_level(logging.DEBUG):
            self.assertEqual(logger.get_log_level(), logging.DEBUG)

        self.assertEqual(logger.get_log_level(), logging.WARNING)

        # Test with min
        with logger.set_level(min(logger.get_log_level(), logging.INFO)):
            self.assertEqual(logger.get_log_level(), logging.INFO)

        logger.init_logger(level=logging.DEBUG)
        with logger.set_level(min(logger.get_log_level(), logging.INFO)):
            self.assertEqual(logger.get_log_level(), logging.DEBUG)


if __name__ == "__main__":
    unittest.main()

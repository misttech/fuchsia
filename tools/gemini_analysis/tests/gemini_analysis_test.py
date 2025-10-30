#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import io
import json
import sys
import unittest
from contextlib import redirect_stdout
from unittest.mock import MagicMock, mock_open, patch

# add the script's directory to the python path
sys.path.append("tools/gemini_analysis")
import gemini_analysis


class GeminiAnalysisTest(unittest.TestCase):
    @patch(
        "sys.argv",
        ["gemini_analysis.py", "--api-key", "test-key", "--verbosity", "1"],
    )
    @patch("gemini_analysis._blocking_gemini_call")
    @patch("gemini_analysis.subprocess.run")
    def test_verbosity_1(
        self,
        mock_subprocess_run: MagicMock,
        mock_gemini_call: MagicMock,
    ) -> None:
        # setup mocks
        mock_gemini_call.return_value = gemini_analysis.GeminiAnalysisResult(
            text="path/to/file.rs:123"
        )
        sys.stdin = io.StringIO("mock error log with path/to/file.rs:123")
        captured_output = io.StringIO()
        with redirect_stdout(captured_output):
            gemini_analysis.main()

        output = captured_output.getvalue()
        self.assertIn("--- Gemini Failure Analysis ---", output)
        self.assertIn("## KEY LINES", output)
        self.assertIn("path/to/file.rs:123", output)
        mock_subprocess_run.assert_not_called()  # git diff should not be called

    @patch(
        "sys.argv",
        ["gemini_analysis.py", "--api-key", "test-key", "--verbosity", "2"],
    )
    @patch("gemini_analysis._blocking_gemini_call")
    @patch("gemini_analysis.subprocess.run")
    def test_verbosity_2(
        self,
        mock_subprocess_run: MagicMock,
        mock_gemini_call: MagicMock,
    ) -> None:
        # setup mocks
        mock_subprocess_run.return_value.stdout = "mock git diff"
        mock_gemini_call.return_value = gemini_analysis.GeminiAnalysisResult(
            text="## KEY LINES\nkey line\n## POTENTIAL ERROR\nmock error"
        )
        sys.stdin = io.StringIO("mock error log")
        captured_output = io.StringIO()
        with redirect_stdout(captured_output):
            gemini_analysis.main()

        output = captured_output.getvalue()
        self.assertIn("--- Gemini Failure Analysis ---", output)
        self.assertIn("## POTENTIAL ERROR", output)
        mock_subprocess_run.assert_called_once()
        mock_gemini_call.assert_called_once()
        call_args = json.loads(mock_gemini_call.call_args_list[0].args[2])
        self.assertIn(
            "mock git diff", call_args["contents"][0]["parts"][0]["text"]
        )

    @patch(
        "sys.argv",
        ["gemini_analysis.py", "--api-key", "test-key", "--verbosity", "3"],
    )
    @patch("gemini_analysis._blocking_gemini_call")
    @patch("gemini_analysis.os.path.getsize")
    @patch("gemini_analysis.os.path.isfile")
    @patch(
        "builtins.open", new_callable=mock_open, read_data=b"mock file content"
    )
    @patch("gemini_analysis.subprocess.run")
    def test_verbosity_3_successful_analysis_with_file_request(
        self,
        mock_subprocess_run: MagicMock,
        mock_open_file: MagicMock,
        mock_isfile: MagicMock,
        mock_getsize: MagicMock,
        mock_gemini_call: MagicMock,
    ) -> None:
        # this is the original test, now adapted for verbosity 3
        # setup mocks
        mock_subprocess_run.return_value.stdout = "mock git diff"
        mock_isfile.return_value = True
        mock_getsize.return_value = 100  # Mock file size to be under the limit
        mock_gemini_call.side_effect = [
            gemini_analysis.GeminiAnalysisResult('["mock_file.py"]'),
            gemini_analysis.GeminiAnalysisResult(
                "## ROOT CAUSE ANALYSIS\nThis is a mock analysis."
            ),
        ]
        sys.stdin = io.StringIO("mock error log")
        captured_output = io.StringIO()
        with redirect_stdout(captured_output):
            gemini_analysis.main()

        output = captured_output.getvalue()
        self.assertIn("--- Gemini Failure Analysis ---", output)
        self.assertIn("## ROOT CAUSE ANALYSIS", output)
        self.assertEqual(mock_gemini_call.call_count, 2)
        first_call_args = json.loads(mock_gemini_call.call_args_list[0].args[2])
        self.assertIn(
            "mock git diff", first_call_args["contents"][0]["parts"][0]["text"]
        )
        second_call_args = json.loads(
            mock_gemini_call.call_args_list[1].args[2]
        )
        self.assertIn(
            "Content of file 'mock_file.py':",
            second_call_args["contents"][0]["parts"][0]["text"],
        )

    @patch(
        "sys.argv",
        ["gemini_analysis.py", "--api-key", "test-key", "--verbosity", "3"],
    )
    @patch("gemini_analysis._blocking_gemini_call")
    @patch("gemini_analysis.os.path.getsize")
    @patch("gemini_analysis.os.path.isfile")
    @patch("builtins.open", new_callable=mock_open)
    @patch("gemini_analysis.subprocess.run")
    def test_verbosity_3_unreadable_file(
        self,
        mock_subprocess_run: MagicMock,
        mock_open_file: MagicMock,
        mock_isfile: MagicMock,
        mock_getsize: MagicMock,
        mock_gemini_call: MagicMock,
    ) -> None:
        # Mocks
        mock_subprocess_run.return_value.stdout = "mock git diff"
        mock_isfile.return_value = True
        mock_getsize.return_value = 100
        mock_open_file.side_effect = IOError("Permission denied")
        mock_gemini_call.side_effect = [
            gemini_analysis.GeminiAnalysisResult('["unreadable_file.txt"]'),
            gemini_analysis.GeminiAnalysisResult(
                "## ROOT CAUSE ANALYSIS\nMock analysis."
            ),
        ]
        sys.stdin = io.StringIO("mock error log")

        # Capture stderr and stdout
        captured_output = io.StringIO()
        captured_error = io.StringIO()
        with redirect_stdout(captured_output), patch(
            "sys.stderr", captured_error
        ):
            gemini_analysis.main()

        # Assertions
        self.assertIn(
            "--- Gemini Failure Analysis ---", captured_output.getvalue()
        )
        self.assertIn("Warning: Could not read file", captured_error.getvalue())

        # Check that the file content was not included in the final prompt
        second_call_args = json.loads(
            mock_gemini_call.call_args_list[1].args[2]
        )
        self.assertNotIn(
            "unreadable_file.txt",
            second_call_args["contents"][0]["parts"][0]["text"],
        )

    @patch("sys.argv", ["gemini_analysis.py", "--api-key", "test-key"])
    @patch("gemini_analysis._blocking_gemini_call")
    @patch("gemini_analysis.subprocess.run")
    def test_api_error(
        self,
        mock_subprocess_run: MagicMock,
        mock_gemini_call: MagicMock,
    ) -> None:
        # setup mocks
        mock_subprocess_run.return_value.stdout = ""
        mock_subprocess_run.return_value.stderr = ""
        mock_gemini_call.return_value = gemini_analysis.GeminiAnalysisResult(
            text="Error: API call failed", error=True
        )
        sys.stdin = io.StringIO("mock error log")

        # capture stderr
        captured_error = io.StringIO()
        sys.stderr = captured_error

        # run and assert
        with self.assertRaises(SystemExit):
            gemini_analysis.main()

        # reset stderr
        sys.stderr = sys.__stderr__

        self.assertIn("Error: API call failed", captured_error.getvalue())

    @patch(
        "sys.argv",
        [
            "gemini_analysis.py",
            "--api-key",
            "test-key",
            "--gemini-model",
            "test-model",
        ],
    )
    @patch("gemini_analysis._blocking_gemini_call")
    @patch("gemini_analysis.subprocess.run")
    def test_gemini_model_cli_arg(
        self,
        mock_subprocess_run: MagicMock,
        mock_gemini_call: MagicMock,
    ) -> None:
        # setup mocks
        mock_subprocess_run.return_value.stdout = ""
        mock_subprocess_run.return_value.stderr = ""
        mock_gemini_call.return_value = gemini_analysis.GeminiAnalysisResult(
            text="mock response"
        )
        sys.stdin = io.StringIO("mock error log")
        captured_output = io.StringIO()
        with redirect_stdout(captured_output):
            gemini_analysis.main()

        mock_gemini_call.assert_called_once()
        self.assertEqual(
            mock_gemini_call.call_args_list[0].args[1], "test-model"
        )


if __name__ == "__main__":
    unittest.main()

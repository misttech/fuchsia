#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import json
import os
import subprocess
import sys
from dataclasses import dataclass
from urllib import request

_PROMPT_VERBOSITY_1 = (
    "You are an expert Fuchsia OS developer's assistant.\n"
    "Your task is to analyze a test failure stack trace and extract only the\n"
    "most important lines that indicate the root cause.\n"
    "Do not add any commentary or explanation. Only return the key lines from\n"
    "the stack trace.\n"
    "\n"
    "Stack Trace:\n"
    "{stack_trace}\n"
)

_PROMPT_VERBOSITY_2 = (
    "\nYou are an expert Fuchsia OS developer's assistant. Your task is to\n"
    "analyze a test failure and provide a concise, standardized debugging\n"
    "report. Format your entire response for a plain text terminal. Do not use\n"
    "any markdown.\n"
    "\n"
    "Your response MUST strictly follow this format, using these exact headers:\n"
    "\n"
    "## KEY LINES\n"
    "Directly quote the most relevant lines from the stack trace that pinpoint\n"
    "the error. Preserve the original formatting exactly. Do not add any\n"
    "commentary in this section.\n"
    "\n"
    "## POTENTIAL ERROR\n"
    "Analyze the stack trace and git diff to determine the most likely root\n"
    "cause.\n"
    "\n"
    "---\n"
    "CONTEXT\n"
    "---\n"
    "\n"
    "Stack Trace:\n"
    "{stack_trace}\n"
    "\n"
    "Git Diff:\n"
    "{git_diff}\n"
)

_PROMPT_VERBOSITY_3_ASK_FOR_FILES = (
    "\nYou are an automated debugging assistant. Analyze the provided stack\n"
    "trace, git diff, and current working directory (PWD).\n"
    "Your task is to identify which files, if any, you need to read to provide\n"
    "a complete diagnosis.\n"
    "Respond ONLY with a valid JSON array of strings, where each string is a\n"
    "file path relative to the PWD.\n"
    'Example: ["src/foo/bar.cc", "BUILD.gn"]\n'
    "If you do not need to read any files, respond with an empty array: [].\n"
    "\n"
    "PWD: {pwd}\n"
    "---\n"
    "STACK TRACE:\n"
    "{stack_trace}\n"
    "---\n"
    "GIT DIFF:\n"
    "{git_diff}\n"
    "---\n"
)

_PROMPT_VERBOSITY_3_PERFORM_ANALYSIS = (
    "\nYou are an expert Fuchsia OS developer's assistant. Your task is to\n"
    "analyze a test failure and provide a concise, standardized debugging\n"
    "report. Format your entire response for a plain text terminal. Do not use\n"
    "any markdown.\n"
    "\n"
    "Your response MUST strictly follow this format, using these exact headers:\n"
    "\n"
    "## KEY LINES\n"
    "Directly quote the most relevant lines from the stack trace that pinpoint\n"
    "the error. Preserve the original formatting exactly. Do not add any\n"
    "commentary in this section. Try to include lines that include a path, so\n"
    "users can click to it in the terminal as well. Even if there's a clear\n"
    "error message, try to include the part of the stack trace that relates to\n"
    "it.\n"
    "\n"
    "## ROOT CAUSE ANALYSIS\n"
    "Analyze the stack trace, git diff, and any provided file contents to\n"
    "determine the most likely root cause. Clearly state the error type (e.g.,\n"
    "Null Pointer Exception, Race Condition, Assertion Failure) and explain the\n"
    "logic that leads to the failure.\n"
    "\n"
    "## SUGGESTED FIX\n"
    "Provide a concise, best-practice code suggestion to resolve the issue. If\n"
    "the fix is uncertain, suggest the most logical next step for debugging\n"
    "(e.g., \"Add a log statement to check the value of 'my_var' before the\n"
    "call to 'do_thing()'\").\n"
    "\n"
    "---\n"
    "CONTEXT\n"
    "---\n"
    "\n"
    "Stack Trace:\n"
    "{stack_trace}\n"
    "\n"
    "Git Diff:\n"
    "{diff_section}\n"
    "\n"
    "File Contents:\n"
    "{file_contents_context}\n"
)


@dataclass
class GeminiAnalysisResult:
    """A structured result from a call to the Gemini API."""

    text: str
    error: bool = False


def _blocking_gemini_call(
    api_key: str, gemini_model: str, data: bytes
) -> GeminiAnalysisResult:
    """synchronous function to make the web request."""
    url = f"https://generativelanguage.googleapis.com/v1beta/models/{gemini_model}:generateContent?key={api_key}"
    headers = {"Content-Type": "application/json"}
    req = request.Request(url, data=data, headers=headers, method="POST")
    try:
        with request.urlopen(req, timeout=45) as response:
            if response.status < 200 or response.status >= 300:
                return GeminiAnalysisResult(
                    f"API Error: {response.status} {response.reason}",
                    error=True,
                )
            response_body = response.read().decode("utf-8")
            response_json = json.loads(response_body)

            candidates = response_json.get("candidates", [])
            if candidates:
                content = candidates[0].get("content", {})
                parts = content.get("parts", [])
                if parts:
                    return GeminiAnalysisResult(
                        parts[0].get(
                            "text",
                            "Error: Could not extract text from response.",
                        )
                    )
            return GeminiAnalysisResult(
                "Error: Unexpected response format.", error=True
            )
    except Exception as e:
        return GeminiAnalysisResult(f"Error during API call: {e}", error=True)


def _call_gemini_with_prompt(
    api_key: str, gemini_model: str, prompt: str, **kwargs: str
) -> GeminiAnalysisResult:
    """Formats a prompt, creates a payload, and calls the Gemini API."""
    formatted_prompt = prompt.format(**kwargs)
    payload = {"contents": [{"parts": [{"text": formatted_prompt}]}]}
    data = json.dumps(payload).encode("utf-8")
    return _blocking_gemini_call(api_key, gemini_model, data)


def _read_files_for_gemini(requested_files: list[str]) -> str:
    if not requested_files:
        return ""
    file_contents = {}
    for file_path in requested_files:
        if not os.path.isfile(file_path):
            error_text = f"Warning: Path '{file_path}' is not a valid file."
            print(f"Gemini analysis warning: {error_text}", file=sys.stderr)
            continue

        try:
            # Skip files larger than 1MB.
            if os.path.getsize(file_path) > 1_000_000:
                error_text = f"Warning: File '{file_path}' is too large (>1MB), skipping."
                print(f"Gemini analysis warning: {error_text}", file=sys.stderr)
                continue

            # Read in binary mode to check for null bytes (binary indicator). This is a heuristic and may not always be accurate.
            with open(file_path, "rb") as f:
                raw_content = f.read()

            if b"\0" in raw_content:
                error_text = f"Warning: File '{file_path}' appears to be binary, skipping."
                print(f"Gemini analysis warning: {error_text}", file=sys.stderr)
                continue

            # Decode as UTF-8 if not binary.
            content = raw_content.decode("utf-8", errors="replace")
            file_contents[file_path] = content
        except Exception as e:
            error_text = (
                f"Warning: Could not read file at '{file_path}': {e}, skipping."
            )
            print(f"Gemini analysis warning: {error_text}", file=sys.stderr)

    context_block = "\nADDITIONAL FILE CONTEXT:\n---\n"
    for path, content in file_contents.items():
        context_block += f"Content of file '{path}':\n{content}\n---\n"
    return context_block


def get_gemini_analysis(
    api_key: str,
    gemini_model: str,
    stack_trace: str,
    git_diff: str | None,
    pwd: str,
    verbosity: int,
) -> GeminiAnalysisResult:
    """Analyzes a test failure with Gemini at a specified verbosity level."""
    if verbosity == 1:
        result = _call_gemini_with_prompt(
            api_key, gemini_model, _PROMPT_VERBOSITY_1, stack_trace=stack_trace
        )
        if not result.error:
            result.text = f"## KEY LINES\n{result.text}\n"
        return result

    if verbosity == 2:
        return _call_gemini_with_prompt(
            api_key,
            gemini_model,
            _PROMPT_VERBOSITY_2,
            stack_trace=stack_trace,
            git_diff=git_diff or "No local changes.",
        )

    # Verbosity level 3
    file_list_result = _call_gemini_with_prompt(
        api_key,
        gemini_model,
        _PROMPT_VERBOSITY_3_ASK_FOR_FILES,
        pwd=pwd,
        stack_trace=stack_trace,
        git_diff=git_diff or "No local changes.",
    )

    if file_list_result.error:
        return file_list_result

    requested_files = []
    try:
        cleaned_response = (
            file_list_result.text.strip()
            .removeprefix("```json")
            .removesuffix("```")
            .strip()
        )
        parsed_json = json.loads(cleaned_response)
        if isinstance(parsed_json, list):
            requested_files = [str(item) for item in parsed_json]
    except (json.JSONDecodeError, TypeError):
        print(
            f"Could not parse file list from Gemini: {file_list_result.text}",
            file=sys.stderr,
        )

    file_contents_context = _read_files_for_gemini(requested_files)
    diff_section = (
        f"Recent code changes that might be related (git diff HEAD):\n---\n{git_diff}\n---\n"
        if git_diff
        else ""
    )

    return _call_gemini_with_prompt(
        api_key,
        gemini_model,
        _PROMPT_VERBOSITY_3_PERFORM_ANALYSIS,
        stack_trace=stack_trace,
        diff_section=diff_section,
        file_contents_context=file_contents_context,
    )


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Analyze test failures with Gemini."
    )
    parser.add_argument("--api-key", required=True, help="Gemini API key.")
    parser.add_argument(
        "--gemini-model",
        default="gemini-2.5-flash-lite-preview-09-2025",
        help="The Gemini model to use for the analysis.",
    )
    parser.add_argument(
        "--verbosity",
        type=int,
        default=2,
        choices=range(1, 4),
        help="Verbosity level (1-3).",
    )
    args = parser.parse_args()

    # run git diff and capture the output
    git_diff = None
    if args.verbosity > 1:
        try:
            result = subprocess.run(
                ["git", "diff", "HEAD"],
                capture_output=True,
                text=True,
                check=False,
            )
            if result.stdout:
                git_diff = result.stdout
            if result.stderr:
                print("--- git diff stderr ---", file=sys.stderr)
                print(result.stderr, file=sys.stderr)
        except FileNotFoundError:
            print("git command not found, skipping git diff.", file=sys.stderr)
        except Exception as e:
            print(
                f"An error occurred while running git diff: {e}",
                file=sys.stderr,
            )

    # read stdin for the error log
    error_log = sys.stdin.read()

    # perform gemini analysis
    pwd = os.getcwd()
    analysis = get_gemini_analysis(
        args.api_key,
        args.gemini_model,
        error_log,
        git_diff,
        pwd,
        args.verbosity,
    )

    # print all results
    if analysis.error:
        print(analysis.text, file=sys.stderr)
        sys.exit(1)
    else:
        print("--- Gemini Failure Analysis ---")
        print(analysis.text)


if __name__ == "__main__":
    main()

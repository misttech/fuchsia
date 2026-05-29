# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os


def get_fuchsia_dir() -> str:
    """Resolves the root Fuchsia directory path."""
    if "FUCHSIA_DIR" in os.environ:
        return os.environ["FUCHSIA_DIR"]
    script_dir = os.path.dirname(os.path.abspath(__file__))
    return os.path.abspath(os.path.join(script_dir, "../../../../"))


def read_file(file_path: str) -> str:
    """Reads the content of a file at the given path.

    Args:
        file_path: The absolute path to the file to read.
    """
    # Clean up potential weird quoting from the model
    file_path = file_path.strip("'\"")
    # The local Gemma model occasionally includes tokenizer control/special tokens
    # (specifically `<|"|>`) when generating string arguments for tool calls.
    # We strip them to ensure the file path resolves correctly.
    file_path = file_path.replace('<|"|>', "")

    # Expand ~ to home directory and resolve absolute path
    file_path = os.path.abspath(os.path.expanduser(file_path))

    # Restrict file access to the Fuchsia checkout workspace
    fuchsia_dir = get_fuchsia_dir()
    try:
        if os.path.commonpath([fuchsia_dir, file_path]) != fuchsia_dir:
            return f"Error: Access denied. Path {file_path} is outside the Fuchsia checkout {fuchsia_dir}."
    except ValueError:
        return f"Error: Access denied. Path {file_path} is outside the Fuchsia checkout {fuchsia_dir}."

    if not os.path.exists(file_path):
        return f"Error: File not found at {file_path}"
    try:
        with open(file_path, "r", encoding="utf-8") as f:
            return f.read()
    except Exception as e:
        return f"Error reading file: {e}"


system_instruction = (
    "You are a helpful assistant. When given a file path in the prompt, "
    "use the read_file tool to read its content. Then, you MUST extract keywords "
    "for categorizing the document and generate a single-sentence description "
    "of the content. Your output MUST strictly follow this format:\n"
    "Description: <single-sentence description>\n"
    "Keywords: <comma-separated list of keywords>\n"
    "Return ONLY the description and keywords, with no additional conversational filler."
)
tools = [read_file]

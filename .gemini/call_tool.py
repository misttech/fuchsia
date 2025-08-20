#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Tool Call Script.

This script receives the tool name as the first argument
and the tool's arguments as a JSON object on stdin.
It must return a JSON object with an "output" key on stdout.
"""

import json
import subprocess
import sys


class GitLsFiles:
    """Lists files in a git repository."""

    def name(self):
        return "git_ls_files"

    def discover(self):
        return {
            "name": self.name(),
            "description": "Custom implementation: List files in a git repository directory using `git ls-files` and returns the exact output.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "files": {
                        "type": "ARRAY",
                        "items": {"type": "STRING"},
                        "description": "The shell glob to search for. Defaults to all files",
                    },
                    "path": {
                        "type": "STRING",
                        "description": "An optional path to a directory to search within. Defaults to the current directory.",
                    },
                },
                "required": ["path"],
            },
        }

    def run(self, args):
        search_path = args.get("path", ".")
        files = args.get("files", [])
        try:
            output = subprocess.check_output(
                ["git", "ls-files"] + files,
                cwd=search_path,
                stderr=subprocess.STDOUT,
            ).decode("utf-8")
        except subprocess.CalledProcessError as e:
            output = e.output.decode("utf-8")
        return output


class JiriGrep:
    """Searches for a pattern across all repositories."""

    def name(self):
        return "jiri_grep"

    def discover(self):
        return {
            "name": self.name(),
            "description": "Custom implementation: Searches for a regular expression pattern across all repositories using `jiri grep` and returns the exact output.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "pattern": {
                        "type": "STRING",
                        "description": "The regular expression (regex) to search for.",
                    },
                    "path": {
                        "type": "STRING",
                        "description": "An optional path to a directory to search within. Defaults to the current directory.",
                    },
                },
                "required": ["pattern"],
            },
        }

    def run(self, args):
        pattern = args.get("pattern")
        search_path = args.get("path", ".")
        try:
            output = subprocess.check_output(
                ["jiri", "grep", "-n", pattern, "--", search_path],
                stderr=subprocess.STDOUT,
            ).decode("utf-8")
        except subprocess.CalledProcessError as e:
            output = e.output.decode("utf-8")
        return output


def print_json_output(output):
    print(json.dumps({"output": output}))


def main():
    """Main function."""
    # Add your new tool here!
    tools_list = [GitLsFiles(), JiriGrep()]
    tools = {tool.name(): tool for tool in tools_list}

    tool_name = sys.argv[1]
    if tool_name == "discover":
        print(json.dumps([tool.discover() for tool in tools.values()]))
        return

    if tool_name in tools:
        args_json = sys.stdin.read()
        args = json.loads(args_json)
        print_json_output(tools[tool_name].run(args))
    else:
        print_json_output(f"Error: Unknown tool name '{tool_name}'")
        sys.exit(1)


if __name__ == "__main__":
    main()

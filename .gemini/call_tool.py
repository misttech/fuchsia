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
import os
import shutil
import subprocess
import sys
import time


class Screenshot:
    """Takes a screenshot of the device."""

    def name(self):
        return "screenshot"

    def discover(self):
        return {
            "name": self.name(),
            "description": "Custom implementation: Takes a screenshot of the device using `ffx screenshot` and saves it to a file. Gemini can read this file to analyze the screenshot and see the screen.",
            "parameters": {
                "type": "OBJECT",
                "properties": {
                    "target": {
                        "type": "STRING",
                        "description": "The name of the target device to screenshot. If not specified, it will be inferred if there is only one available.",
                    }
                },
            },
        }

    def run(self, args):
        try:
            target_info_json = subprocess.check_output(
                ["ffx", "target", "list", "-f", "j"],
                stderr=subprocess.STDOUT,
            )
            all_targets = json.loads(target_info_json)
            rcs_targets = [t for t in all_targets if t.get("rcs_state") == "Y"]
        except (subprocess.CalledProcessError, json.JSONDecodeError) as e:
            return f"Error getting target list: {e}"

        target_to_use = None
        specified_target_name = args.get("target")

        if specified_target_name:
            for t in all_targets:
                if t.get("nodename") == specified_target_name:
                    if t.get("rcs_state") == "Y":
                        target_to_use = t
                        break
                    else:
                        return f"Error: Target '{specified_target_name}' found, but RCS is not active."
            if not target_to_use:
                return f"Error: Specified target '{specified_target_name}' not found."
        elif len(rcs_targets) == 0:
            return "Error: No targets with RCS found."
        elif len(rcs_targets) > 1:
            available_targets = ", ".join(
                [t.get("nodename", "unknown") for t in rcs_targets]
            )
            return f"Error: Multiple targets with RCS found. Please specify one: {available_targets}"
        else:
            target_to_use = rcs_targets[0]

        device_type = target_to_use.get("target_type", "unknown")
        target_name = target_to_use.get("nodename")

        repo_root = os.path.dirname(os.path.dirname(os.path.realpath(__file__)))
        screenshots_dir = os.path.join(repo_root, "local", "screenshots")
        os.makedirs(screenshots_dir, exist_ok=True)
        timestamp = time.strftime("%Y%m%d_%H%M%S")
        temp_dir = f"/tmp/screenshot-{timestamp}"
        filename = f"screenshot-{device_type}-{timestamp}.png"
        final_path = os.path.join(screenshots_dir, filename)

        try:
            os.makedirs(temp_dir, exist_ok=True)
            command = [
                "ffx",
                "-t",
                target_name,
                "target",
                "screenshot",
                "--format",
                "png",
                "-d",
                temp_dir,
            ]
            subprocess.check_output(
                command,
                stderr=subprocess.STDOUT,
            ).decode("utf-8")
            temp_file = os.path.join(temp_dir, "screenshot.png")
            shutil.move(temp_file, final_path)
            os.rmdir(temp_dir)
            return f"Screenshot saved to {final_path}"
        except (subprocess.CalledProcessError, OSError) as e:
            error_output = (
                e.output.decode("utf-8") if hasattr(e, "output") else str(e)
            )
            return f"Error taking screenshot: {error_output}"


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
    tools_list = [GitLsFiles(), JiriGrep(), Screenshot()]
    tools = {tool.name(): tool for tool in tools_list}

    tool_name = sys.argv[1]
    if tool_name == "discover":
        print(json.dumps([tool.discover() for tool in tools.values()]))
        return

    if tool_name in tools:
        args_json = sys.stdin.read()
        args = {}
        if args_json:
            args = json.loads(args_json)
        print_json_output(tools[tool_name].run(args))
    else:
        print_json_output(f"Error: Unknown tool name '{tool_name}'")
        sys.exit(1)


if __name__ == "__main__":
    main()

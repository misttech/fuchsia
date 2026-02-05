# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import re
import subprocess
import sys


def get_change_id():
    """Extracts the Change-Id from the HEAD commit message."""
    try:
        # Get the commit message of HEAD
        result = subprocess.run(
            ["git", "log", "-n", "1"],
            capture_output=True,
            text=True,
            check=True,
        )

        # Look for Change-Id: I...
        match = re.search(r"Change-Id: (I[a-f0-9]+)", result.stdout)
        if match:
            return match.group(1)
        return None
    except subprocess.CalledProcessError:
        print("Error: Failed to run git log.")
        sys.exit(1)


def get_change_details(change_id):
    """Queries Gerrit for the change details using the Change-Id."""
    base_url = "https://fuchsia-review.googlesource.com"
    query_url = f"{base_url}/changes/?q=change:{change_id}&o=DETAILED_ACCOUNTS"

    try:
        # Use simple curl. If you need authentication (for internal changes),
        # ensure your .netrc is configured or use gob-curl if available.
        cmd = ["curl", "-L", "-s", query_url]
        result = subprocess.run(cmd, capture_output=True, text=True, check=True)

        content = result.stdout
        # Strip XSSI protection prefix
        if content.startswith(")]}'"):
            content = content[4:]

        data = json.loads(content)
        if not data:
            return None, None

        # Return the first match.
        # Data is a list of changes.
        change = data[0]
        return base_url, change
    except Exception as e:
        print(f"Error fetching change details: {e}")
        sys.exit(1)


def get_comments(base_url, change_number):
    """Fetches comments for the specific change number."""
    url = f"{base_url}/changes/{change_number}/comments"
    try:
        cmd = ["curl", "-L", "-s", url]
        result = subprocess.run(cmd, capture_output=True, text=True, check=True)

        content = result.stdout
        if content.startswith(")]}'"):
            content = content[4:]

        return json.loads(content)
    except Exception as e:
        print(f"Error fetching comments: {e}")
        sys.exit(1)


def print_comments(comments_data):
    """Prints unresolved comments with context."""
    has_unresolved = False

    for file_path, comments in comments_data.items():
        by_id = {c["id"]: c for c in comments if "id" in c}
        threads = {}

        for c in comments:
            curr = c
            while curr.get("in_reply_to") in by_id:
                curr = by_id[curr["in_reply_to"]]
            root_id = curr.get("id")
            if not root_id:
                root_id = id(c)
            if root_id not in threads:
                threads[root_id] = []
            threads[root_id].append(c)

        unresolved_comments = []
        for thread_comments in threads.values():
            thread_comments.sort(key=lambda x: x.get("updated", ""))
            # A thread is unresolved if its *latest* comment is unresolved
            if thread_comments[-1].get("unresolved", False):
                unresolved_comments.extend(thread_comments)

        if not unresolved_comments:
            continue

        # sort comments by line
        unresolved_comments.sort(key=lambda x: x.get("line", 0))

        for comment in unresolved_comments:
            has_unresolved = True
            line = comment.get("line", "FILE")
            msg = comment.get("message", "").strip()
            author = comment.get("author", {}).get("name", "Unknown")

            print(f"\n{'='*60}")
            print(f"File: {file_path}:{line}")
            print(f"Author: {author}")
            print(f"Comment: {msg}")

            # Try to print the code line
            if isinstance(line, int) and os.path.exists(file_path):
                try:
                    with open(
                        file_path, "r", encoding="utf-8", errors="replace"
                    ) as f:
                        # Read file, get line (1-indexed)
                        lines = f.readlines()
                        if 0 <= line - 1 < len(lines):
                            code_line = lines[line - 1].strip()
                            print(f"Code: {code_line}")
                except Exception:
                    pass

    if not has_unresolved:
        print("\nNo unresolved comments found.")
    else:
        print(f"\n{'='*60}")


def main():
    print("Finding Change-Id from HEAD...")
    change_id_str = get_change_id()
    if not change_id_str:
        print("Error: Could not find Change-Id in HEAD commit message.")
        sys.exit(1)

    print(f"Found Change-Id: {change_id_str}")
    print("Fetching change details...")

    base_url, change_info = get_change_details(change_id_str)
    if not change_info:
        print(f"Error: Change {change_id_str} not found on Gerrit.")
        sys.exit(1)

    change_number = change_info.get("_number")
    print(f"Found Change {change_number} ({base_url}/c/{change_number})")

    print("Fetching comments...")
    comments_data = get_comments(base_url, change_number)

    print_comments(comments_data)


if __name__ == "__main__":
    main()

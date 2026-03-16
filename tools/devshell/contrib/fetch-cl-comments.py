# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
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


def get_change_details(base_url, change_id, use_gob_curl):
    """Queries Gerrit for the change details using the Change-Id."""
    query_url = f"{base_url}/changes/?q=change:{change_id}&o=DETAILED_ACCOUNTS"

    curl_cmd = ["gob-curl"] if use_gob_curl else ["curl"]

    try:
        # Use simple curl. If you need authentication (for internal changes),
        # ensure your .netrc is configured or use gob-curl if available.
        cmd = curl_cmd + ["-s", query_url]
        result = subprocess.run(cmd, capture_output=True, text=True, check=True)

        content = result.stdout
        # Strip XSSI protection prefix
        if content.startswith(")]}'"):
            content = content[4:]

        data = json.loads(content)
        if not data:
            return None

        # Return the first match.
        # Data is a list of changes.
        change = data[0]
        return change
    except Exception as e:
        print(f"Error fetching change details: {e}")
        sys.exit(1)


def get_remote_url():
    """Gets the git remote origin URL."""
    try:
        result = subprocess.run(
            ["git", "remote", "get-url", "origin"],
            capture_output=True,
            text=True,
            check=True,
        )
        return result.stdout.strip()
    except subprocess.CalledProcessError:
        return None


def parse_remote_url(remote_url):
    """Parses the remote URL to determine the Gerrit base URL and curl command.

    Returns:
        (base_url, use_gob_curl)
    """
    if remote_url.startswith("sso://"):
        # e.g. sso://fuchsia/fuchsia -> https://fuchsia-review.googlesource.com
        # e.g. sso://user/repo -> https://user-review.googlesource.com
        parts = remote_url[6:].split("/")
        if parts:
            host = parts[0]
            return f"https://{host}-review.googlesource.com", True
    elif remote_url.startswith("https://"):
        # e.g. https://fuchsia.googlesource.com/fuchsia -> https://fuchsia-review.googlesource.com
        if ".googlesource.com" in remote_url:
            base = remote_url.split(".googlesource.com")[0]
            # e.g. base is https://fuchsia
            return f"{base}-review.googlesource.com", False

    return None, False


def get_comments(base_url, change_number, use_gob_curl):
    """Fetches comments for the specific change number."""
    url = f"{base_url}/changes/{change_number}/comments"
    curl_cmd = ["gob-curl"] if use_gob_curl else ["curl"]
    try:
        cmd = curl_cmd + ["-s", url]
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
    parser = argparse.ArgumentParser(
        description="Fetch unresolved comments for a Gerrit change."
    )
    parser.add_argument(
        "change_url",
        nargs="?",
        help="Optional Gerrit change URL. If not provided, uses Change-Id from HEAD.",
    )
    args = parser.parse_args()

    if args.change_url:
        change_url = args.change_url
        print(f"Using provided URL: {change_url}")
        match = re.search(r"^(https?://[^/]+)(?:.*/\+/|/|/q/)(\d+)", change_url)
        if not match:
            print(
                "Error: Could not extract base URL and change number from provided URL."
            )
            sys.exit(1)

        base_url = match.group(1)
        change_number = match.group(2)
        use_gob_curl = "git.corp.google.com" in base_url

        print(f"Using Gerrit host: {base_url}")
        if use_gob_curl:
            print("Using gob-curl for authentication.")
        print(f"Found Change {change_number} ({base_url}/c/{change_number})")
    else:
        print("Finding Change-Id from HEAD...")
        change_id_str = get_change_id()
        if not change_id_str:
            print("Error: Could not find Change-Id in HEAD commit message.")
            sys.exit(1)

        print(f"Found Change-Id: {change_id_str}")

        print("Checking git remote...")
        remote_url = get_remote_url()
        if not remote_url:
            print("Error: Could not determine git remote URL.")
            sys.exit(1)

        print(f"Found Remote: {remote_url}")
        base_url, use_gob_curl = parse_remote_url(remote_url)
        if not base_url:
            print(
                f"Error: Could not determine Gerrit host from remote: {remote_url}"
            )
            sys.exit(1)

        print(f"Using Gerrit host: {base_url}")
        if use_gob_curl:
            print("Using gob-curl for authentication.")

        change_info = get_change_details(base_url, change_id_str, use_gob_curl)
        if not change_info:
            print(f"Error: Change {change_id_str} not found on Gerrit.")
            sys.exit(1)

        change_number = change_info.get("_number")
        print(f"Found Change {change_number} ({base_url}/c/{change_number})")

    print("Fetching comments...")
    comments_data = get_comments(base_url, change_number, use_gob_curl)

    print_comments(comments_data)


if __name__ == "__main__":
    main()

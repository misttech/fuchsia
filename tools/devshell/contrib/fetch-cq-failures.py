# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import json
import os
import re
import subprocess
import sys


# Try to find the bb tool
def get_bb_tool():
    try:
        # Check if it's in the path or in the prebuilts
        # First check prebuilts relative to fuchsia root
        fuchsia_dir = os.environ.get("FUCHSIA_DIR")
        if fuchsia_dir:
            bb_path = os.path.join(
                fuchsia_dir, "prebuilt", "tools", "buildbucket", "bb"
            )
            if os.path.exists(bb_path):
                return bb_path

        # Then check git rev-parse
        result = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True,
            text=True,
        )
        if result.returncode == 0:
            bb_path = os.path.join(
                result.stdout.strip(), "prebuilt", "tools", "buildbucket", "bb"
            )
            if os.path.exists(bb_path):
                return bb_path

        # Finally check system path
        result = subprocess.run(["which", "bb"], capture_output=True, text=True)
        if result.returncode == 0:
            return result.stdout.strip()

        return None
    except:
        return None


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
    """Parses the remote URL to determine the Gerrit base URL and curl command."""
    if remote_url.startswith("sso://"):
        parts = remote_url[6:].split("/")
        if parts:
            host = parts[0]
            return f"https://{host}-review.googlesource.com", True
    elif remote_url.startswith("https://"):
        if ".googlesource.com" in remote_url:
            base = remote_url.split(".googlesource.com")[0]
            return f"{base}-review.googlesource.com", False
        if "git.corp.google.com" in remote_url:
            base = remote_url.split(".git.corp.google.com")[0]
            return f"{base}-review.git.corp.google.com", True

    return None, False


def get_build_info(bb_tool, build_id):
    """Fetches details for a build using the bb tool."""
    if not bb_tool:
        return None
    try:
        # Use -A (all) flag to ensure all fields like 'steps' and 'properties' are included in the JSON.
        cmd = [bb_tool, "get", str(build_id), "-A", "-json"]
        result = subprocess.run(cmd, capture_output=True, text=True)
        if result.returncode == 0:
            return json.loads(result.stdout)
    except:
        pass
    return None


def format_log_url(url):
    if not url:
        return url
    if url.startswith("logdog://"):
        # e.g. logdog://logs.chromium.org/fuchsia/buildbucket/cr-buildbucket/8686235766258814321/+/...
        # -> https://logs.chromium.org/v/?s=fuchsia/buildbucket/cr-buildbucket/8686235766258814321/+/...
        parts = url[9:].split("/", 1)
        if len(parts) == 2:
            host, path = parts
            return f"https://{host}/v/?s={path}"
    return url


def get_live_builds(bb_tool, gerrit_url, ps_num, args):
    """Fetches live status of all builds for a given CL and patch set."""
    if not bb_tool:
        return {}

    # Normalize URL for 'bb ls'
    # 'https://fuchsia-review.git.corp.google.com/c/fuchsia/+/1519781'
    # -> 'fuchsia-review.googlesource.com/c/fuchsia/+/1519781/[PS]'
    norm_url = gerrit_url
    if "https://" in norm_url:
        norm_url = norm_url.replace("https://", "")
    if "git.corp.google.com" in norm_url:
        norm_url = norm_url.replace(".git.corp.google.com", ".googlesource.com")

    # Ensure it doesn't have a trailing slash before adding PS
    norm_url = norm_url.rstrip("/")
    cl_ps_url = f"{norm_url}/{ps_num}"

    try:
        if args.verbose:
            print(
                f"Debug: Fetching live status for {cl_ps_url} using {bb_tool}..."
            )
        # We use bb ls with -cl to see CURRENT builds for this patch set
        cmd = [bb_tool, "ls", "-cl", cl_ps_url, "-json"]
        result = subprocess.run(cmd, capture_output=True, text=True)
        if result.returncode != 0:
            if args.verbose:
                print(f"Debug: 'bb ls' failed with code {result.returncode}")
                print(f"Debug stderr: {result.stderr}")
            return {}

        build_list = result.stdout.strip().splitlines()
        if args.verbose:
            print(f"Debug: Found {len(build_list)} live builds.")

        builds = {}
        for line in build_list:
            if not line:
                continue
            data = json.loads(line)
            builder = data.get("builder", {}).get("builder")
            status = data.get("status")
            update_time = data.get("updateTime")
            if builder:
                if (
                    builder not in builds
                    or update_time > builds[builder]["updateTime"]
                ):
                    builds[builder] = {
                        "status": status,
                        "id": data.get("id"),
                        "updateTime": update_time,
                    }
        return builds
    except:
        return {}


def get_ps_status(messages, cq_authors):
    """Maps patch set number to whether it eventually passed."""
    ps_status = {}  # {ps_num: bool (True if passed)}
    for m in messages:
        ps_num = m.get("_revision_number", 0)
        author_name = m.get("author", {}).get("name", "")
        message = m.get("message", "").lower()
        is_cq_author = any(a in author_name for a in cq_authors)
        is_autogenerated = "autogenerated:buildbucket" in m.get("tag", "")

        if is_cq_author or is_autogenerated:
            if "failed" in message or "failure" in message:
                if ps_num not in ps_status or not ps_status[ps_num]:
                    ps_status[ps_num] = False
            if "passed" in message or "successful" in message:
                ps_status[ps_num] = True
    return ps_status


def get_failing_patch_sets(ps_status, live_builds, messages):
    """Finds patch sets that have at least one failure report AND did not eventually pass."""
    failing_patch_sets = [ps for ps, passed in ps_status.items() if not passed]

    # Identify all patch sets mentioned in messages
    all_patch_sets = [m.get("_revision_number", 0) for m in messages]
    max(all_patch_sets) if all_patch_sets else 1

    # Check if max_ps is ALL SUCCESSFUL in live runs
    if live_builds:
        # If all builds found for this PS are SUCCESS, then it's effectively passed
        if all(b["status"] == "SUCCESS" for b in live_builds.values()):
            failing_patch_sets = []

    # If a LATER patch set passed, we ignore failures from older patch sets.
    if ps_status:
        max_ps_in_messages = max(ps_status.keys())
        if ps_status.get(max_ps_in_messages, False):
            failing_patch_sets = []

    return failing_patch_sets


def print_build_details(build_ids, bb_tool, args, live_builds):
    """Fetches and prints additional details for a list of builds."""
    if not build_ids:
        return

    if not bb_tool:
        return

    bb_cmd = bb_tool if bb_tool else "bb"
    print("\n--- Additional Details ---")
    for bid in build_ids:
        info = get_build_info(bb_tool, bid)
        if info:
            builder = info.get("builder", {}).get("builder", "Unknown")
            build_status = info.get("status", "Unknown")

            # Check live status
            status_suffix = ""
            if builder in live_builds:
                live_status = live_builds[builder]["status"]
                if live_status == "SUCCESS":
                    status_suffix = " (RESOLVED: Latest build passed!)"
                elif live_status == "SCHEDULED" or live_status == "STARTED":
                    status_suffix = f" (RETRYING: current status {live_status})"
                elif live_status == "FAILURE" or live_status == "INFRA_FAILURE":
                    if live_builds[builder]["id"] != bid:
                        status_suffix = " (STILL FAILING in a later retry)"

            print(
                f"\nBuild: {builder} ({bid}) - Status: {build_status}{status_suffix}"
            )

            if args.verbose:
                print(f"Debug: Fetching full details for {bid}...")
                print(f"Debug keys: {list(info.keys())}")
                if "output" in info:
                    print(f"Debug output keys: {list(info['output'].keys())}")

            # Print summary
            summary = info.get("summary_markdown", "")
            if not summary:
                summary = info.get("output", {}).get("summary_markdown", "")
            if summary:
                print(f"  Summary: {summary}")

            # Print logs in output
            logs = info.get("output", {}).get("logs", [])
            if logs:
                print("  Build Logs:")
                for l in logs:
                    url = l.get("view_url") or l.get("url")
                    print(f"    - {l.get('name')}: {format_log_url(url)}")

            # Print logs in failed steps (including infra failures)
            steps = info.get("steps", [])
            failed_steps_shown = False
            for s in steps:
                step_status = s.get("status", "SUCCESS")
                if step_status not in ["SUCCESS", "STARTED", "SCHEDULED"]:
                    if not failed_steps_shown:
                        print("  Failed Steps:")
                        failed_steps_shown = True
                    print(f"    - {s.get('name')} ({step_status}):")
                    step_logs = s.get("logs", [])
                    if step_logs:
                        for l in step_logs:
                            url = l.get("view_url") or l.get("url")
                            log_name = l.get("name")
                            step_name = s.get("name")
                            print(
                                f"      Log: {log_name}: {format_log_url(url)}"
                            )
                            # Provide the bb log command for quick retrieval
                            print(
                                f'      Fetch: {bb_cmd} log {bid} "{step_name}" {log_name}'
                            )
                    else:
                        print("      (No logs for this step)")

            # Print artifacts in properties
            props = info.get("output", {}).get("properties", {})
            if "isolated" in props:
                print(f"  Isolated Digest: {props['isolated']}")
        else:
            print(
                f"\nBuild: {bid} (Could not fetch details. You may need to run '{bb_cmd} auth-login')"
            )


def print_failures(
    messages, args, cl_url, only_latest_ps=True, fetch_logs=False, bb_tool=None
):
    """Prints CQ failures from the messages."""
    failures_found = False

    # Authors that usually post CQ results
    CQ_AUTHORS = ["CQ Bot", "Fuchsia Buildbucket"]

    ps_status = get_ps_status(messages, CQ_AUTHORS)

    # Identify all patch sets mentioned in messages
    all_patch_sets = [m.get("_revision_number", 0) for m in messages]
    max_ps = max(all_patch_sets) if all_patch_sets else 1

    # Fetch live status for the MAX patch set (it might be currently passing or retrying)
    live_builds = get_live_builds(bb_tool, cl_url, max_ps, args)

    failing_patch_sets = get_failing_patch_sets(
        ps_status, live_builds, messages
    )

    if not failing_patch_sets:
        # Check if everything passed or no reports found
        if any(ps_status.values()):
            latest_ps = max(ps_status.keys())
            print(f"\nAll CQ runs for Patch Set {latest_ps} have passed.")
        else:
            print("\nNo CQ reports found in messages.")
        return

    latest_failing_ps = max(failing_patch_sets)

    for m in messages:
        rev_num = m.get("_revision_number", 0)
        if only_latest_ps and rev_num != latest_failing_ps:
            continue

        author_name = m.get("author", {}).get("name", "")
        message = m.get("message", "")

        # Look for failure keywords in messages from CQ authors or autogenerated buildbucket messages
        is_cq_author = any(a in author_name for a in CQ_AUTHORS)
        is_autogenerated = "autogenerated:buildbucket" in m.get("tag", "")

        if (is_cq_author or is_autogenerated) and (
            "failed" in message.lower() or "failure" in message.lower()
        ):
            failures_found = True

            print(f"\n{'='*60}")
            lines = message.splitlines()
            if lines and lines[0].startswith("Patch Set"):
                print(lines[0])
                message_body = "\n".join(lines[1:]).strip()
            else:
                print(f"Patch Set {rev_num}")
                message_body = message.strip()

            print(f"Author: {author_name}")
            print("-" * 60)
            print(message_body)

            # Extract build IDs and try to get logs/artifacts
            build_ids = re.findall(
                r"https://cr-buildbucket\.appspot\.com/build/(\d+)", message
            )
            print_build_details(build_ids, bb_tool, args, live_builds)

    if not failures_found:
        if only_latest_ps:
            print(f"\nNo CQ failures found for Patch Set {latest_failing_ps}.")
        else:
            print("\nNo CQ failures found.")
    else:
        print(f"\n{'='*60}")


def main():
    parser = argparse.ArgumentParser(
        description="Fetch CQ/Tryjob failures for a Gerrit change."
    )
    parser.add_argument(
        "-v",
        "--verbose",
        action="store_true",
        help="Print more debug information",
    )
    parser.add_argument(
        "-a",
        "--all",
        action="store_true",
        help="Show failures for all patch sets instead of just the latest failing one",
    )
    parser.add_argument(
        "-l",
        "--logs",
        action="store_true",
        help="Try to fetch log/artifact details using the 'bb' tool",
    )
    parser.add_argument(
        "change_url",
        nargs="?",
        help="Optional Gerrit change URL. If not provided, uses Change-Id from HEAD.",
    )
    args = parser.parse_args()

    bb_tool = get_bb_tool() if args.logs else None
    if args.logs and not bb_tool:
        print("Warning: 'bb' tool not found. Log fetching will be skipped.")

    if args.change_url:
        change_url = args.change_url
        if args.verbose:
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
    else:
        if args.verbose:
            print("Finding Change-Id from HEAD...")
        change_id_str = get_change_id()
        if not change_id_str:
            print("Error: Could not find Change-Id in HEAD commit message.")
            sys.exit(1)

        if args.verbose:
            print(f"Found Change-Id: {change_id_str}")

        remote_url = get_remote_url()
        if not remote_url:
            print("Error: Could not determine git remote URL.")
            sys.exit(1)

        if args.verbose:
            print(f"Found Remote: {remote_url}")

        base_url, use_gob_curl = parse_remote_url(remote_url)
        if not base_url:
            print(
                f"Error: Could not determine Gerrit host from remote: {remote_url}"
            )
            sys.exit(1)

        if args.verbose:
            print(f"Using Gerrit host: {base_url}")
            if use_gob_curl:
                print("Using gob-curl for authentication.")

        change_info = get_change_details(base_url, change_id_str, use_gob_curl)
        if not change_info:
            print(f"Error: Change {change_id_str} not found on Gerrit.")
            sys.exit(1)

        change_number = change_info.get("_number")
        if args.verbose:
            print(
                f"Found Change {change_number} ({base_url}/c/{change_number})"
            )

    # Fetch full details with messages
    change_detail_url = f"{base_url}/changes/{change_number}/detail?o=MESSAGES&o=DETAILED_ACCOUNTS"
    curl_cmd = ["gob-curl"] if use_gob_curl else ["curl"]

    try:
        if args.verbose:
            print(f"Fetching messages from {change_detail_url}...")
        cmd = curl_cmd + ["-s", change_detail_url]
        result = subprocess.run(cmd, capture_output=True, text=True, check=True)
        content = result.stdout
        if content.startswith(")]}'"):
            content = content[4:]
        change_data = json.loads(content)

        print(f"Checking CQ results for Change {change_number}...")
        messages = change_data.get("messages", [])
        cl_url = f"{base_url}/c/{change_number}"
        print_failures(
            messages,
            args,
            cl_url,
            only_latest_ps=(not args.all),
            fetch_logs=args.logs,
            bb_tool=bb_tool,
        )

    except Exception as e:
        print(f"Error fetching change details: {e}")
        sys.exit(1)


if __name__ == "__main__":
    main()

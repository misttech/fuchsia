#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Runs SEStarnix userspace tests on Linux in qemu."""

import argparse
import fnmatch
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile
from typing import Any

SUCCESS_RE = re.compile("TEST SUCCESS$", re.MULTILINE)


def parse_manifest(
    path: pathlib.Path, output_dir: pathlib.Path
) -> dict[str, pathlib.Path]:
    """Returns a mapping of package file to file path from the package manifest at path."""
    files = {}
    for line in path.read_text().splitlines():
        dest, origin = line.strip().split("=", 1)
        files[dest] = output_dir / origin
    return files


def build_initrd(
    work_dir: pathlib.Path, fuchsia_dir: pathlib.Path
) -> tuple[pathlib.Path, list[str]]:
    """Builds an initrd containing the tests and associated files.

    Args:
      work_dir: the temporary dir we are working in.
      fuchsia_dir: the root of the Fuchsia checkout.

    Returns:
      A pair of the path to the initrd, and the list of tests found.
    """

    output_dir = pathlib.Path(
        subprocess.check_output(
            ["scripts/fx", "get-build-dir"], cwd=fuchsia_dir, text=True
        ).strip()
    )
    container_manifest = (
        output_dir
        / "obj/src/starnix/tests/selinux/userspace/sestarnix_userspace_test_container.manifest"
    )
    tests_manifest = (
        output_dir
        / "obj/src/starnix/tests/selinux/userspace/sestarnix_userspace_tests.manifest"
    )
    files = parse_manifest(container_manifest, output_dir)
    files.update(parse_manifest(tests_manifest, output_dir))

    initrd_dir = work_dir / "initrd"
    initrd_dir.mkdir()

    tests = []
    for package_path, output_path in files.items():
        if package_path.startswith(
            "data/tests/"
        ) and not package_path.startswith("data/tests/expectations/"):
            tests.append(package_path.removeprefix("data/tests/"))
        if package_path == "data/bin/init_for_linux":
            dest_path = initrd_dir / "init"
        elif package_path.startswith("data/lib/"):
            dest_path = initrd_dir / package_path.removeprefix("data/")
        else:
            dest_path = initrd_dir / package_path
        dest_path.parent.mkdir(exist_ok=True, parents=True)
        shutil.copy(output_path, initrd_dir / dest_path)

    subprocess.run(
        "find . | cpio --quiet -R +0:+0 -H newc -o | gzip -9 -n > ../initrd.img",
        shell=True,
        check=True,
        cwd=initrd_dir,
    )
    return (work_dir / "initrd.img"), tests


def parse_audit_expectations_from_output(stdout: str) -> list[dict[str, Any]]:
    """Parses audit expectation JSON blobs from the test runner stdout."""
    audit_expectations = []
    json_buffer = ""
    for line in stdout.splitlines():
        stripped_line = line.strip()
        if not stripped_line:
            continue

        if stripped_line.startswith("{"):
            json_buffer = stripped_line
        elif json_buffer:
            json_buffer += stripped_line

        if json_buffer and stripped_line.endswith("}"):
            try:
                data = json.loads(json_buffer)
                audit_expectations.append(data)
            except json.JSONDecodeError as e:
                print(f"Failed to load JSON object: {e}")
            finally:
                json_buffer = ""
    return audit_expectations


def sanitize_audit_log(log: str) -> str:
    """Removes volatile fields from an audit log string for comparison."""
    # Remove audit(timestamp:serial):
    sanitized = re.sub(r"audit\([^)]*\):\s*", "", log)
    # Remove pid=...
    sanitized = re.sub(r"\s+pid=\S+", "", sanitized)
    # Remove comm="..."
    sanitized = re.sub(r'\s+comm="[^"]*"', "", sanitized)
    return sanitized.strip()


def write_updated_expectations(
    all_new_expectations: list[dict[str, Any]],
    fuchsia_dir: pathlib.Path,
) -> None:
    """Updates the `audit_success.json` file with new results, if they are meaningfully different."""
    if not all_new_expectations:
        print("No new audit expectations found to update.")
        return

    expectations_file = (
        fuchsia_dir
        / "src/starnix/tests/selinux/userspace/expectations/audit_success.json"
    )
    tests_array_name = "audit_success"

    try:
        with open(expectations_file, "r") as f:
            existing_data = json.load(f)
    except (FileNotFoundError, json.JSONDecodeError) as e:
        print(f"Failed to parse expectations file: {e}")
        return

    existing_tests_map = {
        test["name"]: test for test in existing_data.get(tests_array_name, [])
    }

    updated_count = 0
    for new_exp in all_new_expectations:
        test_name = new_exp["name"]
        existing_exp = existing_tests_map.get(test_name)

        # Sanitize the new expectations before comparison and potential saving.
        new_exp["audit_expectations"] = [
            sanitize_audit_log(log)
            for log in new_exp.get("audit_expectations", [])
        ]

        if not existing_exp:
            print(f"Adding new expectations for {test_name}")
            existing_tests_map[test_name] = new_exp
            updated_count += 1
            continue

        # Compare existing and new expectations by sanitizing audit log strings.
        old_audits = existing_exp.get("audit_expectations", [])
        new_audits = new_exp.get("audit_expectations", [])

        # Simple length check first.
        if len(old_audits) != len(new_audits):
            print(
                f"Updating expectations for {test_name} (audit count changed)"
            )
            existing_tests_map[test_name] = new_exp
            updated_count += 1
            continue

        # Compare sanitized audit strings.
        are_different = False
        for old_audit, new_audit in zip(old_audits, new_audits):
            if old_audit != new_audit:
                are_different = True
                break

        if are_different:
            print(
                f"Updating expectations for {test_name} (audit content changed)"
            )
            existing_tests_map[test_name] = new_exp
            updated_count += 1
        else:
            print(f"No meaningful changes for {test_name}, skipping update.")

    if updated_count == 0:
        print("No meaningful expectation changes detected. File not updated.")
        return

    # Convert back to a list and sort by name for consistent output.
    updated_tests = sorted(existing_tests_map.values(), key=lambda x: x["name"])
    final_data = {tests_array_name: updated_tests}
    with open(expectations_file, "w") as f:
        json.dump(final_data, f, indent=4)
        f.write("\n")
    print(
        f"Successfully updated {expectations_file} with {updated_count} changes."
    )


def run_test(
    work_dir: pathlib.Path,
    test_name: str,
    kernel_path: pathlib.Path,
    initrd_path: pathlib.Path,
    args: argparse.Namespace,
) -> tuple[bool, list[dict[str, Any]]]:
    """Runs a test, returns success or failure and any audit expectations."""

    append_args = f"console=ttyS0 security=selinux debug=all audit=1 audit_backlog_limit=0 panic=-1 -- data/tests/{test_name}"
    if args.json or args.update_audit_expectations:
        append_args += " --json"

    print(f"Running {test_name}")
    result = subprocess.run(
        [
            "qemu-system-x86_64",
            "-kernel",
            kernel_path,
            "-initrd",
            initrd_path,
            "-no-reboot",
            "-display",
            "none",
            "-serial",
            "stdio",
            "-m",
            "1G",
            "-enable-kvm",
            "-append",
            append_args,
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=True,  # This indicates test runner failures
    )
    (work_dir / "output").mkdir(exist_ok=True, parents=True)
    (work_dir / "output" / (test_name + ".log")).write_text(result.stdout)

    passed = SUCCESS_RE.search(result.stdout) != None
    if passed:
        print("... OK")
    else:
        print("... FAILED")

    if args.all_output:
        print(result.stdout)
    elif not passed:
        print("Failure output:")
        result_lines: list[str] = []
        record_lines: bool = False
        tests_were_run = False
        for line in result.stdout.splitlines():
            if "[ RUN      ]" in line:
                record_lines = True
                tests_were_run = True
            if not record_lines:
                continue
            result_lines.append(line)
            if "[       OK ]" in line:
                print(line)
                result_lines = []
            elif "[  FAILED  ]" in line:
                print(*result_lines, sep="\n")
                result_lines = []

        if not tests_were_run:
            print(result.stdout)
            print("Failed to run any tests! See all preceding output.")
        print(f"End of output ({test_name})")

    new_expectations = []
    if args.update_audit_expectations:
        new_expectations = parse_audit_expectations_from_output(result.stdout)

    return passed, new_expectations


def main() -> None:
    parser = argparse.ArgumentParser(
        "run_on_linux.py",
        description="Run SEStarnix userspace tests on Linux via QEMU.",
    )
    parser.add_argument(
        "--test-filter",
        type=str,
        default="*",
        help="Test filter (e.g., 'Bpf*').",
    )
    parser.add_argument(
        "--preserve-work-dir",
        help="Keep the work directory on exit.",
        action="store_true",
    )
    parser.add_argument(
        "--all-output",
        help="Emit all output from tests directly, with no pretty-filtering.",
        action="store_true",
    )
    parser.add_argument(
        "--json",
        help="Generate audit JSON objects for expectations.",
        action="store_true",
    )
    parser.add_argument(
        "--update-audit-expectations",
        help="Update the audit expectation JSON file with the results from this run.",
        action="store_true",
    )
    args = parser.parse_args()
    if args.json:
        args.all_output = True

    work_dir = pathlib.Path(tempfile.mkdtemp())
    try:
        build_and_run_tests(work_dir, args)
    finally:
        if args.preserve_work_dir:
            print(f"Workdir preserved at {work_dir}")
        else:
            shutil.rmtree(work_dir)


def build_and_run_tests(
    work_dir: pathlib.Path, args: argparse.Namespace
) -> None:
    if "FUCHSIA_DIR" not in os.environ:
        print("FUCHSIA_DIR is not set", file=sys.stderr)
        sys.exit(1)
    fuchsia_dir = pathlib.Path(os.environ["FUCHSIA_DIR"])
    kernel_path = fuchsia_dir / "local/vmlinuz"
    if not kernel_path.is_file():
        print(f"No kernel found at {kernel_path}", file=sys.stderr)
        print(
            f"Try copying your current kernel with:\n"
            f"cp $(ls /boot/vmlinuz* | head -n1) {kernel_path}",
            file=sys.stderr,
        )
        sys.exit(1)

    print("Re-building tests...")
    subprocess.run(
        [
            "scripts/fx",
            "build",
            "//src/starnix/tests/selinux/userspace:sestarnix_userspace_tests",
        ],
        check=True,
        cwd=fuchsia_dir,
    )

    initrd_path, tests = build_initrd(work_dir, fuchsia_dir)
    matched_tests = fnmatch.filter(tests, args.test_filter)
    print(f"Matched {len(matched_tests)} tests.")

    failed_tests = []
    new_audit_expectations = []
    for test_name in sorted(matched_tests):
        passed, new_expectations = run_test(
            work_dir, test_name, kernel_path, initrd_path, args
        )
        if not passed:
            failed_tests.append(test_name)
        new_audit_expectations.extend(new_expectations)

    if args.update_audit_expectations:
        write_updated_expectations(new_audit_expectations, fuchsia_dir)

    if failed_tests:
        print(f"Failed tests:")
        for test_name in failed_tests:
            print("  " + test_name)
        sys.exit(1)


if __name__ == "__main__":
    main()

#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import hashlib
import os
import shutil
import sys
import tempfile
from pathlib import Path

import merge_policies


def _compute_policy_hash(
    initial_sids_path: str | None,
    input_paths: list[str],
    handle_unknown: str,
) -> tuple[str, str]:
    """Merges source fragments and returns (merged_text, sha256_hash)."""
    if initial_sids_path is not None:
        with tempfile.TemporaryDirectory() as temp_dir:
            merged_path = os.path.join(temp_dir, "policy.conf")
            merge_policies.merge_text_policies(
                initial_sids_path, input_paths, merged_path
            )
            with open(merged_path, "rt", encoding="utf-8") as f:
                merged_content = f.read()
    else:
        assert (
            len(input_paths) == 1
        ), f"Expected exactly 1 input path when initial_sids is not provided, got {len(input_paths)}"
        with open(input_paths[0], "rt", encoding="utf-8") as f:
            merged_content = f.read()

    hasher = hashlib.sha256()
    hasher.update(f"handle_unknown={handle_unknown}\n".encode("utf-8"))
    hasher.update(b"mls=33\n")
    hasher.update(merged_content.encode("utf-8"))
    return merged_content, hasher.hexdigest()


def _find_checkpolicy(explicit_path: str | None) -> str | None:
    """Locates a checkpolicy binary from arguments, environment, in-tree local, or PATH."""
    if (
        explicit_path is not None
        and os.path.isfile(explicit_path)
        and os.access(explicit_path, os.X_OK)
    ):
        return explicit_path
    if (
        (checkpolicy_path := os.environ.get("CHECKPOLICY_PATH")) is not None
        and os.path.isfile(checkpolicy_path)
        and os.access(checkpolicy_path, os.X_OK)
    ):
        return checkpolicy_path
    # Check default //local/checkpolicy in Fuchsia checkout
    fuchsia_dir = os.environ.get(
        "FUCHSIA_DIR",
        os.path.abspath(
            os.path.join(
                os.path.dirname(__file__), "..", "..", "..", "..", ".."
            )
        ),
    )
    if fuchsia_dir is not None:
        local_checkpolicy = os.path.join(fuchsia_dir, "local", "checkpolicy")
        if os.path.isfile(local_checkpolicy) and os.access(
            local_checkpolicy, os.X_OK
        ):
            return local_checkpolicy
    return shutil.which("checkpolicy")


def _write_depfile(
    depfile: str | None,
    target: str,
    hash_file: str,
    prebuilt: str,
    initial_sids: str | None,
    inputs: list[str],
) -> None:
    if depfile is None:
        return
    deps = []
    if os.path.exists(hash_file):
        deps.append(hash_file)
    if os.path.exists(prebuilt):
        deps.append(prebuilt)
    if initial_sids is not None and os.path.exists(initial_sids):
        deps.append(initial_sids)
    for fragment in inputs:
        if os.path.exists(fragment):
            deps.append(fragment)
    os.makedirs(os.path.dirname(depfile), exist_ok=True)
    with open(depfile, "wt", encoding="utf-8") as f:
        f.write(f"{target}: {' '.join(deps)}\n")


def _to_source_rel(path: str, source_root: str) -> str:
    """Converts a path to be relative to the Fuchsia source root if possible."""
    try:
        return os.path.relpath(os.path.abspath(path), source_root)
    except ValueError:
        return path


def _compile_policy(
    checkpolicy_bin: str,
    policy_name: str,
    merged_text: str,
    output_path: str,
    handle_unknown: str,
) -> None:
    """Compiles text policy to binary using checkpolicy."""
    os.makedirs(os.path.dirname(output_path), exist_ok=True)
    with tempfile.TemporaryDirectory() as temp_dir:
        temp_conf = os.path.join(temp_dir, f"{policy_name}.conf")
        with open(temp_conf, "wt", encoding="utf-8") as f:
            f.write(f"# handle_unknown {handle_unknown}\n")
            f.write(merged_text)

        merge_policies.compile_text_policy_to_binary_policy(
            checkpolicy_bin, temp_conf, output_path, handle_unknown
        )


def _verify_prebuilt(
    policy_name: str,
    output_path: str,
    prebuilt_path: str,
    hash_file_path: str,
    stored_hash: str | None,
    current_hash: str,
    source_root: str,
) -> bool:
    """Compares the compiled binary and source hash against checked-in golden prebuilts."""
    if not os.path.exists(prebuilt_path):
        print(f"\n" + "=" * 70, file=sys.stderr)
        print(
            f"ERROR: Prebuilt '{_to_source_rel(prebuilt_path, source_root)}' does not exist.\n\n"
            f"To acknowledge this change, run:\n"
            f"  cp {_to_source_rel(output_path, source_root)} {_to_source_rel(prebuilt_path, source_root)}\n"
            f"  cp {_to_source_rel(output_path + '.hash', source_root)} {_to_source_rel(hash_file_path, source_root)}\n\n"
            f"Or, rebuild with `update_goldens=true` set in your GN args (e.g. via `fx args`).",
            file=sys.stderr,
        )
        print("=" * 70 + "\n", file=sys.stderr)
        return False

    with open(output_path, "rb") as f_out, open(prebuilt_path, "rb") as f_pre:
        candidate_bytes = f_out.read()
        prebuilt_bytes = f_pre.read()
        if candidate_bytes != prebuilt_bytes or stored_hash != current_hash:
            print(f"\n" + "=" * 70, file=sys.stderr)
            print(
                f"ERROR: SELinux policy '{policy_name}' source fragments have been modified.\n"
                f"The compiled binary policy or hash does not match the checked-in prebuilt:\n"
                f"  Prebuilt:  {_to_source_rel(prebuilt_path, source_root)}\n"
                f"  Hash File: {_to_source_rel(hash_file_path, source_root)}\n\n"
                f"To acknowledge this change, run:\n"
                f"  cp {_to_source_rel(output_path, source_root)} {_to_source_rel(prebuilt_path, source_root)}\n"
                f"  cp {_to_source_rel(output_path + '.hash', source_root)} {_to_source_rel(hash_file_path, source_root)}\n\n"
                f"Or, rebuild with `update_goldens=true` set in your GN args (e.g. via `fx args`).",
                file=sys.stderr,
            )
            print("=" * 70 + "\n", file=sys.stderr)
            return False

    return True


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Verify or compile an SELinux policy."
    )
    parser.add_argument(
        "--policy-name", required=True, help="Name of the policy"
    )
    parser.add_argument("--initial-sids", help="Path to initial_sids file")
    parser.add_argument(
        "--inputs", nargs="+", required=True, help="Input policy fragments"
    )
    parser.add_argument(
        "--prebuilt", required=True, help="Path to checked-in prebuilt binary"
    )
    parser.add_argument(
        "--hash-file", required=True, help="Path to checked-in source hash file"
    )
    parser.add_argument(
        "--output",
        required=True,
        help="Path to output binary policy in out-dir",
    )
    parser.add_argument(
        "--handle-unknown",
        default="deny",
        choices=["allow", "deny", "reject"],
        help="handle_unknown setting",
    )
    parser.add_argument("--checkpolicy", help="Path to checkpolicy executable")
    parser.add_argument(
        "--bless",
        action="store_true",
        help="Update checked-in prebuilt and hash in-place",
    )
    parser.add_argument(
        "--stamp", help="Optional stamp file to write on success"
    )
    parser.add_argument("--depfile", help="Path at which to write the depfile")

    args = parser.parse_args()

    target = args.stamp if args.stamp is not None else args.output

    # 1. Compute current source hash
    merged_text, current_hash = _compute_policy_hash(
        args.initial_sids, args.inputs, args.handle_unknown
    )

    # 2. Read stored hash
    stored_hash = None
    if os.path.exists(args.hash_file):
        with open(args.hash_file, "rt", encoding="utf-8") as f:
            stored_hash = f.read().strip()

    # 3. Fast path: sources unchanged & prebuilt exists
    if (
        stored_hash is not None
        and stored_hash == current_hash
        and os.path.exists(args.prebuilt)
    ):
        os.makedirs(os.path.dirname(args.output), exist_ok=True)
        shutil.copyfile(args.prebuilt, args.output)
        if args.stamp is not None:
            Path(args.stamp).touch()
        _write_depfile(
            args.depfile,
            target,
            args.hash_file,
            args.prebuilt,
            args.initial_sids,
            args.inputs,
        )
        return 0

    # 4. Sources changed or prebuilt missing: Look for checkpolicy
    checkpolicy_bin = _find_checkpolicy(args.checkpolicy)
    if checkpolicy_bin is None:
        print(f"\n" + "=" * 70, file=sys.stderr)
        print(
            f"ERROR: SELinux policy '{args.policy_name}' source fragments have changed,",
            file=sys.stderr,
        )
        print(
            f"but 'checkpolicy' is not available in the build environment.",
            file=sys.stderr,
        )
        print(f"Checked-in hash: {stored_hash}", file=sys.stderr)
        print(f"Calculated hash: {current_hash}", file=sys.stderr)
        print(
            f"\nPlease place a valid checkpolicy executable at //local/checkpolicy or",
            file=sys.stderr,
        )
        print(
            f"revert local changes to '{args.policy_name}' fragments.",
            file=sys.stderr,
        )
        print("=" * 70 + "\n", file=sys.stderr)
        return 1

    # 5. Compile candidate binary in out-dir
    _compile_policy(
        checkpolicy_bin,
        args.policy_name,
        merged_text,
        args.output,
        args.handle_unknown,
    )

    # Write candidate hash file in out-dir for manual copy workflows
    with open(args.output + ".hash", "wt", encoding="utf-8") as f:
        f.write(f"{current_hash}\n")

    source_root = os.environ.get(
        "FUCHSIA_DIR",
        os.path.abspath(
            os.path.join(
                os.path.dirname(__file__), "..", "..", "..", "..", ".."
            )
        ),
    )

    # 6. Bless or verify
    if args.bless:
        os.makedirs(os.path.dirname(args.prebuilt), exist_ok=True)
        shutil.copyfile(args.output, args.prebuilt)
        shutil.copyfile(args.output + ".hash", args.hash_file)
        print(f"Blessed '{args.policy_name}': updated prebuilt and hash.")
        if args.stamp is not None:
            Path(args.stamp).touch()
        _write_depfile(
            args.depfile,
            target,
            args.hash_file,
            args.prebuilt,
            args.initial_sids,
            args.inputs,
        )
        return 0

    if not _verify_prebuilt(
        args.policy_name,
        args.output,
        args.prebuilt,
        args.hash_file,
        stored_hash,
        current_hash,
        source_root,
    ):
        return 1

    if args.stamp is not None:
        Path(args.stamp).touch()
    _write_depfile(
        args.depfile,
        target,
        args.hash_file,
        args.prebuilt,
        args.initial_sids,
        args.inputs,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())

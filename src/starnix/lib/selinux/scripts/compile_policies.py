#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# This tool invokes the `merge_policies` module to merge predefined collections
# of partial policy files into a predefined binary policy files.

import os
import sys
import tempfile

import merge_policies

# Derive the path to the top-level Fuchsia checkout directory from this script's path, unless
# explicitly provided via the FUCHSIA_DIR environment variable.
FUCHSIA_DIR = os.environ.get(
    "FUCHSIA_DIR",
    os.path.join(os.path.dirname(__file__), "..", "..", "..", "..", ".."),
)

_TESTDATA_DIRECTORY = f"{FUCHSIA_DIR}/src/starnix/lib/selinux/testdata"
_INPUT_POLICY_DIRECTORY = f"{_TESTDATA_DIRECTORY}/composite_policies"
_OUTPUT_POLICY_DIRECTORY = f"{_TESTDATA_DIRECTORY}/composite_policies/compiled"
_LEGACY_POLICY_DIRECTORY = f"{_TESTDATA_DIRECTORY}/micro_policies"
_INITIAL_SIDS_PATH = f"{_INPUT_POLICY_DIRECTORY}/initial_sids"

_COMPOSITE_POLICY_PATHS = [
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/anon_inode_policy.conf",
        ],
        "anon_inode_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/append.conf",
        ],
        "append_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/binder.conf",
        ],
        "binder_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/bounded_transition_policy.conf",
        ],
        "bounded_transition_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/bpf_policy.conf",
        ],
        "bpf_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/capabilities_policy.conf",
        ],
        "capabilities_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/fcntl_policy.conf",
        ],
        "fcntl_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/genfscon_create.conf",
        ],
        "genfscon_create_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/genfscon_policy.conf",
        ],
        "genfscon_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/inherit_policy.conf",
        ],
        "inherit_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/io_uring_policy.conf",
        ],
        "io_uring_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/ioctl_policy.conf",
        ],
        "ioctl_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/minimal_policy.conf",
        ],
        "minimal_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/mount_policy.conf",
        ],
        "mount_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/mprotect.conf",
        ],
        "mprotect_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/netlink_audit.conf",
        ],
        "netlink_audit_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/perf_event.conf",
        ],
        "perf_event_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/syslog.conf",
        ],
        "syslog_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/class_defaults_policy.conf",
        ],
        "class_defaults_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/exceptions_config_policy.conf",
        ],
        "exceptions_config_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/role_transition_policy.conf",
        ],
        "role_transition_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/role_transition_not_allowed_policy.conf",
        ],
        "role_transition_not_allowed_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/selinuxfs_policy.conf",
        ],
        "selinuxfs_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/memfd_transition.conf",
        ],
        "memfd_transition_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/memfd_class.conf",
        ],
        "memfd_class_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/pipe_policy.conf",
        ],
        "pipe_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/ptrace_policy.conf",
        ],
        "ptrace_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/socket_policy.conf",
        ],
        "socket_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/netlink_xperms_policy.conf",
        ],
        "netlink_xperms_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/socket_policy.conf",
            "new_file/tun_policy.conf",
        ],
        "tun_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/type_transition_policy.conf",
        ],
        "type_transition_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/timerslack.conf",
        ],
        "timerslack_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/range_transition_policy.conf",
        ],
        "range_transition_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/minimal_policy.conf",
            "new_file/allow_fork.conf",
        ],
        "allow_fork_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/minimal_policy.conf",
            "new_file/with_unlabeled_access_domain_policy.conf",
        ],
        "with_unlabeled_access_domain_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/minimal_policy.conf",
            "new_file/with_unlabeled_access_domain_policy.conf",
            "new_file/with_additional_domain_policy.conf",
        ],
        "with_additional_domain_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/file_transition_policy.conf",
        ],
        "file_transition_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/audit_access_policy.conf",
        ],
        "audit_access_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/xattr_access_policy.conf",
        ],
        "xattr_access_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/overlayfs_policy.conf",
        ],
        "overlayfs_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/open_perms.conf",
        ],
        "open_perms_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/fs_test.conf",
        ],
        "fs_test_policy",
    ),
    (
        [
            "base_policy.conf",
            "new_file/test_policy.conf",
            "new_file/userspace_initial_context.conf",
        ],
        "userspace_initial_context_policy",
    ),
]

_HANDLE_UNKNOWN_POLICY_INPUTS = [
    "new_file/handle_unknown_policy.conf",
]
_HANDLE_UNKNOWN_POLICY_OUTPUT = "handle_unknown_policy-%s"

_LEGACY_POLICIES = [
    # keep-sorted start
    "allow_a_attr_b_attr_class0_perm0_policy",
    "allow_a_t_a1_attr_class0_perm0_a2_attr_class0_perm1_policy",
    "allow_a_t_b_attr_class0_perm0_policy",
    "allow_a_t_b_t_class0_perm0_policy",
    "allow_with_constraints_policy",
    "allowxperm_policy",
    "constraints_policy",
    "file_no_defaults_policy",
    "file_range_source_high_policy",
    "file_range_source_low_high_policy",
    "file_range_source_low_policy",
    "file_range_target_high_policy",
    "file_range_target_low_high_policy",
    "file_range_target_low_policy",
    "file_source_defaults_policy",
    "file_target_defaults_policy",
    "hooks_tests_policy",
    "minimal_policy",
    "multiple_levels_and_categories_policy",
    "no_allow_a_attr_b_attr_class0_perm0_policy",
    "no_allow_a_t_b_attr_class0_perm0_policy",
    "no_allow_a_t_b_t_class0_perm0_policy",
    "security_context_tests_policy",
    "security_server_tests_policy",
    # keep-sorted end
]


def _compile_composite_policy(
    checkpolicy_executable: str,
    inputs: list[str],
    output: str,
    handle_unknown: str,
) -> None:
    """(Re)Compile "composite" test policy sources into a binary policy file."""
    input_paths = list(
        f"{_INPUT_POLICY_DIRECTORY}/{input_path}" for input_path in inputs
    )
    output_path = f"{_OUTPUT_POLICY_DIRECTORY}/{output}"
    with tempfile.TemporaryDirectory() as temporary_directory_name:
        merged_path = f"{temporary_directory_name}/policy.conf"
        merge_policies.merge_text_policies(
            _INITIAL_SIDS_PATH, input_paths, merged_path
        )
        merge_policies.compile_text_policy_to_binary_policy(
            checkpolicy_executable,
            merged_path,
            output_path,
            handle_unknown,
        )


def compile_policies(checkpolicy_executable: str) -> None:
    """(Re)Compile all test policies with the specified `checkpolicy_executable`.
    Both "composite" policies, built from a set of fragments, and legacy
    all-in-one policies are rebuilt.
    """
    for inputs, output in _COMPOSITE_POLICY_PATHS:
        _compile_composite_policy(
            checkpolicy_executable, inputs, output, "deny"
        )
    for name in _LEGACY_POLICIES:
        merge_policies.compile_text_policy_to_binary_policy(
            checkpolicy_executable,
            f"{_LEGACY_POLICY_DIRECTORY}/{name}.conf",
            f"{_LEGACY_POLICY_DIRECTORY}/{name}",
            "deny",
        )
    for handle_unknown in ("allow", "deny", "reject"):
        _compile_composite_policy(
            checkpolicy_executable,
            _HANDLE_UNKNOWN_POLICY_INPUTS,
            _HANDLE_UNKNOWN_POLICY_OUTPUT % handle_unknown,
            handle_unknown,
        )


if __name__ == "__main__":
    checkpolicy_executable = f"{FUCHSIA_DIR}/local/checkpolicy"

    if not os.path.exists(checkpolicy_executable):
        print(
            "This script should be run from the top-level directory of the Fuchsia checkout, and requires that there is an appropriate binary at local/checkpolicy.",
            file=sys.stderr,
        )
        sys.exit(1)

    compile_policies(checkpolicy_executable)

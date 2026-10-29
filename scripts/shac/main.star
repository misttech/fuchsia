# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Main entry point for Fuchsia SHAC checks."""

# keep-sorted start
load("./bazel.star", "register_bazel_checks")
load("./bazel_migration.star", "register_bazel_migration_checks")
load("./bazel_migration_fidl.star", "register_bazel_migration_fidl_checks")
load("./bazel_test_parity.star", "register_bazel_test_parity_checks")
load("./bug_urls.star", "bug_urls")
load("./check_licenses.star", "register_check_licenses_checks")
load("./cml.star", "register_cml_checks")
load("./commit_msg.star", "register_commit_msg_checks")
load("./confusing_characters.star", "confusing_characters")
load("./cpp.star", "register_cpp_checks")
load("./dart.star", "register_dart_checks")
load("./dml.star", "register_dml_checks")
load("./docs.star", "register_doc_checks")
load("./fidl.star", "register_fidl_checks")
load("./gn.star", "register_gn_checks")
load("./go.star", "register_go_checks")
load("./json.star", "register_json_checks")
load("./keep_sorted.star", "keep_sorted")
load("./mirror_blocklists.star", "register_mirror_blocklists_checks")
load("./owners.star", "register_owners_checks")
load("./prebuilts.star", "register_prebuilt_checks")
load("./python.star", "register_python_checks")
load("./readme_fuchsia.star", "register_readme_fuchsia_checks")
load("./rust.star", "register_rust_checks")
load("./skills.star", "register_skills_checks")
load("./starlark.star", "register_starlark_checks")
load("./third_party_readme.star", "register_third_party_readme_checks")
load("./underscore_vs_dash.star", "register_underscore_vs_dash_checks")
# keep-sorted end

def register_all_checks():
    """Register all checks that should run.

    Checks must be registered in a callback function because they can only be
    registered by the root shac.star file, not at the top level of any `load`ed
    file.
    """
    shac.register_check(keep_sorted)
    shac.register_check(bug_urls)
    shac.register_check(confusing_characters)

    # keeps-sorted start
    register_bazel_checks()
    register_bazel_migration_checks()
    register_bazel_migration_fidl_checks()
    register_bazel_test_parity_checks()
    register_check_licenses_checks()
    register_cml_checks()
    register_commit_msg_checks()
    register_cpp_checks()
    register_dart_checks()
    register_dml_checks()
    register_doc_checks()
    register_fidl_checks()
    register_gn_checks()
    register_go_checks()
    register_json_checks()
    register_mirror_blocklists_checks()
    register_owners_checks()
    register_prebuilt_checks()
    register_python_checks()
    register_readme_fuchsia_checks()
    register_rust_checks()
    register_skills_checks()
    register_starlark_checks()
    register_third_party_readme_checks()
    register_underscore_vs_dash_checks()
    # keeps-sorted end

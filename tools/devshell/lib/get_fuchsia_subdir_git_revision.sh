#!/bin/bash
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# This script retrieves the git revision (commit hash) for a specific
# subdirectory within a Fuchsia checkout. It supports both standard git
# repositories and Cog workspaces.
#
# Arguments:
#   $1: The path to the root of the Fuchsia checkout (e.g., ~/fuchsia).
#   $2: The relative path of the git repository subdirectory within the Fuchsia checkout (e.g., "vendor/google").
set -o errexit

fuchsia_dir="$1"
repo_root="$2"

if [[ -d "${fuchsia_dir}/.git" ]]; then
  git --no-optional-locks -C "${fuchsia_dir}/${repo_root}" rev-parse HEAD
else
  # For the cog workspace case
  workspace_id=$(cat "${fuchsia_dir}/../.citc/workspace_id")
  base_snapshot_version=$(cat "${fuchsia_dir}/../.citc/snapshot_version")

  # This payload follow the format of request proto. example grpc_cli invocation
  # can be found in: https://chromium.googlesource.com/external/github.com/grpc/grpc/+/refs/heads/chromium-deps/2016-08-17/doc/command_line_tool.md#basic-usage
  request="request_base { workspace_id: \"${workspace_id}\" base_snapshot_version: ${base_snapshot_version}} repo_root: \"fuchsia/${repo_root}\""
  git citc api.call GetDrafts "${request}" | grep 'commit_hash:' | awk -F '"' '{print $2}'
fi

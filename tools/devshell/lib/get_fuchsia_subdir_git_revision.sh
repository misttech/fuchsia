#!/bin/bash
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# This script retrieves the git revision (commit hash) for a specific
# subdirectory within a Fuchsia checkout. It supports standard git
# repositories.
#
# Arguments:
#   $1: The path to the root of the Fuchsia checkout (e.g., ~/fuchsia).
#   $2: The relative path of the git repository subdirectory within the Fuchsia checkout (e.g., "vendor/google").
set -o errexit

fuchsia_dir="$1"
repo_root="$2"

git --no-optional-locks -C "${fuchsia_dir}/${repo_root}" rev-parse HEAD

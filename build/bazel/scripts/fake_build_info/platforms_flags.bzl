# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Fake platform flags configuration for minimal testing."""

# FAKE FILE FOR MINIMAL TEST WORKSPACE - DO NOT EDIT

fuchsia_has_lto = False
host_has_lto = False

platform_flags_map = {
    "linux_x64": ["--cpu=x86_64"],
    "linux_arm64": ["--cpu=aarch64"],
    "fuchsia_sdk_x64": ["--cpu=x86_64"],
    "fuchsia_sdk_arm64": ["--cpu=aarch64"],
    "fuchsia_sdk_riscv64": ["--cpu=riscv64"],
    "fuchsia_platform_x64": ["--cpu=x86_64"],
    "fuchsia_platform_arm64": ["--cpu=aarch64"],
    "fuchsia_platform_riscv64": ["--cpu=riscv64"],
}

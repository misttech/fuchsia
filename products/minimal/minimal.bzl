# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""
Arguments for defining minimal products.

These are extracted into a loadable .bzl file for sharing between repos.
"""

load(
    "@rules_fuchsia//fuchsia:assembly.bzl",
    "BUILD_TYPES",
    "INPUT_DEVICE_TYPE",
)
load("//build/info:info.bzl", "DEFAULT_PRODUCT_BUILD_INFO")

MINIMAL_PLATFORM_BASE = {
    "build_type": BUILD_TYPES.ENG,
    "fonts": {
        "enabled": False,
    },
    "ui": {
        "supported_input_devices": [
            INPUT_DEVICE_TYPE.BUTTON,
            INPUT_DEVICE_TYPE.TOUCHSCREEN,
        ],
    },
    "power": {
        "enable_non_hermetic_testing": True,
    },
    "kernel": {
        "scheduler_enable_new_wakeup_accounting": True,
    },
}

MINIMAL_PRODUCT_BASE = {
    "build_info": DEFAULT_PRODUCT_BUILD_INFO | {
        "name": "minimal",
    },
}

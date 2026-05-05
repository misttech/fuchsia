# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""
Providers for Fuchsia product assembly.
"""

FuchsiaProductConfigInfo = provider(
    doc = "Info about the ProductConfiguration and it's directory containing the product_config.json and all deps.",
    fields = {
        "directory": "Directory of the product config container",
        "build_type": "The build type of the product.",
        "build_id_dirs": "Directories containing the debug symbols",
    },
)

# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

load("@fuchsia_build_config//:defs.bzl", "build_config")

# Execution platform properties for targets that run on the default RBE workers.
# Both the property key (e.g. "Pool") and value are derived directly from the
# active RBE profile's "default" worker_pools definition.
RBE_USE_DEFAULT_WORKER = build_config.rbe_default_exec_properties

# Execution platform properties for targets that require larger RBE workers
# with more CPU cores and memory. Both the property key (e.g. "Pool") and value
# are derived directly from the active RBE profile's "large" worker_pools definition.
RBE_USE_LARGE_WORKER = build_config.rbe_large_exec_properties

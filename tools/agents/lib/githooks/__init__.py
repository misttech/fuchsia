# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Fuchsia Git hook infrastructure and integration."""

from agents.lib.githooks.reporters import ConsoleReporter

__all__ = ["ConsoleReporter"]

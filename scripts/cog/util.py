# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Utility functions for cog scripts."""

import subprocess


def check_gcert_status() -> bool:
    """Checks if the user has a valid gcert certificate."""
    try:
        subprocess.check_call(["gcertstatus", "-check_ssh=false", "-quiet"])
        return True
    except (subprocess.CalledProcessError, FileNotFoundError):
        return False

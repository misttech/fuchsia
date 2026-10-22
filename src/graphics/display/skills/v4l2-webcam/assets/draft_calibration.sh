#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -eu

# TODO: Replace with the webcam's video source.
devfs_path=/dev/video0

# Usage: change_setting <SETTING_NAME> <SETTING_VALUE>
change_setting() {
  v4l2-ctl --device "$devfs_path" --set-ctrl "$1=$2"
}

# Phase 1: Disable automated settings, so the manual settings remain in effect.

# TODO: Replace the examples below with settings obtained from v4l2-ctl.

# Type: bool.
# Status: disabling automated adjustments.
change_setting "focus_automatic_continuous" 0

# Type: menu. Range: min=0 max=3. 1: Manual Mode
# Status: disabling automated adjustments.
change_setting "auto_exposure" 1

# Phase 2: Configure manual settings.

# TODO: Replace the examples below with settings obtained from v4l2-ctl.

# Type: int. Range: min=1 max=2500 step=1.
# Status: not calibrated.
change_setting "exposure_time_absolute" 330

# Type: int. Range: min=0 max=100 step=1.
# Status: not calibrated.
change_setting "focus_absolute" 30

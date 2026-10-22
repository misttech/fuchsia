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

# Usage: sample_photo <SETTING_NAME> <SETTING_VALUE>
# Photo output path: calibration_samples/<SETTING_NAME>/<SETTING_VALUE>.png
sample_photo() {
  # Avoid cluttering the main directory.
  mkdir -p "calibration_samples/$1"

  # Wait for the camera hardware to physically adjust to any new setting value.
  ffmpeg -hide_banner -y -loglevel error -f v4l2 -i "$devfs_path" -ss 2 -frames:v 1 -update 1 "calibration_samples/$1/$2.png"
}

# Uniform sampling.
range_start=0
range_end=50
sampling_step=5
for value in $(seq $range_start $sampling_step $range_end); do
  change_setting "focus_absolute" "$value"
  sample_photo "focus_absolute" "$value"
done

// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_LIB_FOCALTECH_INCLUDE_LIB_FOCALTECH_FOCALTECH_H_
#define SRC_DEVICES_LIB_FOCALTECH_INCLUDE_LIB_FOCALTECH_FOCALTECH_H_

#include <stdbool.h>
#include <stdint.h>

#define FOCALTECH_DEVICE_FT3X27 0
#define FOCALTECH_DEVICE_FT6336 1
#define FOCALTECH_DEVICE_FT5726 2
#define FOCALTECH_DEVICE_FT5336 3

struct FocaltechMetadata {
  // The specific FocalTech IC, must be a FOCALTECH_DEVICE_ value.
  uint32_t device_id;

  // True if and only if firmware update is needed during driver initialization.
  //
  // If true, the board driver must provide panel type information to the
  // touch controller driver using PANEL_TYPE metadata.
  bool needs_firmware;
};

#endif  // SRC_DEVICES_LIB_FOCALTECH_INCLUDE_LIB_FOCALTECH_FOCALTECH_H_

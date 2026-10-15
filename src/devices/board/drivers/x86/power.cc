// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "power.h"

#include <acpica/acpi.h>

void poweroff(zx_device_t* device) {
  ACPI_STATUS status = AcpiEnterSleepStatePrep(5);
  if (status == AE_OK) {
    // AcpiEnterSleepState is usually called with interrupts disabled so that the interrupt state
    // can be saved and restored on wake. However for power off (sleep state 5) AcpiEnterSleepState
    // can be called without interrupts disabled as caches are dropped regardless.
    AcpiEnterSleepState(5);
  }
}

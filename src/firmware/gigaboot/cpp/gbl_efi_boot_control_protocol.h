// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef __GBL_EFI_BOOT_CONTROL_PROTOCOL_H__
#define __GBL_EFI_BOOT_CONTROL_PROTOCOL_H__

#include <stdint.h>

#include "gbl_protocol_utils.h"

EFI_ENUM(GblEfiUnbootableReason, uint8_t, GBL_EFI_UNBOOTABLE_REASON_UNKNOWN_REASON,
         GBL_EFI_UNBOOTABLE_REASON_NO_MORE_TRIES, GBL_EFI_UNBOOTABLE_REASON_SYSTEM_UPDATE,
         GBL_EFI_UNBOOTABLE_REASON_USER_REQUESTED, GBL_EFI_UNBOOTABLE_REASON_VERIFICATION_FAILURE);

EFI_ENUM(GblEfiOneShotBootMode, uint32_t, GBL_EFI_ONE_SHOT_BOOT_MODE_NONE,
         GBL_EFI_ONE_SHOT_BOOT_MODE_BOOTLOADER, GBL_EFI_ONE_SHOT_BOOT_MODE_RECOVERY);

typedef struct {
  // One UTF-8 encoded single character
  uint32_t suffix;
  // Any value other than those explicitly enumerated in EFI_UNBOOTABLE_REASON
  // will be interpreted as UNKNOWN_REASON.
  GblEfiUnbootableReason unbootable_reason;
  uint8_t priority;
  uint8_t tries;
  // Value of 1 if slot has successfully booted.
  uint8_t successful;
} GblEfiSlotInfo;

typedef struct {
  size_t kernel_size;
  EfiPhysicalAddr kernel;
  size_t ramdisk_size;
  EfiPhysicalAddr ramdisk;
  size_t device_tree_size;
  EfiPhysicalAddr device_tree;
  uint64_t reserved[8];
} GblEfiLoadedOs;

typedef void (*OsEntryPoint)(size_t descriptor_size, uint32_t descriptor_version,
                             size_t num_descriptors, const EfiMemoryDescriptor* memory_map,
                             const GblEfiLoadedOs* os);

static const uint64_t GBL_EFI_BOOT_CONTROL_PROTOCOL_REVISION = GBL_PROTOCOL_REVISION(0, 2);

typedef struct GblEfiBootControlProtocol {
  uint64_t revision;
  // Slot metadata query methods
  EfiStatus (*get_slot_count)(struct GblEfiBootControlProtocol* self,
                              /* out */ uint8_t* slot_count);
  EfiStatus (*get_slot_info)(struct GblEfiBootControlProtocol* self,
                             /* in */ uint8_t index,
                             /* out */ GblEfiSlotInfo* info);
  EfiStatus (*get_current_slot)(struct GblEfiBootControlProtocol* self,
                                /* out */ GblEfiSlotInfo* info);
  // Slot metadata manipulation methods
  EfiStatus (*set_active_slot)(struct GblEfiBootControlProtocol* self,
                               /* in */ uint8_t index);
  // Boot control methods
  EfiStatus (*get_one_shot_boot_mode)(struct GblEfiBootControlProtocol* self,
                                      /* out */ GblEfiOneShotBootMode* mode);
  EfiStatus (*handle_loaded_os)(struct GblEfiBootControlProtocol* self,
                                /* in */ const GblEfiLoadedOs* os,
                                /* out */ OsEntryPoint* entry_point);
} GblEfiBootControlProtocol;

extern bool g_should_stop_in_fastboot;

namespace gigaboot {
efi_status InstallGblEfiBootControlProtocol();
}

#endif  // __GBL_EFI_BOOT_CONTROL_PROTOCOL_H__

// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <gbl/gbl_efi_protocols.h>
#include <phys/efi/main.h>
#include <uefi/protocols/gbl_efi_boot_control_protocol.h>

#define GBL_EFI_BOOT_CONTROL_PROTOCOL_GUID \
  {0xd382db1b, 0x9ac2, 0x11f0, {0x84, 0xc7, 0x04, 0x7b, 0xcb, 0xa9, 0x60, 0x19}}

// Set within `LaunchGbl()`;
bool g_should_stop_in_fastboot = false;

namespace {

// Slot metadata query methods
EfiStatus GetSlotCount(struct GblEfiBootControlProtocol* self,
                       /* out */ uint8_t* slot_count) {
  *slot_count = 2;
  return EFI_SUCCESS;
}
// Boot control methods
EfiStatus GetOneShotBootMode(struct GblEfiBootControlProtocol* self, GblEfiOneShotBootMode* mode) {
  if (!mode) {
    return EFI_INVALID_PARAMETER;
  }
  *mode = g_should_stop_in_fastboot ? GblEfiOneShotBootMode::GBL_EFI_ONE_SHOT_BOOT_MODE_BOOTLOADER
                                    : GblEfiOneShotBootMode::GBL_EFI_ONE_SHOT_BOOT_MODE_NONE;
  return EFI_SUCCESS;
}

GblEfiBootControlProtocol protocol = {
    .revision = GBL_EFI_BOOT_CONTROL_PROTOCOL_REVISION,
    .get_slot_count = GetSlotCount,
    .get_slot_info = nullptr,
    .get_current_slot = nullptr,
    .set_active_slot = nullptr,
    .get_one_shot_boot_mode = GetOneShotBootMode,
    .handle_loaded_os = nullptr,
};

efi_guid guid = GBL_EFI_BOOT_CONTROL_PROTOCOL_GUID;

}  // namespace

namespace gigaboot {
efi_status InstallGblEfiBootControlProtocol() {
  efi_handle out_handle = nullptr;
  return gEfiSystemTable->BootServices->InstallMultipleProtocolInterfaces(&out_handle, &guid,
                                                                          &protocol, NULL);
}
}  // namespace gigaboot

// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef __GBL_EFI_PROTOCOLS_H__
#define __GBL_EFI_PROTOCOLS_H__

#include <efi/types.h>

#include "utils.h"

namespace gigaboot {

extern RebootMode g_reboot_mode;

efi_status InstallGblEfiAvbProtocol();
efi_status InstallGblEfiBootControlProtocol();
efi_status InstallGblEfiFastbootProtocol();

}  // namespace gigaboot

#endif  // __GBL_EFI_PROTOCOLS_H__

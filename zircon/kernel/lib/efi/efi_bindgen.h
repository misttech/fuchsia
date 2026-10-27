// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_EFI_EFI_BINDGEN_H_
#define ZIRCON_KERNEL_LIB_EFI_EFI_BINDGEN_H_

#include <zircon/compiler.h>

#include <efi/runtime-services.h>
#include <efi/types.h>
#include <efi/zircon.h>

__BEGIN_CDECLS

static const efi_status kEfiSuccess = EFI_SUCCESS;
static const efi_status kEfiInvalidParameter =
    EFI_INVALID_PARAMETER;                             // NOLINT(bugprone-signed-bitwise)
static const efi_status kEfiNotFound = EFI_NOT_FOUND;  // NOLINT(bugprone-signed-bitwise)

extern const efi_guid kEfiZirconVendorGuid;
extern const char16_t kEfiZirconCrashlogEfivar[9];

__END_CDECLS

#endif  // ZIRCON_KERNEL_LIB_EFI_EFI_BINDGEN_H_

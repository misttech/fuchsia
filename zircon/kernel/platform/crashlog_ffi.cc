// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <stddef.h>

#include <efi/runtime-services.h>
#include <efi/types.h>
#include <efi/zircon.h>
#include <ktl/type_traits.h>
#include <platform/efi.h>
#include <ram-crashlog/ram-crashlog.h>

static_assert(sizeof(ram_crashlog_v1_t) == 72);
static_assert(sizeof(recovered_ram_crashlog_t) == 40 && alignof(recovered_ram_crashlog_t) == 8);
static_assert(offsetof(recovered_ram_crashlog_t, uptime) == 0);
static_assert(offsetof(recovered_ram_crashlog_t, runtime) == 8);
static_assert(offsetof(recovered_ram_crashlog_t, reason) == 16);
static_assert(offsetof(recovered_ram_crashlog_t, payload_valid) == 20);
static_assert(offsetof(recovered_ram_crashlog_t, payload) == 24);
static_assert(offsetof(recovered_ram_crashlog_t, payload_len) == 32);

static_assert(sizeof(efi_guid) == 16 && alignof(efi_guid) == 4);
static_assert(ktl::is_standard_layout_v<EfiServicesActivation>);
static_assert(sizeof(EfiServicesActivation) == 16 && alignof(EfiServicesActivation) == 8);
static_assert(ZIRCON_CRASHLOG_EFIATTR == 0x7);  // NOLINT(bugprone-signed-bitwise)
static_assert(EFI_SUCCESS == 0);
static_assert(EFI_NOT_FOUND == (EFI_ERROR_MASK | 14u));  // NOLINT(bugprone-signed-bitwise)

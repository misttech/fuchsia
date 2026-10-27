// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <stddef.h>

#include <efi/runtime-services.h>
#include <efi/types.h>
#include <kernel/ffi.h>
#include <ktl/type_traits.h>
#include <platform/efi.h>

static_assert(sizeof(efi_guid) == 16);
static_assert(alignof(efi_guid) == 4);
static_assert(sizeof(EfiServicesActivation) == 16);
static_assert(alignof(EfiServicesActivation) == 8);
static_assert(sizeof(ffi::Uninitialized<EfiServicesActivation>) == sizeof(EfiServicesActivation));
static_assert(alignof(ffi::Uninitialized<EfiServicesActivation>) == alignof(EfiServicesActivation));
static_assert(ktl::is_standard_layout_v<EfiServicesActivation>);
static_assert(ktl::is_standard_layout_v<ffi::Uninitialized<EfiServicesActivation>>);

extern "C" {

// Must not be inlined into non-NO_ASAN callers because the EFI address space does not map KASAN
// shadow memory for the 1:1 physical EFI runtime services region.
[[gnu::noinline]] void NO_ASAN
cpp_try_activate_efi_services(ffi::Uninitialized<EfiServicesActivation>* out) {
  out->Initialize(TryActivateEfiServices());
}

[[gnu::noinline]] bool NO_ASAN
cpp_efi_services_activation_valid(const EfiServicesActivation* services) {
  return services->valid();
}

[[gnu::noinline]] void NO_ASAN cpp_efi_services_activation_reset(EfiServicesActivation* services) {
  services->reset();
}

[[gnu::noinline]] efi_status NO_ASAN cpp_efi_services_set_variable(
    const EfiServicesActivation* services, const char16_t* var_name, const efi_guid* vendor_guid,
    uint32_t attributes, size_t data_size, const void* data) {
  return (*services)->SetVariable(const_cast<char16_t*>(var_name),
                                  const_cast<efi_guid*>(vendor_guid), attributes, data_size, data);
}

}  // extern "C"

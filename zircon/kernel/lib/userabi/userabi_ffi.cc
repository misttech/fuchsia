// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/boot-options/boot-options.h>
#include <lib/fasttime/internal/abi.h>
#include <lib/userabi/userboot.h>
#include <lib/userabi/vdso-constants.h>
#include <stddef.h>

#if ENABLE_ENTROPY_COLLECTOR_TEST
#include <lib/crypto/entropy/quality_test.h>
#endif

#ifdef __aarch64__
#include <arch/arm64/feature.h>
#endif

static_assert(sizeof(PhysMapping::Permissions) == 8 && alignof(PhysMapping::Permissions) == 8);
static_assert(sizeof(PhysMapping) == 80 && alignof(PhysMapping) == 8);
static_assert(sizeof(std::optional<size_t>) == 16 && alignof(std::optional<size_t>) == 8);
static_assert(sizeof(PhysElfImage::Info) == 24 && alignof(PhysElfImage::Info) == 8);
static_assert(PhysElfImage::kZeroFill == ~uintptr_t{0});
static_assert(sizeof(fbl::Vector<PhysMapping>) == 24 && alignof(fbl::Vector<PhysMapping>) == 8);
static_assert(sizeof(HandoffEnd::Elf) == 72 && alignof(HandoffEnd::Elf) == 8);
static_assert(sizeof(HandoffEnd) == 176 && alignof(HandoffEnd) == 8);
static_assert(sizeof(FILE) == 16 && alignof(FILE) == 8);
static_assert(PhysVmo::kMaxExtraHandoffPhysVmos == 3);
static_assert(sizeof(vdso_constants) == 56 && alignof(vdso_constants) == 8);
static_assert(offsetof(vdso_constants, max_num_cpus) == 0);
static_assert(offsetof(vdso_constants, features) == 4);
static_assert(offsetof(vdso_constants, features.cpu) == 4);
static_assert(offsetof(vdso_constants, features.hw_breakpoint_count) == 8);
static_assert(offsetof(vdso_constants, features.hw_watchpoint_count) == 12);
static_assert(offsetof(vdso_constants, features.address_tagging) == 16);
static_assert(offsetof(vdso_constants, features.vm) == 20);
static_assert(offsetof(vdso_constants, features.cpu_sve) == 24);
static_assert(offsetof(vdso_constants, dcache_line_size) == 28);
static_assert(offsetof(vdso_constants, icache_line_size) == 32);
static_assert(offsetof(vdso_constants, page_size) == 36);
static_assert(offsetof(vdso_constants, physmem) == 40);
static_assert(offsetof(vdso_constants, version_string_len) == 48);
static_assert(offsetof(vdso_constants, version_string) == 56);
static_assert(sizeof(fasttime::internal::TimeValues) == 48 &&
              alignof(fasttime::internal::TimeValues) == 8);
static_assert(offsetof(fasttime::internal::TimeValues, version) == 0);
static_assert(offsetof(fasttime::internal::TimeValues, ticks_per_second) == 8);
static_assert(offsetof(fasttime::internal::TimeValues, boot_ticks_offset) == 16);
static_assert(offsetof(fasttime::internal::TimeValues, mono_ticks_offset) == 24);
static_assert(offsetof(fasttime::internal::TimeValues, ticks_to_time_numerator) == 32);
static_assert(offsetof(fasttime::internal::TimeValues, ticks_to_time_denominator) == 36);
static_assert(offsetof(fasttime::internal::TimeValues, usermode_can_access_ticks) == 40);
static_assert(offsetof(fasttime::internal::TimeValues, use_a73_errata_mitigation) == 41);
static_assert(offsetof(fasttime::internal::TimeValues, use_pct_instead_of_vct) == 42);
static_assert(offsetof(fasttime::internal::TimeValues, _padding) == 43);
// vdso.rs mirrors this as TIME_VALUES_VMO_NAME.
static_assert(sizeof(kTimeValuesVmoName) == sizeof("time_values") &&
              __builtin_memcmp(kTimeValuesVmoName, "time_values", sizeof("time_values")) == 0);

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.

FFI_ALWAYS_INLINE void cpp_boot_options_show(bool defaults, FILE* out) {
  BootOptions::Get()->Show(defaults, out);
}

#if ENABLE_ENTROPY_COLLECTOR_TEST
FFI_ALWAYS_INLINE bool cpp_entropy_was_lost() { return crypto::entropy::entropy_was_lost; }

FFI_ALWAYS_INLINE VmObject* cpp_entropy_vmo() {
  fbl::RefPtr<VmObject> vmo = crypto::entropy::entropy_vmo;
  return fbl::ExportToRawPtr(&vmo);
}

FFI_ALWAYS_INLINE size_t cpp_entropy_vmo_stream_size() {
  return crypto::entropy::entropy_vmo_stream_size;
}
#endif

#if defined(__aarch64__) && ZX_DEBUG_ASSERT_IMPLEMENTED
FFI_ALWAYS_INLINE void cpp_arm64_print_midr_cpu_name(FILE* out) { arm64_print_midr_cpu_name(out); }
#endif

}  // extern "C"

void userboot_init(HandoffEnd handoff_end) { rust_userboot_init(&handoff_end); }

// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/boot-options/boot-options.h>
#include <lib/crashlog.h>
#include <lib/userabi/userboot.h>
#include <lib/userabi/vdso.h>

#include <platform/crashlog.h>

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
static_assert(VDso::kNumVdsoVariants == 4);

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.

FFI_ALWAYS_INLINE size_t cpp_platform_crashlog_recover(FILE* tgt) {
  return PlatformCrashlog::Get().Recover(tgt);
}

FFI_ALWAYS_INLINE void cpp_crashlog_stash(VmObject* vmo) {
  crashlog_stash(fbl::RefPtr<VmObject>(vmo));
}

FFI_ALWAYS_INLINE void cpp_platform_crashlog_enable_uptime_updates(bool enabled) {
  PlatformCrashlog::Get().EnableCrashlogUptimeUpdates(enabled);
}

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

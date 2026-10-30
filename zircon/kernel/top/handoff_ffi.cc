// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <stdint.h>

#include <kernel/ffi.h>
#include <phys/handoff/boot-constants.h>
#include <phys/handoff/handoff.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const uint8_t* cpp_boot_constants_kernel_version_ident(size_t* size) {
  std::string_view version = kBootConstants.kernel_version_ident.get();
  *size = version.size();
  return reinterpret_cast<const uint8_t*>(version.data());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const uint8_t* cpp_boot_constants_kernel_debug_ident(size_t* size) {
  std::string_view ident = kBootConstants.kernel_debug_ident.get();
  *size = ident.size();
  return reinterpret_cast<const uint8_t*>(ident.data());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint32_t cpp_boot_constants_hw_reboot_reason() {
  return static_cast<uint32_t>(kBootConstants.hw_reboot_reason);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const uint8_t* cpp_boot_constants_system_version_string(size_t* size) {
  std::string_view version = kBootConstants.system_version_string.get();
  *size = version.size();
  return reinterpret_cast<const uint8_t*>(version.data());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const PhysHandoff* cpp_phys_handoff_get() { return gPhysHandoff; }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const uint8_t* cpp_phys_handoff_crashlog(size_t* size) {
  std::string_view crashlog = gPhysHandoff->crashlog.get();
  *size = crashlog.size();
  return reinterpret_cast<const uint8_t*>(crashlog.data());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint8_t* cpp_phys_handoff_nvram(size_t* size) {
  std::span<std::byte> nvram = gPhysHandoff->nvram.get();
  *size = nvram.size();
  return reinterpret_cast<uint8_t*>(nvram.data());
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const zbi_topology_node_t* cpp_phys_handoff_cpu_topology(size_t* count) {
  std::span<const zbi_topology_node_t> topology = gPhysHandoff->cpu_topology.get();
  *count = topology.size();
  return topology.data();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const memalloc::Range* cpp_phys_handoff_memory(size_t* count) {
  std::span<const memalloc::Range> memory = gPhysHandoff->memory.get();
  *count = memory.size();
  return memory.data();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const MappedMmioRange* cpp_phys_handoff_periph_ranges(size_t* count) {
  std::span<const MappedMmioRange> periph_ranges = gPhysHandoff->periph_ranges.get();
  *count = periph_ranges.size();
  return periph_ranges.data();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_mapped_mmio_range_paddr(const MappedMmioRange& range) {
  return range.paddr();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uintptr_t cpp_mapped_mmio_range_vaddr(const MappedMmioRange& range) {
  return range.vaddr();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE size_t cpp_mapped_mmio_range_size_bytes(const MappedMmioRange& range) {
  return range.size_bytes();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_phys_handoff_platform_id(zbi_platform_id_t* out) {
  std::optional<zbi_platform_id_t> platform_id = gPhysHandoff->platform_id.to_std();
  if (!platform_id) {
    return false;
  }
  *out = *platform_id;
  return true;
}

}  // extern "C"

// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <stdint.h>

#include <kernel/ffi.h>
#include <phys/handoff.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const PhysHandoff* cpp_phys_handoff_get();

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const PhysHandoff* cpp_phys_handoff_get() { return gPhysHandoff; }

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
FFI_ALWAYS_INLINE bool cpp_phys_handoff_platform_id(zbi_platform_id_t* out) {
  std::optional<zbi_platform_id_t> platform_id = gPhysHandoff->platform_id.to_std();
  if (!platform_id) {
    return false;
  }
  *out = *platform_id;
  return true;
}

}  // extern "C"

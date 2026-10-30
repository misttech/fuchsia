// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "kernel/cpu_distance_map.h"

#include <kernel/ffi.h>

lazy_init::LazyInit<CpuDistanceMap> g_cpu_distance_map;

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_cpu_distance_map_init(size_t cpu_count, CpuDistanceCallback cb,
                                                 void* ctx) {
  CpuDistanceMap::Initialize(
      cpu_count, [cb, ctx](cpu_num_t from_id, cpu_num_t to_id) { return cb(from_id, to_id, ctx); });
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_cpu_distance_map_set_distance_threshold(uint32_t threshold) {
  CpuDistanceMap::Get().set_distance_threshold(threshold);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_cpu_distance_map_dump() { CpuDistanceMap::Get().Dump(); }

}  // extern "C"

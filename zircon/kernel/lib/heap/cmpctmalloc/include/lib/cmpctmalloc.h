// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2015 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_HEAP_CMPCTMALLOC_INCLUDE_LIB_CMPCTMALLOC_H_
#define ZIRCON_KERNEL_LIB_HEAP_CMPCTMALLOC_INCLUDE_LIB_CMPCTMALLOC_H_

#include <lib/cmpctmalloc_ffi.h>
#include <stddef.h>
#include <stdint.h>
#include <zircon/compiler.h>

#include <utility>

#include <fbl/enum_bits.h>

enum class CmpctDumpOptions : uint8_t {
  None = 0x0,
  PanicTime = 0x1,
  Verbose = 0x2,
};
FBL_ENABLE_ENUM_BITS(CmpctDumpOptions)

__BEGIN_CDECLS

// The maximum size that |cmpct_alloc| can allocate. Any larger of a size would
// yield a nullptr.
extern const size_t kHeapMaxAllocSize;

__END_CDECLS

inline void* cmpct_alloc(size_t size) { return rust_cmpct_alloc(size); }
inline void cmpct_free(void* payload) { rust_cmpct_free(payload); }
inline void cmpct_sized_free(void* payload, size_t s) { rust_cmpct_sized_free(payload, s); }
inline void* cmpct_memalign(size_t alignment, size_t size) {
  return rust_cmpct_memalign(alignment, size);
}

// Zero-fill allocations smaller than |size|
inline void cmpct_set_fill_on_alloc_threshold(size_t size) {
  rust_cmpct_set_fill_on_alloc_threshold(size);
}
inline void cmpct_init() { rust_cmpct_init(); }
inline void cmpct_dump(CmpctDumpOptions options) { rust_cmpct_dump(std::to_underlying(options)); }
inline void cmpct_get_info(size_t* used_bytes, size_t* free_bytes, size_t* cached_bytes) {
  rust_cmpct_get_info(used_bytes, free_bytes, cached_bytes);
}
inline void cmpct_test() { rust_cmpct_test(); }

// Get and set a user defined cookie against an allocation returned from cmpct_alloc
// or cmpct_memalign. |ptr| must be non-null and these methods are not thread
// safe.
inline void cmpct_set_cookie(void* ptr, uint32_t cookie) { rust_cmpct_set_cookie(ptr, cookie); }
inline uint32_t cmpct_get_cookie(void* ptr) { return rust_cmpct_get_cookie(ptr); }
// Returns the allocation size rounded up to the allocator bucket size.
inline uint32_t cmpct_get_size(void* ptr) { return rust_cmpct_get_size(ptr); }

#endif  // ZIRCON_KERNEL_LIB_HEAP_CMPCTMALLOC_INCLUDE_LIB_CMPCTMALLOC_H_

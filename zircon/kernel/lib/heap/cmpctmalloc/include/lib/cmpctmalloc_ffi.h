// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_HEAP_CMPCTMALLOC_INCLUDE_LIB_CMPCTMALLOC_FFI_H_
#define ZIRCON_KERNEL_LIB_HEAP_CMPCTMALLOC_INCLUDE_LIB_CMPCTMALLOC_FFI_H_

#include <stddef.h>
#include <stdint.h>
#include <zircon/compiler.h>

__BEGIN_CDECLS

void* rust_cmpct_alloc(size_t size);
void rust_cmpct_free(void* payload);
void rust_cmpct_sized_free(void* payload, size_t s);
void* rust_cmpct_memalign(size_t alignment, size_t size);
void rust_cmpct_set_fill_on_alloc_threshold(size_t size);
void rust_cmpct_init(void);
void rust_cmpct_dump(uint8_t options);
void rust_cmpct_get_info(size_t* used_bytes, size_t* free_bytes, size_t* cached_bytes);
void rust_cmpct_test(void);
void rust_cmpct_set_cookie(void* ptr, uint32_t cookie);
uint32_t rust_cmpct_get_cookie(void* ptr);
uint32_t rust_cmpct_get_size(void* ptr);

__END_CDECLS

#endif  // ZIRCON_KERNEL_LIB_HEAP_CMPCTMALLOC_INCLUDE_LIB_CMPCTMALLOC_FFI_H_

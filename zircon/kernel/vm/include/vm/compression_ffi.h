// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_COMPRESSION_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_COMPRESSION_FFI_H_

#include <zircon/compiler.h>

#include <kernel/ffi.h>

#include "vm/compression.h"

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
__BEGIN_CDECLS

struct FfiCompressResult {
  uint8_t tag;
  uint32_t ref;
  vm_page_t* fail_page;
  uint32_t fail_metadata;
};

FFI_ALWAYS_INLINE void cpp_vmcompression_destroy(VmCompression* compression);
FFI_ALWAYS_INLINE void cpp_vmcompression_free(VmCompression* compression);
FFI_ALWAYS_INLINE fbl::RefCounted<VmCompression>* cpp_vmcompression_get_ref_counted(
    VmCompression* compression);
FFI_ALWAYS_INLINE void cpp_vmcompression_acquire_compressor(
    ffi::Uninitialized<VmCompression::CompressorGuard>* guard, VmCompression* compression);
FFI_ALWAYS_INLINE void cpp_vmcompression_compressor_guard_destroy(
    VmCompression::CompressorGuard* guard);
FFI_ALWAYS_INLINE VmCompressor* cpp_vmcompression_compressor_guard_get(
    VmCompression::CompressorGuard* guard);
FFI_ALWAYS_INLINE FfiCompressResult cpp_vmcompression_compress(VmCompression* compression,
                                                               const void* page_src);
FFI_ALWAYS_INLINE void cpp_vmcompression_set_metadata(VmCompression* compression, uint32_t ref,
                                                      uint32_t metadata);
FFI_ALWAYS_INLINE void cpp_vmcompression_free_reference(VmCompression* compression, uint32_t ref);
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wreturn-type-c-linkage"
FFI_ALWAYS_INLINE VmCompression::Stats cpp_vmcompression_get_stats(
    const VmCompression* compression);
#pragma clang diagnostic pop

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_COMPRESSION_FFI_H_

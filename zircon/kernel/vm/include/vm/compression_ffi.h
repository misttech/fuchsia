// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_COMPRESSION_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_COMPRESSION_FFI_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

#include <fbl/ref_counted.h>
#include <kernel/ffi.h>
#include <vm/compression.h>

__BEGIN_CDECLS

// C++ -> Rust FFI shims for VmCompressedStorage and VmCompressionStrategy.

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_vm_compressed_storage_free_instance(VmCompressedStorage* storage);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void* cpp_vm_compressed_storage_get_ref_counted(
    const VmCompressedStorage* storage);

VmCompressedStorage* cpp_vm_slot_page_storage_create();

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_vm_compressed_storage_store(VmCompressedStorage* storage,
                                                       vm_page_t* page, size_t len,
                                                       uint32_t* out_ref, vm_page_t** out_page);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_vm_compressed_storage_free(VmCompressedStorage* storage, uint32_t ref);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const void* cpp_vm_compressed_storage_compressed_data(
    const VmCompressedStorage* storage, uint32_t ref, uint32_t* out_metadata, size_t* out_len);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint32_t cpp_vm_compressed_storage_get_metadata(VmCompressedStorage* storage,
                                                                  uint32_t ref);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_vm_compressed_storage_set_metadata(VmCompressedStorage* storage,
                                                              uint32_t ref, uint32_t metadata);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_vm_compressed_storage_dump(const VmCompressedStorage* storage);

#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wreturn-type-c-linkage"
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE VmCompressedStorage::MemoryUsage cpp_vm_compressed_storage_get_memory_usage(
    const VmCompressedStorage* storage);
#pragma clang diagnostic pop

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_vm_compression_strategy_free_instance(VmCompressionStrategy* strategy);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void* cpp_vm_compression_strategy_get_ref_counted(
    const VmCompressionStrategy* strategy);

VmCompressionStrategy* cpp_vm_lz4_compressor_create();

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint8_t cpp_vm_compression_strategy_compress(VmCompressionStrategy* strategy,
                                                               const void* src, void* dst,
                                                               size_t dst_limit, size_t* out_size);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_vm_compression_strategy_decompress(VmCompressionStrategy* strategy,
                                                              const void* src, size_t src_len,
                                                              void* dst);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_vm_compression_strategy_dump(const VmCompressionStrategy* strategy);

// Rust -> C++ FFI shims for VmCompression and VmCompression::CompressorGuard.

void rust_vm_compression_recycle(VmCompression* compression);
VmCompression* rust_vm_compression_create(VmCompressedStorage* storage,
                                          VmCompressionStrategy* strategy,
                                          size_t compression_threshold);
void rust_vm_compression_decompress(const VmCompression* compression, uint32_t ref, void* page_dest,
                                    uint32_t* metadata_dest);
void rust_vm_compression_free(const VmCompression* compression, uint32_t ref);
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wreturn-type-c-linkage"
VmCompressor::PageAndMetadata rust_vm_compression_move_temp_reference(
    const VmCompression* compression, uint32_t ref);
#pragma clang diagnostic pop
uint32_t rust_vm_compression_get_metadata(const VmCompression* compression, uint32_t ref);
void rust_vm_compression_set_metadata(const VmCompression* compression, uint32_t ref,
                                      uint32_t metadata);
void rust_vm_compression_acquire_compressor(const VmCompression* compression,
                                            VmCompression::CompressorGuard* out_guard);
void rust_vm_compression_compressor_guard_destroy(VmCompression::CompressorGuard* guard);
VmCompressor* rust_vm_compression_compressor_guard_get(const VmCompression::CompressorGuard* guard);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_COMPRESSION_FFI_H_

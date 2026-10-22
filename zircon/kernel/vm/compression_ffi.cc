// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <zircon/assert.h>

#include <fbl/alloc_checker.h>
#include <fbl/ref_ptr.h>
#include <vm/compression_ffi.h>
#include <vm/lz4_compressor.h>
#include <vm/slot_page_storage.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_vm_compressed_storage_free_instance(VmCompressedStorage* storage) {
  delete storage;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void* cpp_vm_compressed_storage_get_ref_counted(
    const VmCompressedStorage* storage) {
  return const_cast<fbl::RefCounted<VmCompressedStorage>*>(
      static_cast<const fbl::RefCounted<VmCompressedStorage>*>(storage));
}

VmCompressedStorage* cpp_vm_slot_page_storage_create() {
  fbl::AllocChecker ac;
  fbl::RefPtr<VmCompressedStorage> storage =
      fbl::AdoptRef<VmSlotPageStorage>(new (&ac) VmSlotPageStorage());
  if (!ac.check()) {
    return nullptr;
  }
  return fbl::ExportToRawPtr(&storage);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_vm_compressed_storage_store(VmCompressedStorage* storage,
                                                       vm_page_t* page, size_t len,
                                                       uint32_t* out_ref, vm_page_t** out_page) {
  auto [maybe_ref, return_page] = storage->Store(page, len);
  *out_page = return_page;
  if (maybe_ref.has_value()) {
    *out_ref = maybe_ref->value();
    return true;
  }
  return false;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_vm_compressed_storage_free(VmCompressedStorage* storage, uint32_t ref) {
  storage->Free(VmCompressedStorage::CompressedRef{ref});
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const void* cpp_vm_compressed_storage_compressed_data(
    const VmCompressedStorage* storage, uint32_t ref, uint32_t* out_metadata, size_t* out_len) {
  auto [data, metadata, len] = storage->CompressedData(VmCompressedStorage::CompressedRef{ref});
  *out_metadata = metadata;
  *out_len = len;
  return data;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint32_t cpp_vm_compressed_storage_get_metadata(VmCompressedStorage* storage,
                                                                  uint32_t ref) {
  return storage->GetMetadata(VmCompressedStorage::CompressedRef{ref});
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_vm_compressed_storage_set_metadata(VmCompressedStorage* storage,
                                                              uint32_t ref, uint32_t metadata) {
  storage->SetMetadata(VmCompressedStorage::CompressedRef{ref}, metadata);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_vm_compressed_storage_dump(const VmCompressedStorage* storage) {
  storage->Dump();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE VmCompressedStorage::MemoryUsage cpp_vm_compressed_storage_get_memory_usage(
    const VmCompressedStorage* storage) {
  return storage->GetMemoryUsage();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_vm_compression_strategy_free_instance(VmCompressionStrategy* strategy) {
  delete strategy;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void* cpp_vm_compression_strategy_get_ref_counted(
    const VmCompressionStrategy* strategy) {
  return const_cast<fbl::RefCounted<VmCompressionStrategy>*>(
      static_cast<const fbl::RefCounted<VmCompressionStrategy>*>(strategy));
}

VmCompressionStrategy* cpp_vm_lz4_compressor_create() {
  fbl::RefPtr<VmCompressionStrategy> strategy = VmLz4Compressor::Create();
  return fbl::ExportToRawPtr(&strategy);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint8_t cpp_vm_compression_strategy_compress(VmCompressionStrategy* strategy,
                                                               const void* src, void* dst,
                                                               size_t dst_limit, size_t* out_size) {
  VmCompressionStrategy::CompressResult result = strategy->Compress(src, dst, dst_limit);
  if (const size_t* size = ktl::get_if<size_t>(&result)) {
    *out_size = *size;
    return 0;
  }
  if (ktl::holds_alternative<VmCompressionStrategy::ZeroTag>(result)) {
    return 1;
  }
  ZX_DEBUG_ASSERT(ktl::holds_alternative<VmCompressionStrategy::FailTag>(result));
  return 2;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_vm_compression_strategy_decompress(VmCompressionStrategy* strategy,
                                                              const void* src, size_t src_len,
                                                              void* dst) {
  strategy->Decompress(src, src_len, dst);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_vm_compression_strategy_dump(const VmCompressionStrategy* strategy) {
  strategy->Dump();
}

}  // extern "C"

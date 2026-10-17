// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/compression_ffi.h"

#include <kernel/ffi.h>
#include <ktl/memory.h>

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {

FFI_ALWAYS_INLINE void cpp_vmcompression_destroy(VmCompression* compression) {
  ktl::destroy_at(compression);
}

FFI_ALWAYS_INLINE void cpp_vmcompression_free(VmCompression* compression) { delete compression; }

FFI_ALWAYS_INLINE fbl::RefCounted<VmCompression>* cpp_vmcompression_get_ref_counted(
    VmCompression* compression) {
  return const_cast<fbl::RefCounted<VmCompression>*>(
      static_cast<const fbl::RefCounted<VmCompression>*>(compression));
}

FFI_ALWAYS_INLINE void cpp_vmcompression_acquire_compressor(
    ffi::Uninitialized<VmCompression::CompressorGuard>* guard, VmCompression* compression) {
  // `Initialize` move-constructs into `guard` and destroys the temporary returned by
  // `AcquireCompressor()`. This is safe because the move constructor transfers the lock, so the
  // temporary's destructor does not release it.
  guard->Initialize(compression->AcquireCompressor());
}

FFI_ALWAYS_INLINE void cpp_vmcompression_compressor_guard_destroy(
    VmCompression::CompressorGuard* guard) {
  ktl::destroy_at(guard);
}

FFI_ALWAYS_INLINE VmCompressor* cpp_vmcompression_compressor_guard_get(
    VmCompression::CompressorGuard* guard) {
  return &guard->get();
}

FFI_ALWAYS_INLINE FfiCompressResult cpp_vmcompression_compress(VmCompression* compression,
                                                               const void* page_src) {
  VmCompression::CompressResult res = compression->Compress(page_src);
  const auto* ref = ktl::get_if<VmCompression::CompressedRef>(&res);
  const auto* fail = ktl::get_if<VmCompression::FailTag>(&res);
  return FfiCompressResult{
      .tag = static_cast<uint8_t>(res.index()),
      .ref = ref ? ref->value() : 0,
      .fail_page = fail ? fail->src_page.page : nullptr,
      .fail_metadata = fail ? fail->src_page.metadata : 0,
  };
}

FFI_ALWAYS_INLINE void cpp_vmcompression_set_metadata(VmCompression* compression, uint32_t ref,
                                                      uint32_t metadata) {
  compression->SetMetadata(VmCompression::CompressedRef(ref), metadata);
}

FFI_ALWAYS_INLINE void cpp_vmcompression_free_reference(VmCompression* compression, uint32_t ref) {
  compression->Free(VmCompression::CompressedRef(ref));
}

FFI_ALWAYS_INLINE VmCompression::Stats cpp_vmcompression_get_stats(
    const VmCompression* compression) {
  return compression->GetStats();
}

FFI_ALWAYS_INLINE VmCompression* cpp_vmcompression_create_default() {
  fbl::RefPtr<VmCompression> compression = VmCompression::CreateDefault();
  return fbl::ExportToRawPtr(&compression);
}

FFI_ALWAYS_INLINE void cpp_vmcompression_dump(const VmCompression* compression) {
  compression->Dump();
}

}  // extern "C"

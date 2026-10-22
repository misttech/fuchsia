// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <zircon/assert.h>

#include <vm/compression.h>
#include <vm/compression_ffi.h>

void VmCompression::fbl_recycle() { rust_vm_compression_recycle(this); }

// static
fbl::RefPtr<VmCompression> VmCompression::Create(fbl::RefPtr<VmCompressedStorage> storage,
                                                 fbl::RefPtr<VmCompressionStrategy> strategy,
                                                 size_t compression_threshold) {
  VmCompression* raw = rust_vm_compression_create(
      fbl::ExportToRawPtr(&storage), fbl::ExportToRawPtr(&strategy), compression_threshold);
  return fbl::ImportFromRawPtr(raw);
}

void VmCompression::Decompress(CompressedRef ref, void* page_dest, uint32_t* metadata_dest) {
  *metadata_dest = 0;
  rust_vm_compression_decompress(this, ref.value(), page_dest, metadata_dest);
}

void VmCompression::Free(CompressedRef ref) { rust_vm_compression_free(this, ref.value()); }

VmCompression::PageAndMetadata VmCompression::MoveTempReference(CompressedRef ref) {
  return rust_vm_compression_move_temp_reference(this, ref.value());
}

uint32_t VmCompression::GetMetadata(CompressedRef ref) {
  return rust_vm_compression_get_metadata(this, ref.value());
}

void VmCompression::SetMetadata(CompressedRef ref, uint32_t metadata) {
  rust_vm_compression_set_metadata(this, ref.value(), metadata);
}

VmCompression::CompressorGuard::CompressorGuard(const VmCompression* compression) {
  rust_vm_compression_acquire_compressor(compression, this);
}

VmCompression::CompressorGuard::~CompressorGuard() {
  rust_vm_compression_compressor_guard_destroy(this);
}

VmCompressor& VmCompression::CompressorGuard::get() {
  return *rust_vm_compression_compressor_guard_get(this);
}

VmCompression::CompressorGuard VmCompression::AcquireCompressor() { return CompressorGuard(this); }

// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_COMPRESSION_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_COMPRESSION_FFI_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

#include <vm/compression.h>

__BEGIN_CDECLS

// Rust -> C++ FFI shims for VmCompression and VmCompression::CompressorGuard.

void rust_vm_compression_recycle(VmCompression* compression);
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

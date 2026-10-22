// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_COMPRESSOR_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_COMPRESSOR_FFI_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

#include "vm/compressor.h"

__BEGIN_CDECLS

zx_status_t rust_vm_compressor_arm(const VmCompressor* compressor);
uint32_t rust_vm_compressor_start(const VmCompressor* compressor,
                                  VmCompressor::PageAndMetadata src_page);
void rust_vm_compressor_compress(const VmCompressor* compressor);
uint8_t rust_vm_compressor_take_compression_result(
    const VmCompressor* compressor, uint32_t* out_ref,
    VmCompressor::PageAndMetadata* out_fail_src_page);
void rust_vm_compressor_finalize(const VmCompressor* compressor);
void rust_vm_compressor_free(const VmCompressor* compressor, uint32_t ref);
void rust_vm_compressor_return_temp_reference(const VmCompressor* compressor, uint32_t ref);
bool rust_vm_compressor_is_temp_reference(const VmCompressor* compressor, uint32_t ref);
bool rust_vm_compressor_is_temp_reference_in_use(const VmCompressor* compressor);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_COMPRESSOR_FFI_H_

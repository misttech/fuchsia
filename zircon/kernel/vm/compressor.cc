// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/compressor.h"

#include <zircon/assert.h>

#include "vm/compressor_ffi.h"

zx_status_t VmCompressor::Arm() { return rust_vm_compressor_arm(this); }

VmCompressor::CompressedRef VmCompressor::Start(VmCompressor::PageAndMetadata src_page) {
  return CompressedRef{rust_vm_compressor_start(this, src_page)};
}

void VmCompressor::Compress() { rust_vm_compressor_compress(this); }

VmCompressor::CompressResult VmCompressor::TakeCompressionResult() {
  uint32_t ref = 0;
  PageAndMetadata fail_src_page;
  uint8_t tag = rust_vm_compressor_take_compression_result(this, &ref, &fail_src_page);
  switch (tag) {
    case 0:
      return CompressedRef{ref};
    case 1:
      return ZeroTag{};
    case 2:
      return FailTag{.src_page = fail_src_page};
    default:
      ZX_PANIC("unreachable");
  }
}

void VmCompressor::Finalize() { rust_vm_compressor_finalize(this); }

void VmCompressor::Free(CompressedRef ref) { rust_vm_compressor_free(this, ref.value()); }

void VmCompressor::ReturnTempReference(CompressedRef ref) {
  rust_vm_compressor_return_temp_reference(this, ref.value());
}

bool VmCompressor::IsTempReference(CompressedRef ref) const {
  return rust_vm_compressor_is_temp_reference(this, ref.value());
}

bool VmCompressor::IsTempReferenceInUse() const {
  return rust_vm_compressor_is_temp_reference_in_use(this);
}

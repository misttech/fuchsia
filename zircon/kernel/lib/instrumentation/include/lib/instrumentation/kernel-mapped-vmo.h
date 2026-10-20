// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_INSTRUMENTATION_INCLUDE_LIB_INSTRUMENTATION_KERNEL_MAPPED_VMO_H_
#define ZIRCON_KERNEL_LIB_INSTRUMENTATION_INCLUDE_LIB_INSTRUMENTATION_KERNEL_MAPPED_VMO_H_

#include <fbl/macros.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <ktl/array.h>
#include <ktl/string_view.h>
#include <vm/vm_object.h>

class Handle;

// Implemented in instrumentation_ffi.rs.
extern "C" {
void rust_kernel_mapped_vmo_new(void* storage);
void rust_kernel_mapped_vmo_drop_in_place(void* storage);
zx_status_t rust_kernel_mapped_vmo_init(void* storage, VmObject* vmo, size_t offset, size_t size,
                                        const char* name, size_t name_len);
Handle* rust_kernel_mapped_vmo_publish(const void* storage, const char* vmo_name,
                                       size_t vmo_name_len, size_t stream_size);
vaddr_t rust_kernel_mapped_vmo_base_locking(const void* storage);
size_t rust_kernel_mapped_vmo_size_locking(const void* storage);
}  // extern "C"

// This maintains a kernel mapping from a VMO; mapped pages must stay pinned to
// prevent kernel-mode page faults.  Destroying this object unmaps and unpins.
class KernelMappedVmo {
 public:
  KernelMappedVmo() { rust_kernel_mapped_vmo_new(&storage_); }
  ~KernelMappedVmo() { rust_kernel_mapped_vmo_drop_in_place(&storage_); }

  DISALLOW_COPY_ASSIGN_AND_MOVE(KernelMappedVmo);

  // Initialize with a kernel mapping from the VMO.
  zx_status_t Init(fbl::RefPtr<VmObject> vmo, size_t offset, size_t size, const char* name) {
    return rust_kernel_mapped_vmo_init(&storage_, fbl::ExportToRawPtr(&vmo), offset, size, name,
                                       name ? __builtin_strlen(name) : 0);
  }

  // Publish this VMO to userland as a read-only handle using the given name
  // and stream size.
  Handle* Publish(ktl::string_view vmo_name, size_t stream_size) const {
    return rust_kernel_mapped_vmo_publish(&storage_, vmo_name.data(), vmo_name.size(), stream_size);
  }

  template <size_t N>
  Handle* Publish(const ktl::array<char, N>& name, size_t stream_size) const {
    return Publish(ktl::string_view(name.data(), name.size()), stream_size);
  }

  // Return the bounds of the mapping in the kernel address space.
  vaddr_t base_locking() const { return rust_kernel_mapped_vmo_base_locking(&storage_); }
  size_t size_locking() const { return rust_kernel_mapped_vmo_size_locking(&storage_); }

 private:
  // Holds the Rust `KernelMappedVmo`, constructed by `rust_kernel_mapped_vmo_new` and destroyed by
  // `rust_kernel_mapped_vmo_drop_in_place`.  Size and alignment are checked on both sides.
  OpaqueStorage<32, 8> storage_;
};

static_assert(sizeof(KernelMappedVmo) == 32);
static_assert(alignof(KernelMappedVmo) == 8);

#endif  // ZIRCON_KERNEL_LIB_INSTRUMENTATION_INCLUDE_LIB_INSTRUMENTATION_KERNEL_MAPPED_VMO_H_

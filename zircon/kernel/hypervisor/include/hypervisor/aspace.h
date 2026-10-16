// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_ASPACE_H_
#define ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_ASPACE_H_

#include <lib/zx/result.h>

#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <ktl/unique_ptr.h>
#include <ktl/utility.h>
#include <vm/vm_address_region.h>
#include <vm/vm_aspace.h>
#include <vm/vm_object.h>

namespace hypervisor {
class GuestPtr;
class GuestPhysicalAspace;
}  // namespace hypervisor

extern "C" {
void rust_guest_ptr_reset(hypervisor::GuestPtr* guest_ptr);
void* rust_guest_ptr_as(const hypervisor::GuestPtr* guest_ptr, size_t type_size);
zx_status_t rust_guest_physical_aspace_create(hypervisor::GuestPhysicalAspace* out);
void rust_guest_physical_aspace_destroy(hypervisor::GuestPhysicalAspace* gpa);
size_t rust_guest_physical_aspace_size(const hypervisor::GuestPhysicalAspace* gpa);
ArchVmAspace* rust_guest_physical_aspace_arch_aspace(hypervisor::GuestPhysicalAspace* gpa);
VmAddressRegion* rust_guest_physical_aspace_root_vmar(const hypervisor::GuestPhysicalAspace* gpa);
bool rust_guest_physical_aspace_is_mapped(const hypervisor::GuestPhysicalAspace* gpa,
                                          zx_gpaddr_t guest_paddr);
zx_status_t rust_guest_physical_aspace_map_interrupt_controller(
    hypervisor::GuestPhysicalAspace* gpa, zx_gpaddr_t guest_paddr, zx_paddr_t host_paddr,
    size_t len);
zx_status_t rust_guest_physical_aspace_unmap_range(hypervisor::GuestPhysicalAspace* gpa,
                                                   zx_gpaddr_t guest_paddr, size_t len);
zx_status_t rust_guest_physical_aspace_page_fault(hypervisor::GuestPhysicalAspace* gpa,
                                                  zx_gpaddr_t guest_paddr);
zx_status_t rust_guest_physical_aspace_create_guest_ptr(hypervisor::GuestPhysicalAspace* gpa,
                                                        zx_gpaddr_t guest_paddr, size_t len,
                                                        const char* name,
                                                        hypervisor::GuestPtr* out);
}  // extern "C"

namespace hypervisor {

// RAII object that holds a mapping of guest physical address space to the host
// kernel virtual address space. Can be used to map a frequently accessed
// portion of guest physical memory for faster access.
class GuestPtr {
 public:
  GuestPtr() = default;
  GuestPtr(GuestPtr&& other) noexcept : storage_(ktl::exchange(other.storage_, {})) {}
  GuestPtr& operator=(GuestPtr&& other) noexcept {
    if (this != &other) {
      reset();
      storage_ = ktl::exchange(other.storage_, {});
    }
    return *this;
  }
  ~GuestPtr() { reset(); }

  GuestPtr(const GuestPtr&) = delete;
  GuestPtr& operator=(const GuestPtr&) = delete;

  void reset() { rust_guest_ptr_reset(this); }

  template <typename T>
  T* as() const {
    return static_cast<T*>(rust_guest_ptr_as(this, sizeof(T)));
  }

 private:
  [[maybe_unused]] OpaqueStorage<40, 8> storage_{};
};
static_assert(sizeof(GuestPtr) == 40);
static_assert(alignof(GuestPtr) == 8);

class GuestPhysicalAspace {
 public:
  static zx::result<GuestPhysicalAspace> Create() {
    GuestPhysicalAspace gpa;
    if (zx_status_t status = rust_guest_physical_aspace_create(&gpa); status != ZX_OK) {
      return zx::error(status);
    }
    return zx::ok(ktl::move(gpa));
  }
  GuestPhysicalAspace() = default;
  GuestPhysicalAspace(GuestPhysicalAspace&& other) noexcept
      : storage_(ktl::exchange(other.storage_, {})) {}
  GuestPhysicalAspace& operator=(GuestPhysicalAspace&& other) noexcept {
    if (this != &other) {
      rust_guest_physical_aspace_destroy(this);
      storage_ = ktl::exchange(other.storage_, {});
    }
    return *this;
  }
  ~GuestPhysicalAspace() { rust_guest_physical_aspace_destroy(this); }

  GuestPhysicalAspace(const GuestPhysicalAspace&) = delete;
  GuestPhysicalAspace& operator=(const GuestPhysicalAspace&) = delete;

  size_t size() const { return rust_guest_physical_aspace_size(this); }
  ArchVmAspace& arch_aspace() { return *rust_guest_physical_aspace_arch_aspace(this); }
  fbl::RefPtr<VmAddressRegion> RootVmar() const {
    return fbl::ImportFromRawPtr(rust_guest_physical_aspace_root_vmar(this));
  }

  bool IsMapped(zx_gpaddr_t guest_paddr) const {
    return rust_guest_physical_aspace_is_mapped(this, guest_paddr);
  }
  zx::result<> MapInterruptController(zx_gpaddr_t guest_paddr, zx_paddr_t host_paddr, size_t len) {
    return zx::make_result(
        rust_guest_physical_aspace_map_interrupt_controller(this, guest_paddr, host_paddr, len));
  }
  zx::result<> UnmapRange(zx_gpaddr_t guest_paddr, size_t len) {
    return zx::make_result(rust_guest_physical_aspace_unmap_range(this, guest_paddr, len));
  }
  zx::result<> PageFault(zx_gpaddr_t guest_paddr) {
    return zx::make_result(rust_guest_physical_aspace_page_fault(this, guest_paddr));
  }
  zx::result<GuestPtr> CreateGuestPtr(zx_gpaddr_t guest_paddr, size_t len, const char* name) {
    GuestPtr guest_ptr;
    if (zx_status_t status =
            rust_guest_physical_aspace_create_guest_ptr(this, guest_paddr, len, name, &guest_ptr);
        status != ZX_OK) {
      return zx::error(status);
    }
    return zx::ok(ktl::move(guest_ptr));
  }

 private:
  [[maybe_unused]] OpaqueStorage<sizeof(uintptr_t), alignof(uintptr_t)> storage_{};
};
static_assert(sizeof(GuestPhysicalAspace) == sizeof(uintptr_t));
static_assert(alignof(GuestPhysicalAspace) == alignof(uintptr_t));

}  // namespace hypervisor

#endif  // ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_ASPACE_H_

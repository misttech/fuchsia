// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef SRC_DEVICES_PCI_DRIVERS_PCI_ALLOCATION_H_
#define SRC_DEVICES_PCI_DRIVERS_PCI_ALLOCATION_H_

#include <fidl/fuchsia.hardware.pci.root/cpp/wire.h>
#include <lib/ddk/debug.h>
#include <lib/zx/eventpair.h>
#include <lib/zx/resource.h>
#include <lib/zx/result.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <optional>

#include <fbl/macros.h>
#include <region-alloc/region-alloc.h>

// PciAllocations and PciAllocators are concepts internal to UpstreamNodes which
// track address space allocations across roots and bridges. PciAllocator is an
// interface for roots and bridges to provide allocators to downstream bridges
// for their own allocations.
//
// === The Life of a PciAllocation ===
// Allocations at the top level of the bus driver are provided by a
// PciRootALlocator. This allocator serves requests from PCI Bridges & Devices
// that are just under the root complex and fulfills them by requesting space
// from the platform bus driver over the fuchsia.hardware.pci.root protocol. When these bridges
// allocate their windows and bars from upstream they are requesting address
// space from the PciRootAllocator. The PciAllocations handed back to them
// contain a base/size pair, as well as a zx::resource corresponding to the
// given address space. A PciAllocation also has the ability to create a VMO
// constrained by the base / size it understands, which can be used for device bar
// allocations for drivers. If the requester of a PciAllocation is a Bridge
// fulfilling its bridge windows then the allocation is fed to the PciAllocators
// of that bridge. These allocators fulfill the same interface as
// PciRootAllocators, except they allow those bridges to provide for devices
// downstream of them.
//
// As a tree, the system looks like this:
//
//                               Root Protocol
//                                |         |
//                                v         v
//                           Bridge        Bridge
//                      (RootAllocator) (RootAllocator)
//                             |              |
//                             v              v
//                      RootAllocation  RootAllocation
//                            |               |
//                            v               v
//                          Bridge        Device (bar 4)
//                     (RegionAllocator)
//                      |          |
//                      v          v
//         RegionAllocation   RegionAllocation
//                 |                 |
//                 v                 v
//           Device (bar 2)     Device (bar 1)

namespace pci {

class PciAllocation {
 public:
  // Delete Copy and Assignment ctors
  DISALLOW_COPY_AND_ASSIGN_ALLOW_MOVE(PciAllocation);

  virtual ~PciAllocation() = default;
  virtual zx_paddr_t base() const = 0;
  virtual size_t size() const = 0;
  // Create a VMO bounded by the base/size of this allocation using the
  // provided resource. This is used to provide VMOs for device BAR
  // allocations.
  virtual zx::result<zx::vmo> CreateVmo() const;
  // Create a resource for use with zx_ioports_request in the device driver.
  virtual zx::result<zx::resource> CreateResource() const;
  fuchsia_hardware_pci_root::AddressSpace type() const { return type_; }

 protected:
  PciAllocation(fuchsia_hardware_pci_root::AddressSpace type, zx::resource&& resource)
      : type_(type), resource_(std::move(resource)) {}
  const zx::resource& resource() { return resource_; }

 private:
  // Allow PciRegionAllocator / Device to duplicate the resource for use further
  // down the bridge chain. The security implications of this are not a
  // concern because:
  // 1. The allocation object strictly bounds the VMO to the specified base & size
  // 2. The resource is already in the driver process's address space, so we're not
  //    leaking it anywhere out of band.
  // 3. Device needs to be able to pass a resource to PciProxy for setting
  //    IO permission bits.
  // This is only needed for PciRegionAllocators because PciRootAllocators do not
  // hold a backing PciAllocation object.
  const fuchsia_hardware_pci_root::AddressSpace type_;
  const zx::resource resource_;
  friend class PciRegionAllocator;
  friend class Device;
  const zx::resource& resource() const { return resource_; }
};

class PciRootAllocation final : public PciAllocation {
 public:
  PciRootAllocation(const fuchsia_hardware_pci_root::AddressSpace type, zx::resource resource,
                    zx::eventpair ep, zx_paddr_t base, size_t size)
      : PciAllocation(type, std::move(resource)), ep_(std::move(ep)), base_(base), size_(size) {}
  ~PciRootAllocation() final = default;

  zx_paddr_t base() const final { return base_; }
  size_t size() const final { return size_; }

 private:
  zx::eventpair ep_;
  const zx_paddr_t base_;
  const size_t size_;
  // The platform bus driver is notified the allocation is free when this eventpair is closed.
};

class PciRegionAllocation final : public PciAllocation {
 public:
  PciRegionAllocation(fuchsia_hardware_pci_root::AddressSpace type, zx::resource&& resource,
                      RegionAllocator::Region::UPtr&& region)
      : PciAllocation(type, std::move(resource)), region_(std::move(region)) {}

  zx_paddr_t base() const final { return region_->base; }
  size_t size() const final { return region_->size; }

 private:
  // The Region contains the base & size for the allocation through .base and .size
  const RegionAllocator::Region::UPtr region_;
};

// The base class for Root & Region allocators used by UpstreamNodes
class PciAllocator {
 public:
  virtual ~PciAllocator() = default;
  // Delete Copy and Assignment ctors
  DISALLOW_COPY_ASSIGN_AND_MOVE(PciAllocator);
  // Request a region of address space spanning from |base| to |base| + |size|
  // for a downstream device or bridge. If |base| is nullopt then the region can be
  // allocated from any base.
  virtual zx::result<std::unique_ptr<PciAllocation>> Allocate(std::optional<zx_paddr_t> base,
                                                              size_t size) = 0;
  // Provide this allocator with a PciAllocation, granting it ownership of that
  // range of address space for calls to Allocate.
  virtual zx_status_t SetParentAllocation(std::unique_ptr<PciAllocation> alloc) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  virtual fuchsia_hardware_pci_root::AddressSpace type() const {
    ZX_ASSERT(type_.has_value());
    return *type_;
  }

 protected:
  explicit PciAllocator(fuchsia_hardware_pci_root::AddressSpace type) : type_(type) {}
  PciAllocator() = default;

 private:
  std::optional<fuchsia_hardware_pci_root::AddressSpace> type_;
};

// PciRootAllocators are an implementation of PciAllocator designed
// to use the fuchsia.hardware.pci.root protocol for allocation, fulfilling the requirements
// for a PciRoot to implement the UpstreamNode interface.
class PciRootAllocator final : public PciAllocator {
 public:
  PciRootAllocator(const fidl::WireSyncClient<fuchsia_hardware_pci_root::Root>& root,
                   fuchsia_hardware_pci_root::AddressSpace type, bool low)
      : PciAllocator(type), root_(root), low_(low) {}
  ~PciRootAllocator() final = default;
  zx::result<std::unique_ptr<PciAllocation>> Allocate(std::optional<zx_paddr_t> base,
                                                      size_t size) final;

 private:
  // The bus driver outlives allocator objects.
  const fidl::WireSyncClient<fuchsia_hardware_pci_root::Root>& root_;
  // This denotes whether this allocator requests memory < 4GB. More detail
  // can be found in the explanation for mmio in root.h.
  const bool low_;
};

// PciRegionAllocators are a wrapper around RegionAllocators to allow Bridge
// objects to implement the UpstreamNode interface by using regions that they
// are provided by nodes further upstream. They hand out PciRegionAllocations
// which will release allocations back upstream if they go out of scope.
class PciRegionAllocator final : public PciAllocator {
 public:
  ~PciRegionAllocator() final = default;
  zx::result<std::unique_ptr<PciAllocation>> Allocate(std::optional<zx_paddr_t> base,
                                                      size_t size) override;
  zx_status_t SetParentAllocation(std::unique_ptr<PciAllocation> alloc) override;
  // Region allocators are unique in that they derive their time from their backing allocation.
  fuchsia_hardware_pci_root::AddressSpace type() const final {
    ZX_ASSERT(parent_alloc_);
    return parent_alloc_->type();
  }

 private:
  std::unique_ptr<PciAllocation> parent_alloc_;
  // Unlike a Root allocator which has bookkeeping handled by the PCI root, a
  // Region allocator has a backing RegionAllocator object to handle that
  // metadata.
  RegionAllocator allocator_;
};

}  // namespace pci

#endif  // SRC_DEVICES_PCI_DRIVERS_PCI_ALLOCATION_H_

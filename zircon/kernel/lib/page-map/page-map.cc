// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "lib/page-map.h"

namespace page_map {

zx::result<object_cache::UniquePtr<internal::Entry>> PageMap::MakeEntry(
    fbl::RefPtr<VmObjectPaged> vmo, size_t page_offset_in_vmo) {
  // Commit and pin.
  const size_t kMappingSize = kPageSize;
  zx_status_t status = vmo->CommitRangePinned(page_offset_in_vmo, kMappingSize, /*write=*/true);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  auto unpin = fit::defer([&]() { vmo->Unpin(page_offset_in_vmo, kMappingSize); });

  // Then map into the kernel aspace.
  fbl::RefPtr<VmAddressRegion> kernel_vmar =
      VmAspace::kernel_aspace()->RootVmar()->as_vm_address_region();
  zx::result<VmAddressRegion::MapResult> mapping_result =
      kernel_vmar->CreateVmMapping(0, kMappingSize, 0, 0, vmo, page_offset_in_vmo,
                                   ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE, "PageMap");
  if (mapping_result.is_error()) {
    return mapping_result.take_error();
  }
  auto unmap = fit::defer([&]() { mapping_result->mapping->Destroy(); });

  // Fault-in the range.
  status = mapping_result->mapping->MapRange(0, kMappingSize, true);
  if (status != ZX_OK) {
    return zx::error_result(status);
  }

  auto new_entry = allocator_.Allocate(*this, ktl::move(vmo), ktl::move(mapping_result->mapping));
  if (new_entry.is_error()) {
    return new_entry.take_error();
  }
  unmap.cancel();
  unpin.cancel();

  return zx::ok(ktl::move(new_entry.value()));
}

void PageMap::Release(internal::Entry& entry) {
  Map::PtrType entry_destroyer;
  {
    Guard<CriticalMutex> guard{get_lock()};
    if (entry.DecrementAccessorCount()) {
      // This was the last one.  Remove it from the container so we can destroy it without holding
      // the container's lock.
      entry_destroyer = map_.erase(entry);
    }
  }
}

}  // namespace page_map

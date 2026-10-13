// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_MAGMA_LIB_MAGMA_PLATFORM_ZIRCON_ZIRCON_PLATFORM_BUS_MAPPER_H_
#define SRC_GRAPHICS_MAGMA_LIB_MAGMA_PLATFORM_ZIRCON_ZIRCON_PLATFORM_BUS_MAPPER_H_

#include <lib/magma/platform/platform_bus_mapper.h>
#include <lib/zx/pmt.h>

#include <vector>

#include "zircon_platform_buffer.h"
#include "zircon_platform_handle.h"

namespace magma {

class ZirconPlatformBusMapper : public PlatformBusMapper {
 public:
  ZirconPlatformBusMapper(std::shared_ptr<ZirconPlatformHandle> bus_transaction_initiator)
      : bus_transaction_initiator_(std::move(bus_transaction_initiator)) {}

  std::unique_ptr<BusMapping> MapPageRangeBus(PlatformBuffer* buffer, uint64_t start_page_index,
                                              uint64_t page_count, bool writeable) override;
  std::unique_ptr<PlatformBuffer> CreateContiguousBuffer(size_t size, uint32_t alignment_log2,
                                                         const char* name) override;

  class BusMapping : public PlatformBusMapper::BusMapping {
   public:
    BusMapping(uint64_t page_offset, std::vector<uint64_t> page_addr, std::vector<zx::pmt> pmt,
               bool writeable)
        : page_offset_(page_offset),
          page_addr_(std::move(page_addr)),
          pmt_(std::move(pmt)),
          writeable_(writeable) {}
    ~BusMapping();

    uint64_t page_offset() override { return page_offset_; }
    uint64_t page_count() override { return page_addr_.size(); }

    std::vector<uint64_t>& Get() override { return page_addr_; }
    bool writeable() override { return writeable_; }

   private:
    uint64_t page_offset_;
    std::vector<uint64_t> page_addr_;
    std::vector<zx::pmt> pmt_;
    bool writeable_;
  };

 private:
  std::shared_ptr<ZirconPlatformHandle> bus_transaction_initiator_;
};

}  // namespace magma

#endif  // SRC_GRAPHICS_MAGMA_LIB_MAGMA_PLATFORM_ZIRCON_ZIRCON_PLATFORM_BUS_MAPPER_H_

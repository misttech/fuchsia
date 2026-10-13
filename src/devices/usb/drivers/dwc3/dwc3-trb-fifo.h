// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_DRIVERS_DWC3_DWC3_TRB_FIFO_H_
#define SRC_DEVICES_USB_DRIVERS_DWC3_DWC3_TRB_FIFO_H_

#include <cstdint>
#include <memory>

#include "src/devices/usb/drivers/dwc3/dwc3-fifo.h"
#include "src/devices/usb/drivers/dwc3/dwc3-types.h"

namespace dwc3 {

class TrbFifo : public Fifo<dwc3_trb_t> {
 public:
  zx::result<> Init(zx::bti& bti, bool cached) override {
    zx::result<bool> result = InitBuffer(bti, cached);
    if (result.is_error()) {
      fdf::error("Failed to init FIFO {}", result);
      return result.take_error();
    }

    if (*result) {
      // set up link TRB pointing back to the start of the fifo
      zx_paddr_t trb_phys = Fifo::GetPhys(first_);
      last_--;
      const zx_off_t offset = (last_ - first_) * sizeof(dwc3_trb_t);
      if (auto status =
              buffer_->ExecuteWriteOps(offset, sizeof(dwc3_trb_t),
                                       [&](uint8_t* ptr) {
                                         auto link_trb = reinterpret_cast<dwc3_trb_t*>(ptr);
                                         link_trb->ptr_low = (uint32_t)trb_phys;
                                         link_trb->ptr_high = (uint32_t)(trb_phys >> 32);
                                         link_trb->status = 0;
                                         link_trb->control = TRB_TRBCTL_LINK | TRB_HWO | TRB_CHN;
                                       });
          status.is_error()) {
        fdf::error("ExecuteWriteOps failed: {}", status);
        Release();
        return status.take_error();
      }
    } else {
      Reset();
    }
    trb_sizes_ = std::make_unique<uint32_t[]>(TotalSlots());
    return zx::ok();
  }

  void Release() override {
    trb_sizes_.reset();
    Fifo::Release();
  }

  zx::result<dwc3_trb_t> ReadOne() {
    const zx_off_t offset = (read_ - first_) * sizeof(dwc3_trb_t);
    if (auto status = buffer_->CacheFlushInvalidate(offset, sizeof(dwc3_trb_t));
        status.is_error()) {
      fdf::error("CacheFlushInvalidate failed: {}", status);
      return status.take_error();
    }
    return zx::ok(*read_);
  }

  dwc3_trb_t* current_read() { return read_; }

  // Returns the requested transfer size recorded for the TRB at |read_|.
  uint32_t current_read_size() const {
    ZX_DEBUG_ASSERT(trb_sizes_ != nullptr);
    return trb_sizes_[read_ - first_];
  }

  // Advances the write pointer, recording |trb_size| for the allocated TRB
  // slot so completed bytes can be computed after hardware overwrites the TRB
  // status field with the residual byte count.
  dwc3_trb_t* AdvanceWrite(uint32_t trb_size = 0) {
    ZX_DEBUG_ASSERT(trb_sizes_ != nullptr);
    trb_sizes_[write_ - first_] = trb_size;
    return Fifo::Advance(write_);
  }
  void AdvanceRead() {
    if (read_ == write_) {
      fdf::error("Advancing read_ past write_. Invalid!");
      return;
    }
    Fifo::Advance(read_);
  }

  void Reset() {
    if (!buffer_) {
      return;
    }
    const size_t len = (last_ - first_) * sizeof(dwc3_trb_t);
    if (auto status = buffer_->ExecuteWriteOps(
            0, len,
            [&](uint8_t* ptr) {
              auto trbs = reinterpret_cast<dwc3_trb_t*>(ptr);
              for (size_t i = 0; i < static_cast<size_t>(last_ - first_); i++) {
                trbs[i].control = 0;
              }
            });
        status.is_error()) {
      fdf::error("ExecuteWriteOps failed: {}", status);
    }
    write_ = first_;
    read_ = write_;
  }

  size_t MaxAvailableSlots() const { return TotalSlots() - 1; }

 private:
  // Stores the requested transfer size for each TRB slot in the FIFO.
  std::unique_ptr<uint32_t[]> trb_sizes_;
};

}  // namespace dwc3

#endif  // SRC_DEVICES_USB_DRIVERS_DWC3_DWC3_TRB_FIFO_H_

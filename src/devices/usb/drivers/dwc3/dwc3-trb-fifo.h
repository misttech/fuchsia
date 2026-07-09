// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_DRIVERS_DWC3_DWC3_TRB_FIFO_H_
#define SRC_DEVICES_USB_DRIVERS_DWC3_DWC3_TRB_FIFO_H_

#include "src/devices/usb/drivers/dwc3/dwc3-fifo.h"
#include "src/devices/usb/drivers/dwc3/dwc3-types.h"

namespace dwc3 {

class TrbFifo : public Fifo<dwc3_trb_t> {
 public:
  zx::result<> Init(zx::bti& bti, bool cached) override {
    bool needs_init = !buffer_;
    auto result = Fifo::Init(bti, cached);
    if (result.is_error()) {
      fdf::error("Failed to init FIFO {}", result);
      return result.take_error();
    }

    if (needs_init) {
      // set up link TRB pointing back to the start of the fifo
      zx_paddr_t trb_phys = Fifo::GetPhys(first_);
      last_--;
      last_->ptr_low = (uint32_t)trb_phys;
      last_->ptr_high = (uint32_t)(trb_phys >> 32);
      last_->status = 0;
      last_->control = TRB_TRBCTL_LINK | TRB_HWO;
      CacheFlushIfCached(buffer_.get(), (last_ - first_) * sizeof(dwc3_trb_t), sizeof(dwc3_trb_t));
    }
    return zx::ok();
  }

  const dwc3_trb_t& ReadOne() {
    const zx_off_t offset = (read_ - first_) * sizeof(dwc3_trb_t);
    CacheFlushInvalidateIfCashed(buffer_.get(), offset, sizeof(dwc3_trb_t));
    return *read_;
  }

  dwc3_trb_t* AdvanceWrite() { return Fifo::Advance(write_); }
  void AdvanceRead() {
    if (read_ == write_) {
      fdf::error("Advancing read_ past write_. Invalid!");
      return;
    }
    Fifo::Advance(read_);
  }

  void Reset() {
    for (auto x = first_; x < last_; x++) {
      x->control = 0;
    }
    write_ = first_;
    read_ = write_;
    CacheFlushIfCached(buffer_.get(), 0, (last_ - first_) * sizeof(dwc3_trb_t));
  }
};

}  // namespace dwc3

#endif  // SRC_DEVICES_USB_DRIVERS_DWC3_DWC3_TRB_FIFO_H_

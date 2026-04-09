// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "usb-dci-interface-server.h"

#include <fidl/fuchsia.hardware.usb.dci/cpp/wire.h>
#include <lib/trace/event.h>

#include "src/devices/usb/drivers/usb-peripheral/usb-peripheral.h"

// This header appears unused, but is required to appease the forward declaration of UsbFunction
// brought in by way of usb-peripheral.h (which is required).
#include "src/devices/usb/drivers/usb-peripheral/usb-function.h"

namespace usb_peripheral {

void UsbDciInterfaceServer::Control(ControlRequestView req, ControlCompleter::Sync& completer) {
  TRACE_DURATION("usb-peripheral", __func__);

  cpp20::span<uint8_t> span_write = req->write.get();

  drv_->CommonControl(
      req->setup, span_write,
      [completer = completer.ToAsync()](zx::result<std::vector<uint8_t>> result) mutable {
        if (result.is_error()) {
          completer.ReplyError(result.error_value());
          return;
        }
        fidl::Arena arena;
        completer.buffer(arena).ReplySuccess(
            fidl::VectorView<uint8_t>::FromExternal(result.value()));
      });
}

void UsbDciInterfaceServer::SetConnected(SetConnectedRequestView req,
                                         SetConnectedCompleter::Sync& completer) {
  TRACE_DURATION("usb-peripheral", __func__);
  drv_->OnHostConnectionChanged(req->is_connected);
  completer.ReplySuccess();
}

void UsbDciInterfaceServer::SetSpeed(SetSpeedRequestView req, SetSpeedCompleter::Sync& completer) {
  TRACE_DURATION("usb-peripheral", __func__);
  drv_->speed_ = static_cast<usb_speed_t>(req->speed);
  completer.ReplySuccess();
}

void UsbDciInterfaceServer::Stop() {
  TRACE_DURATION("usb-peripheral", __func__);
  dispatcher_.ShutdownAsync();
  // Ensure the dispatcher is completely shut down before proceeding,
  // preventing any concurrent access to driver resources during teardown.
  dispatcher_shutdown_.Wait();
}

}  // namespace usb_peripheral

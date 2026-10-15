// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.usb.descriptor/cpp/wire.h>
#include <fidl/fuchsia.hardware.usb.policy/cpp/common_types_format.h>
#include <fidl/fuchsia.hardware.usb.policy/cpp/fidl.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/fit/defer.h>
#include <lib/trace/event.h>
#include <zircon/errors.h>

#include <mutex>

#include <fbl/algorithm.h>
#include <usb/descriptors.h>

#include "src/devices/usb/drivers/dwc3/dwc3-regs.h"
#include "src/devices/usb/drivers/dwc3/dwc3.h"

namespace dwc3 {

namespace fdescriptor = fuchsia_hardware_usb_descriptor;
namespace fpolicy = fuchsia_hardware_usb_policy;

namespace {

bool IsTestModeRequest(const fdescriptor::wire::UsbSetup& setup) {
  return setup.bm_request_type == kStandardDeviceOut &&
         (setup.b_request == fidl::ToUnderlying(fdescriptor::StandardRequest::kSetFeature) ||
          setup.b_request == fidl::ToUnderlying(fdescriptor::StandardRequest::kClearFeature)) &&
         setup.w_value == fidl::ToUnderlying(fdescriptor::FeatureSelector::kDeviceTestMode);
}

}  // namespace

zx_status_t Dwc3::Ep0Init() {
  TRACE_DURATION("dwc3", "Dwc3::Ep0Init");
  // Always use a cached TRB FIFO for EP0.
  if (zx::result result = ep0_.shared_fifo.Init(bti_, /*cached=*/true); result.is_error()) {
    return result.error_value();
  }
  ep0_.out.queued_trbs = 0;
  ep0_.in.queued_trbs = 0;

  const std::array eps{&ep0_.out, &ep0_.in};
  for (Endpoint* ep : eps) {
    ep->max_packet_size = kEp0MaxPacketSize;
    ep->type = fdescriptor::EndpointType::kControl;
    ep->interval = 0;
  }
  ep0_.out.usb_endpoint_address = 0x00;
  ep0_.in.usb_endpoint_address = 0x80;

  return ZX_OK;
}

void Dwc3::Ep0Start() {
  TRACE_DURATION("dwc3", "Dwc3::Ep0Start");
  if (!CmdStartNewConfig(ep0_.out, 0)) {
    fdf::error("CmdStartNewConfig failed");
    ResetEndpoints();
    return;
  }
  EpSetConfig(ep0_.out, true);
  EpSetConfig(ep0_.in, true);

  Ep0QueueSetup();
}

void Dwc3::Ep0QueueSetup() {
  TRACE_DURATION("dwc3", "Dwc3::Ep0QueueSetup");
  ep0_.setup_generation++;
  Ep0CancelTransfers();
  ep0_.test_mode = 0;
  if (auto status = ep0_.buffer->CacheFlushInvalidate(0, sizeof(fdescriptor::wire::UsbSetup));
      status.is_error()) {
    fdf::error("CacheFlushInvalidate failed: {}", status);
    return;
  }
  EpStartSingleTransfer(ep0_.out, ep0_.shared_fifo, TRB_TRBCTL_SETUP, ep0_.buffer->phys(),
                        sizeof(fdescriptor::wire::UsbSetup));
  ep0_.state = Ep0::State::Setup;
}

void Dwc3::Ep0StartEndpoints() {
  TRACE_DURATION("dwc3", "Dwc3::Ep0StartEndpoints");
  fdf::debug("Dwc3::Ep0StartEndpoints");

  ep0_.in.type = fdescriptor::EndpointType::kControl;
  ep0_.in.interval = 0;
  CmdEpSetConfig(ep0_.in, true);

  // The hard-coded value of '2' here is required by specification, see 'Start
  // New Configuration (DEPSTARTCFG)' in the programming guide. We call this
  // function upon receiving a SetConfiguration call, prior to setting up
  // endpoints > 1.
  CmdStartNewConfig(ep0_.out, 2);
}

void Dwc3::HandleEp0TransferCompleteEvent(uint8_t ep_num) {
  TRACE_DURATION("dwc3", "Dwc3::HandleEp0TransferCompleteEvent", "ep_num", ep_num);
  ZX_ASSERT(is_ep0_num(ep_num));
  auto& ep = (ep_num == kEp0Out) ? ep0_.out : ep0_.in;
  auto& other_ep = (ep_num == kEp0Out) ? ep0_.in : ep0_.out;
  ep.transfer_state = Endpoint::TransferState::kIdle;
  ep.rsrc_id = Endpoint::kInvalidResourceId;
  if (ep0_.out.stale_starts_to_end > 0 || ep0_.in.stale_starts_to_end > 0) {
    const size_t live_trbs = (ep.queued_trbs > 0) ? 1 : 0;
    const size_t stale_trbs = (ep.queued_trbs - live_trbs) + other_ep.queued_trbs;
    ep0_.shared_fifo.SkipTrbs(stale_trbs);
    ep.queued_trbs = live_trbs;
    other_ep.queued_trbs = 0;
    ep0_.out.stale_starts_to_end = 0;
    ep0_.in.stale_starts_to_end = 0;
  }

  // Only DataOut and DataIn states need TRB read.
  dwc3_trb_t trb{};
  if (ep0_.state == Ep0::State::DataOut || ep0_.state == Ep0::State::DataIn) {
    if (auto res = ep0_.shared_fifo.ReadOne(); res.is_ok()) {
      trb = *res;
    } else {
      fdf::error("ReadOne failed: {}", res.status_string());
      return;
    }
  }
  if (ep.queued_trbs > 0) {
    ep.queued_trbs--;
    ep0_.shared_fifo.AdvanceRead();
  }

  switch (ep0_.state) {
    case Ep0::State::Setup: {
      // Control Endpoint stall is cleared upon receiving SETUP.
      ep0_.out.stalled = false;

      if (auto setup = ep0_.buffer->ReadStruct<decltype(ep0_.cur_setup)>(); setup.is_ok()) {
        ep0_.cur_setup = *setup;
      } else {
        fdf::error("ReadStruct failed: {}", setup.status_string());
        Ep0EndAndStall(ep0_.out);
        Ep0QueueSetup();
        break;
      }

      fdf::debug("got setup: type: 0x{:02x} req: {} value: {} index: {} length: {}",
                 ep0_.cur_setup.bm_request_type, ep0_.cur_setup.b_request, ep0_.cur_setup.w_value,
                 ep0_.cur_setup.w_index, ep0_.cur_setup.w_length);

      const bool is_two_stage = ep0_.cur_setup.w_length == 0;
      const bool is_out = usb_request_is_out(ep0_.cur_setup.bm_request_type);

      if (is_two_stage || IsTestModeRequest(ep0_.cur_setup)) {
        ep0_.state = Ep0::State::TwoStage;
        HandleEp0Setup(0);
        break;
      }

      // For out-type three-stage transfers, data is first read from the host and then passed up
      // through the stack. For all in-type transfers, the stack generates in-data, and then
      // transfers it to the host.
      if (is_out) {
        // The DWC3 controller requires OUT TRB lengths to be multiples of the endpoint's max
        // packet size, and only retires an unchained OUT TRB when a short packet arrives or
        // TRB_BUFSIZ reaches zero. Round w_length up to ep0_.out.max_packet_size so MPS-aligned
        // transfers reach TRB_BUFSIZ == 0 while unaligned transfers remain MPS-aligned and retire
        // on the final short packet.
        ZX_DEBUG_ASSERT(ep0_.out.max_packet_size > 0);
        ep0_.cur_transfer_len =
            fbl::round_up<size_t>(ep0_.cur_setup.w_length, ep0_.out.max_packet_size);
        ZX_DEBUG_ASSERT(ep0_.cur_transfer_len <= ep0_.buffer->size());
        EpStartSingleTransfer(ep0_.out, ep0_.shared_fifo, TRB_TRBCTL_CONTROL_DATA,
                              ep0_.buffer->phys(), ep0_.cur_transfer_len);
        ep0_.state = Ep0::State::DataOut;
      } else {
        ep0_.state = Ep0::State::DataIn;
        HandleEp0Setup(ep0_.buffer->size());
      }
      break;
    }
    case Ep0::State::DataOut: {
      if (ep_num != kEp0Out) {
        // This indicates a disagreement between the host and controller about the directionality of
        // the data exchange. In this case, the setup packet indicated a control-write (OUT-type
        // transfer), which would involve a DataOut packet. The controller actually received an
        // unexpected DataIn packet from the host. To recover, gracefully stall and reset the
        // transfer.

        fdf::warn(
            "host/target data direction disagreement, expected data-out, got data-in "
            "(cur_setup: req_type=0x{:02x}, req=0x{:02x}, val=0x{:04x}, idx=0x{:04x}, len={})",
            ep0_.cur_setup.bm_request_type, ep0_.cur_setup.b_request, ep0_.cur_setup.w_value,
            ep0_.cur_setup.w_index, ep0_.cur_setup.w_length);
        Ep0EndAndStall(ep0_.out);
        Ep0QueueSetup();
        break;
      }

      const size_t expected = ep0_.cur_transfer_len;
      const size_t remaining = TRB_BUFSIZ(trb.status);
      zx_off_t received = 0;
      if (remaining > expected) {
        fdf::error(
            "Underflow detected on Ep0 OUT: expected {}, remaining {}. Clamping received to 0",
            expected, remaining);
      } else {
        received = expected - remaining;
      }
      if (received > ep0_.cur_setup.w_length) {
        fdf::error(
            "DataOut overflow: received {}, w_length {} "
            "(cur_setup: req_type=0x{:02x}, req=0x{:02x}, val=0x{:04x}, idx=0x{:04x})",
            received, ep0_.cur_setup.w_length, ep0_.cur_setup.bm_request_type,
            ep0_.cur_setup.b_request, ep0_.cur_setup.w_value, ep0_.cur_setup.w_index);
        metrics_.RecordEvent(std::format(
            "ep0: Stalled DataOut overflow "
            "[type=0x{:02x} req=0x{:02x} val=0x{:04x} idx=0x{:04x} len={} received={}]",
            ep0_.cur_setup.bm_request_type, ep0_.cur_setup.b_request, ep0_.cur_setup.w_value,
            ep0_.cur_setup.w_index, ep0_.cur_setup.w_length, received));
        Ep0EndAndStall(ep0_.out);
        Ep0QueueSetup();
        break;
      }
      ep0_.out.total_transfers++;
      ep0_.out.total_bytes += received;
      ep0_.state = Ep0::State::WaitNrdyIn;
      if (auto status = ep0_.buffer->CacheFlushInvalidate(0, ep0_.buffer->size());
          status.is_error()) {
        fdf::error("CacheFlushInvalidate failed: {}", status);
        Ep0EndAndStall(ep0_.out);
        Ep0QueueSetup();
        break;
      }
      HandleEp0Setup(received);
      break;
    }
    case Ep0::State::DataIn: {
      if (ep_num != kEp0In) {
        // See above, but reverse the directionality for a control-read.
        fdf::warn(
            "host/target data direction disagreement, expected data-in, got data-out "
            "(cur_setup: req_type=0x{:02x}, req=0x{:02x}, val=0x{:04x}, idx=0x{:04x}, len={})",
            ep0_.cur_setup.bm_request_type, ep0_.cur_setup.b_request, ep0_.cur_setup.w_value,
            ep0_.cur_setup.w_index, ep0_.cur_setup.w_length);
        Ep0EndAndStall(ep0_.in);
        Ep0QueueSetup();
        break;
      }
      const size_t expected = ep0_.cur_transfer_len;
      const size_t remaining = TRB_BUFSIZ(trb.status);
      zx_off_t transferred = 0;
      if (remaining > expected) {
        fdf::error(
            "Underflow detected on Ep0 IN: expected {}, remaining {}. Clamping transferred to 0",
            expected, remaining);
      } else {
        transferred = expected - remaining;
      }
      ep0_.in.total_transfers++;
      ep0_.in.total_bytes += transferred;

      if (transferred < ep0_.cur_setup.w_length && (transferred % ep0_.in.max_packet_size) == 0 &&
          transferred > 0) {
        ep0_.state = Ep0::State::WaitZlpIn;
        ep0_.cur_transfer_len = 0;
        EpStartSingleTransfer(ep0_.in, ep0_.shared_fifo, TRB_TRBCTL_CONTROL_DATA, 0, 0);
        break;
      }

      ep0_.state = Ep0::State::WaitNrdyOut;
      break;
    }
    case Ep0::State::WaitZlpIn: {
      if (ep_num != kEp0In) {
        fdf::warn(
            "host/target data direction disagreement in WaitZlpIn: expected EP0 IN ({}), got {}, request=0x{:02x}",
            kEp0In, ep_num, ep0_.cur_setup.b_request);
        Ep0EndAndStall(ep0_.in);
        Ep0QueueSetup();
        break;
      }
      ep0_.in.total_transfers++;
      ep0_.state = Ep0::State::WaitNrdyOut;
      break;
    }
    case Ep0::State::Status:
      // Apply the latched test mode selector to DCTL.TSTCTL only after the Status stage has
      // completed, masking ULSTCHNGREQ to avoid triggering an unintended link state change.
      if (ep0_.test_mode != 0) {
        auto* mmio = get_mmio();
        DCTL::Get().ReadFrom(mmio).set_ULSTCHNGREQ(0).set_TSTCTL(ep0_.test_mode).WriteTo(mmio);
        ep0_.test_mode = 0;
      }
      Ep0QueueSetup();
      break;
    default:
      fdf::error("unexpected XferComplete state={}", ep0_.state);
      break;
  }
}

void Dwc3::HandleEp0TransferNotReadyEvent(uint8_t ep_num, uint32_t stage) {
  TRACE_DURATION("dwc3", "Dwc3::HandleEp0TransferNotReadyEvent", "ep_num", ep_num, "stage", stage);
  fdf::debug("Dwc3::HandleEp0TransferNotReadyEvent state {} stage {}", ep0_.state, stage);

  ZX_ASSERT(is_ep0_num(ep_num));

  switch (ep0_.state) {
    case Ep0::State::Setup:
      if ((stage == DEPEVT_XFER_NOT_READY_STAGE_DATA) ||
          (stage == DEPEVT_XFER_NOT_READY_STAGE_STATUS)) {
        // Stall if we receive XferNotReady(Data/Status) while waiting for setup to complete
        Ep0EndAndStall(ep0_.out);
        Ep0QueueSetup();
      }
      break;
    case Ep0::State::TwoStage:
      ZX_ASSERT(stage);  // Must be 1 or 2.
      if (stage == DEPEVT_XFER_NOT_READY_STAGE_DATA) {
        Ep0EndAndStall(ep0_.out);
        Ep0QueueSetup();
      } else {
        ep0_.state = Ep0::State::WaitFidl;
      }
      break;
    case Ep0::State::WaitHost:
      ZX_ASSERT(stage);  // Must be 1 or 2.
      if (stage == DEPEVT_XFER_NOT_READY_STAGE_DATA) {
        Ep0EndAndStall(ep0_.out);
        Ep0QueueSetup();
      } else {
        EpStartSingleTransfer(ep0_.in, ep0_.shared_fifo, TRB_TRBCTL_STATUS_2, 0, 0);
        ep0_.state = Ep0::State::Status;
      }
      break;
    case Ep0::State::DataOut:
      if ((ep_num == kEp0In) && (stage == DEPEVT_XFER_NOT_READY_STAGE_DATA)) {
        // End transfer and stall if we receive XferNotReady(Data) in the opposite direction.
        Ep0EndAndStall(ep0_.out);
        Ep0QueueSetup();
      }
      break;
    case Ep0::State::DataIn:
      if ((ep_num == kEp0Out) && (stage == DEPEVT_XFER_NOT_READY_STAGE_DATA)) {
        // End transfer and stall if we receive XferNotReady(Data) in the opposite direction.
        Ep0EndAndStall(ep0_.in);
        Ep0QueueSetup();
      }
      break;
    case Ep0::State::WaitNrdyOut:
      if (ep_num == kEp0Out) {
        EpStartSingleTransfer(ep0_.out, ep0_.shared_fifo, TRB_TRBCTL_STATUS_3, 0, 0);
        ep0_.state = Ep0::State::Status;
      }
      break;
    case Ep0::State::WaitNrdyIn:
      if (ep_num == kEp0In) {
        EpStartSingleTransfer(ep0_.in, ep0_.shared_fifo, TRB_TRBCTL_STATUS_3, 0, 0);
        ep0_.state = Ep0::State::Status;
      }
      break;
    case Ep0::State::WaitZlpIn:
      if ((ep_num == kEp0Out) && (stage == DEPEVT_XFER_NOT_READY_STAGE_DATA)) {
        // End transfer and stall if we receive XferNotReady(Data) in the opposite direction.
        Ep0EndAndStall(ep0_.in);
        Ep0QueueSetup();
      }
      break;
    case Ep0::State::Status:
    default:
      fdf::error("ready unhandled state {}", ep0_.state);
      break;
  }
}

void Dwc3::Ep0CancelTransfers() {
  for (Endpoint* ep : {&ep0_.out, &ep0_.in}) {
    if (is_active()) {
      if (ep->transfer_state == Endpoint::TransferState::kStartingSingle) {
        ep->stale_starts_to_end++;
      } else if (ep->TransferStateIsActive() && ep->rsrc_id != Endpoint::kInvalidResourceId) {
        CmdEpEndTransfer(*ep, /*cmd_ioc=*/false);
      }
    }
    ep->transfer_state = Endpoint::TransferState::kIdle;
    ep->rsrc_id = Endpoint::kInvalidResourceId;
  }
  // While a DEPSTRTXFER is still in flight on either EP0 direction (stale_starts_to_end > 0), the
  // controller may still be fetching the TRB at the head of ep0_.shared_fifo: leave it alone until
  // all stale starts have been ended (HandleEpTransferStartedEvent) or the event ring is stopped
  // (StopEvents()).
  if (ep0_.out.stale_starts_to_end == 0 && ep0_.in.stale_starts_to_end == 0) {
    for (Endpoint* ep : {&ep0_.out, &ep0_.in}) {
      ep0_.shared_fifo.SkipTrbs(ep->queued_trbs);
      ep->queued_trbs = 0;
    }
    ZX_ASSERT_MSG(ep0_.shared_fifo.IsEmpty(),
                  "EP0 shared FIFO leaked %zu active TRBs after Ep0CancelTransfers",
                  ep0_.shared_fifo.GetActiveCount());
  }
}

void Dwc3::Ep0EndAndStall(Endpoint& ep) {
  Ep0CancelTransfers();
  EpSetStall(ep, true);
}

void Dwc3::HandleEp0Setup(size_t length) {
  TRACE_DURATION("dwc3", "Dwc3::HandleEp0Setup", "length", length);
  // Copy the setup packet to ensure it is correctly captured in the Then closure.
  fdescriptor::wire::UsbSetup setup = ep0_.cur_setup;

  auto fail = [this]() {
    Ep0EndAndStall(ep0_.out);
    Ep0QueueSetup();
  };

  if (setup.bm_request_type == kStandardDeviceOut) {
    // handle some special setup requests in this driver
    switch (setup.b_request) {
      case fidl::ToUnderlying(fdescriptor::StandardRequest::kSetAddress):
        SetDeviceAddress(setup.w_value);
        ep0_.state = Ep0::State::WaitHost;
        return;
      case fidl::ToUnderlying(fdescriptor::StandardRequest::kSetConfiguration): {
        ResetConfiguration();
        const uint64_t setup_gen = ep0_.setup_generation;
        WaitForAllUserEndpointsIdle([this, setup, length, setup_gen](bool idle) {
          if (!idle || !power_on_ || ep0_.setup_generation != setup_gen ||
              (ep0_.state != Ep0::State::TwoStage && ep0_.state != Ep0::State::WaitFidl)) {
            return;
          }
          Ep0StartEndpoints();
          DoControlCall(setup, length);
        });
        return;
      }
      case fidl::ToUnderlying(fdescriptor::StandardRequest::kSetFeature):
        if (setup.w_value == fidl::ToUnderlying(fdescriptor::FeatureSelector::kDeviceTestMode)) {
          const uint8_t test_selector = static_cast<uint8_t>(setup.w_index >> 8);
          if ((setup.w_index & 0xff) != 0 || setup.w_length != 0 ||
              test_selector < fidl::ToUnderlying(fdescriptor::TestModeSelector::kTestJ) ||
              test_selector > fidl::ToUnderlying(fdescriptor::TestModeSelector::kTestForceEnable)) {
            fail();
            return;
          }
          ep0_.test_mode = test_selector;
          ep0_.state = Ep0::State::WaitHost;
          return;
        }
        break;
      case fidl::ToUnderlying(fdescriptor::StandardRequest::kClearFeature):
        if (setup.w_value == fidl::ToUnderlying(fdescriptor::FeatureSelector::kDeviceTestMode)) {
          // The Test_Mode feature cannot be cleared by the ClearFeature() request.
          fail();
          return;
        }
        break;
      default:
        // fall through to the common DoControlCall
        break;
    }
  }

  DoControlCall(setup, length);
}

void Dwc3::DoControlCall(fdescriptor::wire::UsbSetup setup, size_t length) {
  auto fail = [this]() {
    Ep0EndAndStall(ep0_.out);
    Ep0QueueSetup();
  };
  if (!dci_intf_.is_valid()) {
    fail();
    return;
  }

  const bool is_out = usb_request_is_out(setup.bm_request_type);

  // We can't fit this in FIDL so we can't dispatch. Log loudly and fail.
  if (is_out && length > fuchsia_hardware_usb_dci::kMaxControlRequestLen) {
    fdf::error(
        "control data request too large ({}) bm_request_type=0x{:02X}, "
        "b_request=0x{:02X}, w_value=0x{:04X}, w_index=0x{:04X}, w_length={}",
        length, setup.bm_request_type, setup.b_request, setup.w_value, setup.w_index,
        setup.w_length);
    fail();
    return;
  }

  fidl::Arena arena;
  fidl::VectorView<uint8_t> out_payload;
  if (is_out && length > 0) {
    out_payload = fidl::VectorView<uint8_t>(arena, length);
    if (auto status = ep0_.buffer->Read(0, length, out_payload.data()); status.is_error()) {
      fdf::error("ep0 buffer Read failed: {}", status.status_string());
      fail();
      return;
    }
  }

  const uint64_t setup_gen = ep0_.setup_generation;
  dci_intf_.buffer(arena)
      ->Control(setup, out_payload)
      .Then([this, is_out, fail, length, setup,
             setup_gen](fidl::WireUnownedResult<fuchsia_hardware_usb_dci::UsbDciInterface::Control>&
                            result) {
        if (!power_on_ || !controller_started_) {
          // Return in case the core was powered off or disabled between the setup event and
          // the reply from our child.
          return;
        }

        if (ep0_.setup_generation != setup_gen) {
          fdf::warn("Ignoring stale Control() completion: gen {} != current {}", setup_gen,
                    ep0_.setup_generation);
          return;
        }

        if (!result.ok()) {
          fdf::error("(framework) Control() length = {}: {}", length, result.FormatDescription());
          metrics_.RecordEvent(
              std::format("ep0: Stalled setup request "
                          "[type=0x{:02x} req=0x{:02x} val=0x{:04x} idx=0x{:04x} len={}] "
                          "(framework error: {})",
                          setup.bm_request_type, setup.b_request, setup.w_value, setup.w_index,
                          setup.w_length, result.status_string()));
          fail();
          return;
        }
        if (result->is_error()) {
          if (result->error_value() != ZX_ERR_NOT_SUPPORTED) {
            fdf::error(
                "Control([type=0x{:02x} req=0x{:02x} val=0x{:04x} idx=0x{:04x} len={}]) request failed: {}",
                setup.bm_request_type, setup.b_request, setup.w_value, setup.w_index,
                setup.w_length, zx_status_get_string(result->error_value()));
          } else {
            fdf::debug("Control request failed: {}", zx_status_get_string(result->error_value()));
          }
          metrics_.RecordEvent(
              std::format("ep0: Stalled setup request "
                          "[type=0x{:02x} req=0x{:02x} val=0x{:04x} idx=0x{:04x} len={}] "
                          "(error: {})",
                          setup.bm_request_type, setup.b_request, setup.w_value, setup.w_index,
                          setup.w_length, zx_status_get_string(result->error_value())));
          fail();
          return;
        }

        switch (ep0_.state) {
          case Ep0::State::TwoStage:
            ep0_.state = Ep0::State::WaitHost;
            break;
          case Ep0::State::WaitFidl:
            EpStartSingleTransfer(ep0_.in, ep0_.shared_fifo, TRB_TRBCTL_STATUS_2, 0, 0);
            ep0_.state = Ep0::State::Status;
            break;
          case Ep0::State::WaitHost:
            // Nonsensical case that should never happen. See state commentary.
            fdf::error("Invalid Ep0 state");
            fail();
            break;
          default:
            if (!is_out) {
              // A lightweight byte-span is used to make it easier to process the read data.
              cpp20::span<uint8_t> read_data{result.value()->read.get()};
              // Don't blow out caller's buffer.
              if (read_data.size_bytes() > length) {
                fail();
                return;
              }

              if (!read_data.empty()) {
                if (auto status = ep0_.buffer->Write(read_data.data(), 0, read_data.size_bytes());
                    status.is_error()) {
                  fdf::error("Write failed: {}", status);
                  fail();
                  return;
                }
              }

              fdf::debug("HandleSetup success: actual {}", read_data.size_bytes());
              // queue a write for the data phase
              ep0_.cur_transfer_len = read_data.size_bytes();
              EpStartSingleTransfer(ep0_.in, ep0_.shared_fifo, TRB_TRBCTL_CONTROL_DATA,
                                    ep0_.buffer->phys(), read_data.size_bytes());
            }
        }

        if (setup.bm_request_type == kStandardDeviceOut &&
            setup.b_request == fdescriptor::StandardRequest::kSetConfiguration) {
          SetDeviceState(fpolicy::DeviceState::kConfigured);
        }
      });
}

}  // namespace dwc3

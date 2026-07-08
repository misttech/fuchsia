// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver/logging/cpp/logger.h>
#include <lib/trace/event.h>

#include "src/devices/usb/drivers/dwc3/dwc3-regs.h"
#include "src/devices/usb/drivers/dwc3/dwc3.h"

namespace dwc3 {

void Dwc3::EpEnable(Endpoint& ep, bool enable) {
  TRACE_DURATION("dwc3", "Dwc3::EpEnable", "ep_num", ep.ep_num, "enable", enable);
  auto* mmio = get_mmio();

  if (enable) {
    DALEPENA::Get().ReadFrom(mmio).EnableEp(ep.ep_num).WriteTo(mmio);
  } else {
    DALEPENA::Get().ReadFrom(mmio).DisableEp(ep.ep_num).WriteTo(mmio);
  }

  ep.enabled = enable;
}

void Dwc3::EpSetConfig(Endpoint& ep, bool enable) {
  TRACE_DURATION("dwc3", "Dwc3::EpSetConfig", "ep_num", ep.ep_num, "enable", enable);
  fdf::debug("Dwc3::EpSetConfig {}", ep.ep_num);

  if (enable) {
    CmdEpSetConfig(ep, false);
    CmdEpTransferConfig(ep);
    EpEnable(ep, true);
  } else {
    EpEnable(ep, false);
  }
}

zx_status_t Dwc3::EpSetStall(Endpoint& ep, bool stall) {
  TRACE_DURATION("dwc3", "Dwc3::EpSetStall", "ep_num", ep.ep_num, "stall", stall);
  if (!ep.enabled) {
    return ZX_ERR_BAD_STATE;
  }

  if (stall && !ep.stalled) {
    CmdEpSetStall(ep);
  } else if (!stall && ep.stalled) {
    CmdEpClearStall(ep);
  }

  ep.stalled = stall;
  return ZX_OK;
}

void Dwc3::EpStartTransfer(Endpoint& ep, TrbFifo& fifo, uint32_t type, zx_paddr_t buffer,
                           size_t length, bool zlp) {
  TRACE_DURATION("dwc3", "Dwc3::EpStartTransfer", "ep_num", ep.ep_num, "type", type, "length",
                 length, "zlp", zlp);
  fdf::debug("Dwc3::EpStartTransfer ep {} type %u length {} zlp {}", ep.ep_num, type, length, zlp);

  dwc3_trb_t* trb = fifo.AdvanceWrite();
  trb->ptr_low = static_cast<uint32_t>(buffer);
  trb->ptr_high = static_cast<uint32_t>(buffer >> 32);
  trb->status = TRB_BUFSIZ(static_cast<uint32_t>(length));

  zx_paddr_t trb_phys;
  if (zlp) {
    // NOTE: The DWC3 programming manual states support for a `Normal-ZLP`
    // transfer type that should be able to handle this. Alas, it doesn't
    // actually seem to work, so we just enqueue an actual zero length transfer
    // as a second TRB.
    trb->control = type | TRB_HWO;

    dwc3_trb_t* trb2 = fifo.AdvanceWrite();
    trb2->ptr_low = 0;
    trb2->ptr_high = 0;
    trb2->status = TRB_BUFSIZ(0);
    trb2->control = type | TRB_LST | TRB_IOC | TRB_HWO;

    trb_phys = fifo.Write(trb, 2);
  } else {
    trb->control = type | TRB_LST | TRB_IOC | TRB_HWO;
    trb_phys = fifo.Write(trb, 1);
  }

  CmdEpStartTransfer(ep, trb_phys);
  ep.xfer_in_progress = true;
}

void Dwc3::EpServer::CancelAll(zx_status_t reason) {
  TRACE_DURATION("dwc3", "Dwc3::EpServer::CancelAll", "ep_num", uep_->ep.ep_num, "reason", reason);
  fdf::debug("Dwc3::EpServer::CancelAll ep {} reason {}", uep_->ep.ep_num,
             zx_status_get_string(reason));

  if (current_req.has_value()) {
    dwc3_->CmdEpEndTransfer(uep_->ep);
    RequestComplete(reason, 0, std::move(current_req->request));
    uep_->ep.xfer_in_progress = false;
    current_req.reset();
  }

  for (; !queued_reqs.empty(); queued_reqs.pop()) {
    RequestComplete(reason, 0, std::move(queued_reqs.front()));
  }
  uep_->fifo.Clear();
}

void Dwc3::UserEpQueueNext(UserEndpoint& uep) {
  TRACE_DURATION("dwc3", "Dwc3::UserEpQueueNext", "ep_num", uep.ep.ep_num);
  if (uep.server->current_req.has_value() || !uep.ep.got_not_ready ||
      uep.server->queued_reqs.empty() || uep.ep.stalled) {
    return;
  }

  auto& pending_req = uep.server->queued_reqs.front();

  zx::result result = uep.server->get_iter(pending_req, zx_system_get_page_size());
  ZX_ASSERT_MSG(result.is_ok(), "[BUG] server->phys_iter(): %s", result.status_string());

  // TODO(voydanoff) scatter/gather support
  zx_paddr_t phys;
  size_t size;
  std::tie(phys, size) = *result->at(0).begin();

  size_t trb_count = 1;
  bool needs_zlp = false;
  if (uep.ep.IsInput()) {
    auto& req = std::get<usb::FidlRequest>(pending_req);
    bool short_bit = req->short_().value_or(false);
    if (short_bit && size > 0 && (size % uep.ep.max_packet_size == 0)) {
      needs_zlp = true;
      trb_count = 2;
    }
  }

  if (uep.fifo.AvailableSlots() < trb_count) {
    fdf::warn("Dwc3::UserEpQueueNext ep {} not enough FIFO slots for {}-TRB request", uep.ep.ep_num,
              trb_count);
    return;
  }

  uep.server->current_req.emplace(EpServer::RequestState{
      .request = std::move(pending_req),
      .total_trbs = trb_count,
      .completed_trbs = 0,
      .completed_bytes = 0,
  });

  uep.server->queued_reqs.pop();

  EpStartTransfer(uep.ep, uep.fifo, TRB_TRBCTL_NORMAL, phys, size, needs_zlp);
}

void Dwc3::HandleEpTransferCompleteEvent(uint8_t ep_num) {
  TRACE_DURATION("dwc3", "Dwc3::HandleEpTransferCompleteEvent", "ep_num", ep_num);
  if (is_ep0_num(ep_num)) {
    HandleEp0TransferCompleteEvent(ep_num);
    return;
  }

  UserEndpoint* const uep = get_user_endpoint(ep_num);
  ZX_ASSERT(uep != nullptr);
  if (!uep->server->current_req.has_value()) {
    fdf::error("no usb request found to complete!");
    return;
  }
  auto& current_req = uep->server->current_req.value();
  bool completed = false;
  while (true) {
    dwc3_trb_t trb = uep->fifo.Read();
    if (trb.control & TRB_HWO) {
      break;
    }

    auto& req = std::get<usb::FidlRequest>(current_req.request);
    size_t actual =
        req->data()->size() > current_req.completed_trbs
            ? req->data()->at(current_req.completed_trbs).size().value() - TRB_BUFSIZ(trb.status)
            // If we have more completed TRBs than data regions, it means we
            // have completed a ZLP.
            : 0;
    current_req.completed_trbs++;
    current_req.completed_bytes += actual;
    uep->fifo.AdvanceRead();

    if (current_req.completed_trbs == current_req.total_trbs) {
      completed = true;
      break;
    }
  }

  if (!completed) {
    fdf::error("TRB_HWO still set in dwc3_ep_xfer_complete {}", uep->ep.ep_num);
    return;
  }

  uep->ep.total_transfers++;
  uep->ep.total_bytes += current_req.completed_bytes;

  uep->server->RequestComplete(ZX_OK, current_req.completed_bytes, std::move(current_req.request));
  uep->server->current_req.reset();
  uep->ep.xfer_in_progress = false;
}

void Dwc3::HandleEpTransferNotReadyEvent(uint8_t ep_num, uint32_t stage) {
  TRACE_DURATION("dwc3", "Dwc3::HandleEpTransferNotReadyEvent", "ep_num", ep_num, "stage", stage);
  if (is_ep0_num(ep_num)) {
    HandleEp0TransferNotReadyEvent(ep_num, stage);
    return;
  }

  UserEndpoint* const uep = get_user_endpoint(ep_num);
  ZX_ASSERT(uep != nullptr);
  uep->ep.got_not_ready = true;
  UserEpQueueNext(*uep);
}

void Dwc3::HandleEpTransferStartedEvent(uint8_t ep_num, uint32_t rsrc_id) {
  TRACE_DURATION("dwc3", "Dwc3::HandleEpTransferStartedEvent", "ep_num", ep_num, "rsrc_id",
                 rsrc_id);
  if (is_ep0_num(ep_num)) {
    ((ep_num == kEp0Out) ? ep0_.out : ep0_.in).rsrc_id = rsrc_id;
  } else {
    UserEndpoint* const uep = get_user_endpoint(ep_num);
    ZX_ASSERT(uep != nullptr);
    uep->ep.rsrc_id = rsrc_id;
  }
}

}  // namespace dwc3

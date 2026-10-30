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
  ep.enabled = enable;
  auto* mmio = get_mmio();

  if (enable) {
    DALEPENA::Get().ReadFrom(mmio).EnableEp(ep.ep_num).WriteTo(mmio);
  } else if (power_on_ && ep.transfer_state != Endpoint::TransferState::kPendingCancel &&
             ep.transfer_state != Endpoint::TransferState::kCanceling &&
             ep.stale_starts_to_end == 0 && ep.stale_cancels_to_ignore == 0 &&
             !DEPCMD::Get(ep.ep_num).ReadFrom(mmio).CMDACT()) {
    // Do not clear DALEPENA while a transfer command (DEPSTRTXFER / DEPENDXFER)
    // is active or pending cancel on the endpoint. If transfer_state is
    // kPendingCancel or kCanceling, or a stale DEPSTRTXFER / DEPENDXFER completion
    // is still in flight, HandleEpTransferStartedEvent / HandleEpTransferEndedEvent
    // (or StopEvents()) will clear DALEPENA once the transfer has ended.
    DALEPENA::Get().ReadFrom(mmio).DisableEp(ep.ep_num).WriteTo(mmio);
  }
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

void Dwc3::EpStartSingleTransfer(Endpoint& ep, TrbFifo& fifo, uint32_t type, zx_paddr_t buffer,
                                 size_t length) {
  TRACE_DURATION("dwc3", "Dwc3::EpStartSingleTransfer", "ep_num", ep.ep_num, "type", type, "length",
                 length);
  fdf::debug("Dwc3::EpStartSingleTransfer ep {} type {} length {}", ep.ep_num, type, length);

  dwc3_trb_t* trb = fifo.AdvanceWrite();
  trb->ptr_low = static_cast<uint32_t>(buffer);
  trb->ptr_high = static_cast<uint32_t>(buffer >> 32);
  trb->status = TRB_BUFSIZ(static_cast<uint32_t>(length));

  // std::memory_order_release enforces compiler memory ordering for uncached
  // memory, preventing TRB_HWO from being committed to RAM before the payload.
  std::atomic_thread_fence(std::memory_order_release);
  trb->control = type | TRB_LST | TRB_IOC | TRB_HWO;
  zx_paddr_t trb_phys = fifo.Write(trb, 1);

  ep.queued_trbs++;
  ep.transfer_state = Endpoint::TransferState::kStartingSingle;
  CmdEpStartTransfer(ep, trb_phys);
}

void Dwc3::EpServer::FlushCancelCompleters(zx_status_t status) {
  auto completers = std::move(cancel_completers);
  for (auto& completer : completers) {
    if (status == ZX_OK) {
      completer.Reply(zx::ok());
    } else {
      completer.Reply(zx::error(status));
    }
  }
}

void Dwc3::EpServer::RemoveRequests(zx_status_t reason) {
  TRACE_DURATION("dwc3", "Dwc3::EpServer::RemoveRequests", "ep_num", uep_->ep.ep_num, "reason",
                 reason);
  if (!active_reqs.empty() || !queued_reqs.empty()) {
    if (dwc3_->is_active() && !dwc3_->link_connected_) {
      // Physical unplug: the link is already gone and usb-peripheral has not been told yet, so a
      // busy endpoint is the normal case. End the transfer synchronously and return the requests;
      // the function driver's CancelAll / DisableEndpoint that follow during the DCI drain then
      // find the endpoint idle.
      fdf::debug(
          "Dwc3::EpServer::RemoveRequests ep {}: link gone with endpoint busy (state={}); "
          "ending transfer and returning {} active and {} queued requests",
          uep_->ep.ep_num, uep_->ep.transfer_state, active_reqs.size(), queued_reqs.size());
    } else if (dwc3_->is_active()) {
      // The core is still running with the link up (StopController() or driver stop), so a
      // function driver left this endpoint busy instead of disabling it before the controller was
      // stopped. End the transfer synchronously and return the requests.
      fdf::info(
          "Dwc3::EpServer::RemoveRequests ep {}: endpoint still busy at teardown (state={}); "
          "ending transfer and returning {} active and {} queued requests",
          uep_->ep.ep_num, uep_->ep.transfer_state, active_reqs.size(), queued_reqs.size());
    } else if (dwc3_->disconnect_in_flight_ || dwc3_->stopping_) {
      // Normal disconnect or stop teardown while the core is unpowered or inactive.
      // Log at info rather than warn so expected teardown does not emit spurious warnings.
      fdf::info(
          "Dwc3::EpServer::RemoveRequests ep {}: core inactive during teardown (power_on={}, "
          "controller_started={}, disconnect_in_flight={}, stopping={}, state={}); returning {} active and {} queued requests",
          uep_->ep.ep_num, dwc3_->power_on_, dwc3_->controller_started_,
          dwc3_->disconnect_in_flight_, dwc3_->stopping_, uep_->ep.transfer_state,
          active_reqs.size(), queued_reqs.size());
    } else {
      // Requests were still outstanding when the core stopped or lost power outside of normal
      // teardown: keep this at WARN so an unexpected teardown order remains visible.
      fdf::warn(
          "Dwc3::EpServer::RemoveRequests ep {}: core inactive (power_on={}, "
          "controller_started={}, state={}); returning {} active and {} queued requests",
          uep_->ep.ep_num, dwc3_->power_on_, dwc3_->controller_started_, uep_->ep.transfer_state,
          active_reqs.size(), queued_reqs.size());
    }
  }

  if (dwc3_->is_active()) {
    if (uep_->ep.TransferStateIsActive() && uep_->ep.rsrc_id != Endpoint::kInvalidResourceId) {
      // TransferStateIsActive() is only true for kActiveSingle/kActiveOngoing (it excludes
      // kCanceling, where DEPENDXFER was already issued and DEPCMD.CMDACT was already polled).
      // Halt DMA synchronously without generating a DEPEVT_CMD_CMPLT (CMDIOC=0): the event ring is
      // about to be stopped, so no event could be consumed anyway.
      dwc3_->CmdEpEndTransfer(uep_->ep, /*cmd_ioc=*/false);
    } else if (uep_->ep.transfer_state == Endpoint::TransferState::kStartingSingle ||
               uep_->ep.transfer_state == Endpoint::TransferState::kStartingOngoing ||
               uep_->ep.transfer_state == Endpoint::TransferState::kPendingCancel) {
      // DEPSTRTXFER was issued and its DEPEVT_XFER_STARTED event with rsrc_id is still in flight.
      // Record that the upcoming start event must immediately end that rsrc_id with CMDIOC=0.
      uep_->ep.stale_starts_to_end++;
    } else if (uep_->ep.transfer_state == Endpoint::TransferState::kCanceling) {
      // DEPENDXFER (with CMDIOC=1) was already issued and its DEPEVT_CMD_CMPLT event is still
      // in flight. Record that the upcoming end event must be ignored so it is not mistaken for
      // a subsequent cancellation on this endpoint.
      uep_->ep.stale_cancels_to_ignore++;
    }
  }

  size_t pending_trbs = 0;
  while (!active_reqs.empty()) {
    auto& request_state = active_reqs.front();
    pending_trbs += request_state.total_trbs - request_state.completed_trbs;
    RequestComplete(reason, 0, std::move(request_state.request), /*send_now=*/false);
    active_reqs.pop();
  }
  while (!queued_reqs.empty()) {
    RequestComplete(reason, 0, std::move(queued_reqs.front()), /*send_now=*/false);
    queued_reqs.pop();
  }

  size_t active_count = uep_->fifo.GetActiveCount();
  ZX_DEBUG_ASSERT_MSG(active_count == pending_trbs, "%zu == %zu", active_count, pending_trbs);
  ZX_DEBUG_ASSERT_MSG(uep_->ep.queued_trbs == pending_trbs,
                      "queued_trbs (%zu) != pending_trbs (%zu) on ep %u", uep_->ep.queued_trbs,
                      pending_trbs, uep_->ep.ep_num);

  uep_->fifo.SkipTrbs(pending_trbs);
  uep_->ep.queued_trbs = 0;
  ZX_ASSERT_MSG(uep_->fifo.IsEmpty(),
                "Endpoint %u FIFO leaked %zu active TRBs after RemoveRequests", uep_->ep.ep_num,
                uep_->fifo.GetActiveCount());
  pending_cancel_reason.reset();
  uep_->ep.transfer_state = Endpoint::TransferState::kIdle;
  uep_->ep.rsrc_id = Endpoint::kInvalidResourceId;
  if (!uep_->ep.enabled) {
    dwc3_->EpEnable(uep_->ep, false);
  }
  SendCompletions();
  FlushCancelCompleters(ZX_OK);
  dwc3_->NotifyEndpointIdle(*uep_);
}

void Dwc3::EpServer::CancelAll(zx_status_t reason) {
  TRACE_DURATION("dwc3", "Dwc3::EpServer::CancelAll", "ep_num", uep_->ep.ep_num, "reason", reason);
  fdf::debug(
      "Dwc3::EpServer::CancelAll ep {} reason {} reqs = ({}, {}), controller={}, power_on={}",
      uep_->ep.ep_num, zx_status_get_string(reason), active_reqs.size(), queued_reqs.size(),
      dwc3_->controller_started_, dwc3_->power_on_);

  if (!dwc3_->is_active()) {
    // With the core stopped or unpowered no transfer can be ended and no event can arrive; just
    // return the requests.
    RemoveRequests(reason);
    return;
  }

  // Any request that hasn't been enqueued yet is immediately returned.
  for (; !queued_reqs.empty(); queued_reqs.pop()) {
    RequestComplete(reason, 0, std::move(queued_reqs.front()), /*send_now=*/false);
  }
  SendCompletions();

  switch (uep_->ep.transfer_state) {
    case Endpoint::TransferState::kIdle:
    case Endpoint::TransferState::kCanceling:
    case Endpoint::TransferState::kPendingCancel:
      return;
    case Endpoint::TransferState::kActiveOngoing:
    case Endpoint::TransferState::kActiveSingle:
      pending_cancel_reason = reason;
      uep_->ep.transfer_state = Endpoint::TransferState::kCanceling;
      if (!dwc3_->CmdEpEndTransfer(uep_->ep)) {
        fdf::error(
            "Dwc3::EpServer::CancelAll ep {}: CmdEpEndTransfer failed; returning {} active and {} queued requests",
            uep_->ep.ep_num, active_reqs.size(), queued_reqs.size());
        // The command never completed, so no DEPEVT_CMD_CMPLT is owed: leave kCanceling before
        // RemoveRequests() so it does not record a stale cancel that would swallow the next real
        // cancel completion on this endpoint.
        uep_->ep.transfer_state = Endpoint::TransferState::kIdle;
        RemoveRequests(reason);
        return;
      }
      break;
    case Endpoint::TransferState::kStartingSingle:
    case Endpoint::TransferState::kStartingOngoing:
      // We're waiting for a start transfer to be emitted, record the
      // cancelation reason and issue end transfer.
      pending_cancel_reason = reason;
      uep_->ep.transfer_state = Endpoint::TransferState::kPendingCancel;
      break;
  }
}

void Dwc3::UserEpQueueNext(UserEndpoint& uep) {
  TRACE_DURATION("dwc3", "Dwc3::UserEpQueueNext", "ep_num", uep.ep.ep_num);
  if (!uep.ep.enabled || !uep.ep.got_not_ready || uep.server->queued_reqs.empty() ||
      uep.ep.stalled) {
    return;
  }

  bool start_transfer;
  switch (uep.ep.transfer_state) {
    case Endpoint::TransferState::kIdle:
      start_transfer = true;
      break;
    case Endpoint::TransferState::kCanceling:
    case Endpoint::TransferState::kActiveSingle:
    case Endpoint::TransferState::kStartingSingle:
    case Endpoint::TransferState::kStartingOngoing:
    case Endpoint::TransferState::kPendingCancel:
      return;
    case Endpoint::TransferState::kActiveOngoing:
      start_transfer = false;
      break;
  }

  const bool enable_ongoing_transfer = AllowEnqueueManyTRBs(uep.ep.type);
  if (!enable_ongoing_transfer) {
    if (!start_transfer) {
      return;
    }
    UserEpQueueNextSingle(uep);
  } else {
    UserEpQueueNextOngoing(uep, start_transfer);
  }
}

Dwc3::QueueRequestResult Dwc3::QueueNextRequest(UserEndpoint& uep, bool is_single) {
  if (uep.server->queued_reqs.empty()) {
    return fit::error(QueueRequestError::kEmpty);
  }

  usb::RequestVariant& pending_req = uep.server->queued_reqs.front();

  zx::result result = uep.server->get_iter(pending_req, TRB_BUFSIZ(~0u));
  if (result.is_error()) {
    fdf::error("get_iter failed: {}", result.status_string());
    usb::RequestVariant req = std::move(pending_req);
    uep.server->queued_reqs.pop();
    uep.server->RequestComplete(result.status_value(), 0, std::move(req));
    return fit::error(QueueRequestError::kError);
  }

  size_t total_size = 0;
  size_t data_trbs = 0;
  for (dma_buffer::PhysIter& phys_iter : *result) {
    for (const auto& [paddr, size] : phys_iter) {
      total_size += size;
      data_trbs++;
    }
  }

  const usb::FidlRequest& req = std::get<usb::FidlRequest>(pending_req);
  bool needs_zlp = false;
  if (uep.ep.IsInput()) {
    bool short_bit = req->short_().value_or(false);
    if (short_bit && total_size > 0 && (total_size % uep.ep.max_packet_size == 0)) {
      needs_zlp = true;
    }
  }

  const size_t total_trbs = needs_zlp ? data_trbs + 1 : std::max(data_trbs, 1ul);
  // TODO(https://fxbug.dev/572870946): Fail if we can't ever have enough TRB
  // entries for the request for now. It may be possible to split this single
  // request into different transfers with the controller, if we take care to
  // handle MPS alignment and short packets.
  if (total_trbs > uep.fifo.MaxAvailableSlots()) {
    fdf::error("Dwc3::QueueNextRequest ep {} request requires {} TRBs, exceeding max capacity {}",
               uep.ep.ep_num, total_trbs, uep.fifo.MaxAvailableSlots());
    usb::RequestVariant req_complete = std::move(pending_req);
    uep.server->queued_reqs.pop();
    uep.server->RequestComplete(ZX_ERR_OUT_OF_RANGE, 0, std::move(req_complete));
    return fit::error(QueueRequestError::kError);
  }

  if (total_trbs > uep.fifo.AvailableSlots()) {
    fdf::debug("Dwc3::UserEpQueueNext ep {} not enough FIFO slots for request", uep.ep.ep_num);
    return fit::error(QueueRequestError::kNotEnoughSpace);
  }

  dwc3_trb_t* first_trb = nullptr;
  dwc3_trb_t* second_trb = nullptr;
  uint32_t first_trb_control = 0;
  size_t trb_idx = 0;

  auto record_trb = [&](dwc3_trb_t* trb, uint32_t control) {
    if (trb_idx == 0) {
      first_trb = trb;
      first_trb_control = control;
    } else {
      if (trb_idx == 1) {
        second_trb = trb;
      }
      trb->control = control;
    }
    trb_idx++;
  };

  for (dma_buffer::PhysIter& phys_iter : *result) {
    for (const auto& [paddr, size] : phys_iter) {
      dwc3_trb_t* trb = uep.fifo.AdvanceWrite(static_cast<uint32_t>(size));
      trb->ptr_low = static_cast<uint32_t>(paddr);
      trb->ptr_high = static_cast<uint32_t>(paddr >> 32);
      trb->status = TRB_BUFSIZ(static_cast<uint32_t>(size));

      uint32_t control = TRB_TRBCTL_NORMAL | TRB_HWO;
      if (uep.ep.IsOutput() && !is_single) {
        control |= TRB_CSP;
      }
      if (trb_idx + 1 < data_trbs) {
        control |= TRB_CHN;
      } else {
        control |= TRB_IOC;
        if (is_single && !needs_zlp) {
          control |= TRB_LST;
        }
      }
      record_trb(trb, control);
    }
  }

  // NOTE: The DWC3 programming manual states support for a `Normal-ZLP`
  // transfer type that should be able to handle this. Alas, it doesn't actually
  // seem to work, so we just enqueue an actual zero length transfer as a second
  // TRB.
  //
  // Because the data TRB and ZLP TRB are submitted as separate, unchained
  // Transfer Descriptors (TDs), both must have TRB_IOC set so the driver
  // receives completion events to finish the request.
  if (data_trbs == 0 || needs_zlp) {
    dwc3_trb_t* zlp_trb = uep.fifo.AdvanceWrite(0);
    zlp_trb->ptr_low = 0;
    zlp_trb->ptr_high = 0;
    zlp_trb->status = TRB_BUFSIZ(0);

    uint32_t control = TRB_TRBCTL_NORMAL | TRB_IOC | TRB_HWO;
    if (is_single) {
      control |= TRB_LST;
    }
    record_trb(zlp_trb, control);
  }

  // Flush all TRBs except the first to RAM before committing HWO on the first
  // TRB.
  if (total_trbs > 1) {
    uep.fifo.Write(second_trb, total_trbs - 1);
  }

  // Safely grant hardware ownership to the primary TRB spanning the release
  // fence.
  std::atomic_thread_fence(std::memory_order_release);
  first_trb->control = first_trb_control;
  zx_paddr_t first_trb_phys = uep.fifo.Write(first_trb, 1);

  uep.ep.queued_trbs += total_trbs;
  uep.server->active_reqs.push(EpServer::RequestState{
      .request = std::move(pending_req),
      .total_trbs = total_trbs,
      .completed_trbs = 0,
      .completed_bytes = 0,
  });
  uep.server->queued_reqs.pop();

  return fit::ok(first_trb_phys);
}

void Dwc3::UserEpQueueNextSingle(UserEndpoint& uep) {
  while (true) {
    fit::result result = QueueNextRequest(uep, /*is_single=*/true);
    if (result.is_ok()) {
      uep.ep.transfer_state = Endpoint::TransferState::kStartingSingle;
      CmdEpStartTransfer(uep.ep, result.value());
      return;
    }
    switch (result.error_value()) {
      case QueueRequestError::kEmpty:
      case QueueRequestError::kNotEnoughSpace:
        return;
      case QueueRequestError::kError:
        break;
    }
  }
}

void Dwc3::UserEpQueueNextOngoing(UserEndpoint& uep, bool start_transfer) {
  // For endpoints that allow continuous transfers with an ongoing TRB ring, the
  // strategy is to issue the start transfer command *once* and then only issue
  // the update-transfer command.
  //
  // Note that a "transfer" is not mapped to a specific set of USB requests.
  // "transfer" means an ongoing session with an active TRB FIFO with the
  // controller. The programming manual refers to this as a transfer so we
  // maintain the jargon.
  //
  // This allows us to operate on the TRBs in parallel with the controller, only
  // kicking it when new requests are enqueue for it to look at the head of the
  // TRB ring.
  //
  // This strategy greatly improves performance, even when still getting
  // interrupts for every transfer, because the transfer only stops/stalls when
  // endpoint clients run out of requests to send down, as opposed to at every
  // request upon ending the transfer.

  if (start_transfer) {
    ZX_DEBUG_ASSERT(uep.fifo.IsEmpty());
  } else if (uep.ep.rsrc_id == Endpoint::kInvalidResourceId) {
    // We haven't received a resource ID yet from a started transfer. Can't
    // proceed.
    fdf::debug("Dwc3::UserEpQueueNext ep {} waiting for transfer start", uep.ep.ep_num);
    return;
  }

  zx_paddr_t first_trb_phys = 0;
  size_t enqueued = 0;
  bool process = true;
  while (process) {
    fit::result result = QueueNextRequest(uep, /*is_single=*/false);
    if (result.is_ok()) {
      if (first_trb_phys == 0) {
        first_trb_phys = result.value();
      }
      enqueued++;
      continue;
    }
    switch (result.error_value()) {
      case QueueRequestError::kEmpty:
      case QueueRequestError::kNotEnoughSpace:
        process = false;
        break;
      case QueueRequestError::kError:
        break;
    }
  }

  if (enqueued == 0) {
    return;
  }

  if (start_transfer) {
    fdf::debug("Dwc3::UserEpQueueNext ep {} starting transfer {}/{}", uep.ep.ep_num, enqueued,
               uep.server->active_reqs.size());
    CmdEpStartTransfer(uep.ep, first_trb_phys);
    uep.ep.transfer_state = Endpoint::TransferState::kStartingOngoing;
  } else {
    CmdEpUpdateTransfer(uep.ep);
  }
}

void Dwc3::HandleEpTransferCompleteEvent(uint8_t ep_num) {
  TRACE_DURATION("dwc3", "Dwc3::HandleEpTransferCompleteEvent", "ep_num", ep_num);
  if (is_ep0_num(ep_num)) {
    HandleEp0TransferCompleteEvent(ep_num);
    return;
  }

  UserEndpoint* const uep = get_user_endpoint(ep_num);
  ZX_ASSERT(uep != nullptr);
  UserEpCompleteTransfers(*uep);
  if (uep->ep.transfer_state == Endpoint::TransferState::kCanceling ||
      uep->ep.transfer_state == Endpoint::TransferState::kPendingCancel) {
    return;
  }
  uep->ep.transfer_state = Endpoint::TransferState::kIdle;
  uep->ep.rsrc_id = Endpoint::kInvalidResourceId;
  NotifyEndpointIdle(*uep);
  UserEpQueueNext(*uep);
}

void Dwc3::HandleEpTransferInProgressEvent(uint8_t ep_num) {
  TRACE_DURATION("dwc3", "Dwc3::HandleEpTransferInProgressEvent", "ep_num", ep_num);
  if (is_ep0_num(ep_num)) {
    return;
  }
  UserEndpoint* const uep = get_user_endpoint(ep_num);
  ZX_ASSERT(uep != nullptr);
  UserEpCompleteTransfers(*uep);
  UserEpQueueNext(*uep);
}

void Dwc3::UserEpCompleteTransfers(UserEndpoint& uep) {
  if (uep.server->active_reqs.empty()) {
    return;
  }

  while (!uep.server->active_reqs.empty()) {
    zx::result trb_res = uep.fifo.ReadOne();
    if (trb_res.is_error()) {
      fdf::error("ReadOne failed: {}", trb_res.status_string());
      break;
    }
    const dwc3_trb_t& trb = *trb_res;

    if (trb.control & TRB_HWO) {
      // not yet completed by the controller
      break;
    }

    EpServer::RequestState& current_req = uep.server->active_reqs.front();
    usb::FidlRequest& req = std::get<usb::FidlRequest>(current_req.request);
    ZX_ASSERT(current_req.completed_trbs < current_req.total_trbs);
    uint32_t trb_size = uep.fifo.current_read_size();
    uint32_t remaining = TRB_BUFSIZ(trb.status);
    size_t actual = trb_size > remaining ? trb_size - remaining : 0;
    current_req.completed_trbs++;
    ZX_DEBUG_ASSERT(uep.ep.queued_trbs > 0);
    uep.ep.queued_trbs--;
    current_req.completed_bytes += actual;
    uep.fifo.AdvanceRead();

    if (uep.ep.IsOutput() && (remaining > 0 || (trb.status & TRB_SPR) != 0)) {
      while (current_req.completed_trbs < current_req.total_trbs) {
        ZX_ASSERT_MSG(!uep.fifo.IsEmpty(),
                      "Software/Hardware desync: expected %zu TRBs, found empty FIFO at %zu",
                      current_req.total_trbs, current_req.completed_trbs);
        uep.fifo.SkipTrbs(1);
        ZX_DEBUG_ASSERT(uep.ep.queued_trbs > 0);
        uep.ep.queued_trbs--;
        current_req.completed_trbs++;
      }
    }

    if (current_req.completed_trbs < current_req.total_trbs) {
      continue;
    }

    uep.ep.total_transfers++;
    uep.ep.total_bytes += current_req.completed_bytes;
    uep.server->RequestComplete(ZX_OK, current_req.completed_bytes, std::move(req),
                                /*send_now=*/false);
    uep.server->active_reqs.pop();

    // Complete at most one request per transfer event. Every request ends in a
    // TRB_IOC TRB, and an IN request that needs a ZLP has a second TRB_IOC on the
    // trailing ZLP TRB, so each request raises at least one event. Stopping after
    // one request prevents racing ahead into a subsequent multi-TRB Bulk OUT
    // request whose first TRB has TRB_HWO=0 due to a short packet while the
    // controller is still skipping the rest of its chained TRBs without clearing
    // their TRB_HWO bit. An extra event from an IN ZLP may complete the next IN
    // request early, which is harmless because IN requests never skip TRBs.
    // TODO(https://fxbug.dev/579572059): Make IN+ZLP raise a single event and
    // drop this caveat.
    break;
  }
}

void Dwc3::HandleEpTransferNotReadyEvent(uint8_t ep_num, uint32_t stage) {
  TRACE_DURATION("dwc3", "Dwc3::HandleEpTransferNotReadyEvent", "ep_num", ep_num, "stage", stage);
  if (is_ep0_num(ep_num)) {
    HandleEp0TransferNotReadyEvent(ep_num, stage);
    return;
  }

  UserEndpoint* const uep = get_user_endpoint(ep_num);
  ZX_ASSERT(uep != nullptr);
  if (!uep->ep.enabled || uep->ep.transfer_state == Endpoint::TransferState::kPendingCancel ||
      uep->ep.transfer_state == Endpoint::TransferState::kCanceling) {
    return;
  }
  uep->ep.got_not_ready = true;
  UserEpQueueNext(*uep);
}

void Dwc3::HandleEpTransferStartedEvent(uint8_t ep_num, uint32_t rsrc_id) {
  TRACE_DURATION("dwc3", "Dwc3::HandleEpTransferStartedEvent", "ep_num", ep_num, "rsrc_id",
                 rsrc_id);
  if (is_ep0_num(ep_num)) {
    Endpoint& ep0_dir = (ep_num == kEp0Out) ? ep0_.out : ep0_.in;
    if (ep0_dir.stale_starts_to_end > 0) {
      ep0_dir.stale_starts_to_end--;
      Endpoint stale_ep(ep0_dir.ep_num);
      stale_ep.rsrc_id = rsrc_id;
      CmdEpEndTransfer(stale_ep, /*cmd_ioc=*/false);
      ep0_.shared_fifo.SkipTrbs(1);
      ZX_DEBUG_ASSERT(ep0_dir.queued_trbs > 0);
      ep0_dir.queued_trbs--;
      if (ep0_dir.stale_starts_to_end == 0 && !ep0_dir.enabled) {
        EpEnable(ep0_dir, false);
      }
      if (ep0_.out.stale_starts_to_end == 0 && ep0_.in.stale_starts_to_end == 0) {
        for (Endpoint* ep0 : {&ep0_.out, &ep0_.in}) {
          const size_t live_trbs = (ep0->transfer_state == Endpoint::TransferState::kIdle) ? 0 : 1;
          ZX_DEBUG_ASSERT(ep0->queued_trbs >= live_trbs);
          ep0_.shared_fifo.SkipTrbs(ep0->queued_trbs - live_trbs);
          ep0->queued_trbs = live_trbs;
        }
        if (ep0_.out.enabled && ep0_.state == Ep0::State::Setup &&
            ep0_.out.transfer_state == Endpoint::TransferState::kIdle) {
          Ep0QueueSetup();
        }
      }
      return;
    }
    if (ep0_dir.transfer_state != Endpoint::TransferState::kStartingSingle) {
      Endpoint stale_ep(ep0_dir.ep_num);
      stale_ep.rsrc_id = rsrc_id;
      CmdEpEndTransfer(stale_ep, /*cmd_ioc=*/false);
      return;
    }
    ep0_dir.rsrc_id = rsrc_id;
    ep0_dir.transfer_state = Endpoint::TransferState::kActiveSingle;
    return;
  }

  UserEndpoint* const uep = get_user_endpoint(ep_num);
  ZX_ASSERT(uep != nullptr);
  if (uep->ep.stale_starts_to_end > 0) {
    uep->ep.stale_starts_to_end--;
    Endpoint stale_ep(uep->ep.ep_num);
    stale_ep.rsrc_id = rsrc_id;
    CmdEpEndTransfer(stale_ep, /*cmd_ioc=*/false);
    if (uep->ep.stale_starts_to_end == 0 && !uep->ep.enabled) {
      EpEnable(uep->ep, false);
    }
    NotifyEndpointIdle(*uep);
    return;
  }
  uep->ep.rsrc_id = rsrc_id;
  switch (uep->ep.transfer_state) {
    case Endpoint::TransferState::kIdle:
    case Endpoint::TransferState::kCanceling:
    case Endpoint::TransferState::kActiveOngoing:
    case Endpoint::TransferState::kActiveSingle:
      fdf::warn("Dwc3::HandleEpTransferStartedEvent ep {} in unexpected state {}", ep_num,
                uep->ep.transfer_state);
      break;
    case Endpoint::TransferState::kStartingSingle:
      uep->ep.transfer_state = Endpoint::TransferState::kActiveSingle;
      break;
    case Endpoint::TransferState::kStartingOngoing:
      uep->ep.transfer_state = Endpoint::TransferState::kActiveOngoing;
      break;
    case Endpoint::TransferState::kPendingCancel:
      // We've been requested to end the transfer.
      uep->ep.transfer_state = Endpoint::TransferState::kCanceling;
      if (!CmdEpEndTransfer(uep->ep) && uep->server.has_value()) {
        fdf::error(
            "Dwc3::HandleEpTransferStartedEvent ep {}: CmdEpEndTransfer failed; returning {} active and {} queued requests",
            ep_num, uep->server->active_reqs.size(), uep->server->queued_reqs.size());
        // As in CancelAll(): the failed command owes no completion, so do not record a stale
        // cancel for it.
        uep->ep.transfer_state = Endpoint::TransferState::kIdle;
        uep->server->RemoveRequests(uep->server->pending_cancel_reason.value_or(ZX_ERR_CANCELED));
        return;
      }
      break;
  }
  // Attempt to enqueue more things now that we have a resource ID. States that
  // can't enqueue any more are handled within.
  UserEpQueueNext(*uep);
}

void Dwc3::HandleEpTransferEndedEvent(uint8_t ep_num) {
  TRACE_DURATION("dwc3", "Dwc3::HandleEpTransferEndedEvent", "ep_num", ep_num);
  if (is_ep0_num(ep_num)) {
    return;
  }

  UserEndpoint* const uep = get_user_endpoint(ep_num);
  ZX_ASSERT(uep != nullptr);
  if (uep->ep.stale_cancels_to_ignore > 0) {
    uep->ep.stale_cancels_to_ignore--;
    fdf::info(
        "Dwc3::HandleEpTransferEndedEvent ep {} ignoring expected stale cancel event (remaining={})",
        ep_num, uep->ep.stale_cancels_to_ignore);
    if (uep->ep.stale_cancels_to_ignore == 0 && !uep->ep.enabled) {
      EpEnable(uep->ep, false);
    }
    return;
  }
  if (uep->ep.transfer_state != Endpoint::TransferState::kCanceling) {
    fdf::error("Dwc3::HandleEpTransferEndedEvent ep {} ignoring unexpected event in state {}",
               ep_num, uep->ep.transfer_state);
    return;
  }
  fdf::debug("Dwc3::HandleEpTransferEndedEvent ep {}", ep_num);

  size_t pending_trbs = 0;
  if (uep->server) {
    // Reason may not be set if the endpoint is reset/halted from under us, fallback to
    // IO_REFUSED.
    zx_status_t reason = uep->server->pending_cancel_reason.value_or(ZX_ERR_IO_REFUSED);
    uep->server->pending_cancel_reason.reset();
    while (!uep->server->active_reqs.empty()) {
      auto& request_state = uep->server->active_reqs.front();
      pending_trbs += request_state.total_trbs - request_state.completed_trbs;
      uep->server->RequestComplete(reason, 0, std::move(request_state.request), /*send_now=*/false);
      uep->server->active_reqs.pop();
    }
    uep->server->SendCompletions();
    size_t active_count = uep->fifo.GetActiveCount();
    ZX_ASSERT_MSG(active_count == pending_trbs, "%ld == %ld", active_count, pending_trbs);
    ZX_DEBUG_ASSERT_MSG(uep->ep.queued_trbs == pending_trbs,
                        "queued_trbs (%zu) != pending_trbs (%zu) on ep %u", uep->ep.queued_trbs,
                        pending_trbs, uep->ep.ep_num);

    uep->server->FlushCancelCompleters(ZX_OK);
  }
  uep->fifo.SkipTrbs(pending_trbs);
  uep->ep.queued_trbs = 0;
  ZX_ASSERT_MSG(uep->fifo.IsEmpty(),
                "Endpoint %u FIFO leaked %zu active TRBs after HandleEpTransferEndedEvent", ep_num,
                uep->fifo.GetActiveCount());
  uep->ep.transfer_state = Endpoint::TransferState::kIdle;
  uep->ep.rsrc_id = Endpoint::kInvalidResourceId;
  if (!uep->ep.enabled) {
    EpEnable(uep->ep, false);
  }
  NotifyEndpointIdle(*uep);
  UserEpQueueNext(*uep);
}

}  // namespace dwc3

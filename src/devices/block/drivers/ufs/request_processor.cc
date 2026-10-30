// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "request_processor.h"

#include <lib/driver/logging/cpp/logger.h>

#include <bit>
#include <chrono>

#include "src/devices/block/drivers/ufs/ufs.h"

namespace ufs {

namespace {

// Releases a slot's DMA buffer. Without |unpin| the PMT handle is closed without zx_pmt_unpin(),
// which leaves the pages quarantined on the BTI; see RequestProcessor::AbortInflightRequests().
zx_status_t ReleasePmt(zx::pmt &pmt, bool unpin) {
  if (!pmt.is_valid()) {
    return ZX_OK;
  }
  if (!unpin) {
    pmt.reset();
    return ZX_OK;
  }
  return pmt.unpin();
}

}  // namespace

template <typename Processor, typename Descriptor>
zx::result<std::unique_ptr<Processor>> RequestProcessor::Create(Ufs &ufs, zx::unowned_bti bti,
                                                                const fdf::MmioView mmio,
                                                                uint8_t entry_count) {
  zx::result<RequestList> request_list =
      RequestList::Create(bti->borrow(), sizeof(Descriptor), entry_count);
  if (request_list.is_error()) {
    return request_list.take_error();
  }

  fbl::AllocChecker ac;
  auto request_processor = fbl::make_unique_checked<Processor>(
      &ac, std::move(request_list.value()), ufs, std::move(bti), mmio, entry_count);
  if (!ac.check()) {
    fdf::error("Failed to allocate request processor.");
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  return zx::ok(std::move(request_processor));
}

template zx::result<std::unique_ptr<TransferRequestProcessor>>
RequestProcessor::Create<TransferRequestProcessor, TransferRequestDescriptor>(
    Ufs &ufs, zx::unowned_bti bti, const fdf::MmioView mmio, uint8_t entry_count);

template zx::result<std::unique_ptr<TaskManagementRequestProcessor>>
RequestProcessor::Create<TaskManagementRequestProcessor, TaskManagementRequestDescriptor>(
    Ufs &ufs, zx::unowned_bti bti, const fdf::MmioView mmio, uint8_t entry_count);

void RequestProcessor::ReclaimTimedOutSlotsLocked() {
  std::optional<uint32_t> doorbell;
  ForEachAllocatedSlotLocked(
      [this, &doorbell](uint8_t slot_num, RequestSlot &slot) TA_REQ(slot_lock_) {
        if (slot.state != SlotState::kTimeout) {
          return;
        }
        if (!doorbell.has_value()) {
          // A timed-out slot is still allocated and so holds the link active.
          doorbell = ReadDoorBellRegister();
        }
        if ((*doorbell & (1u << slot_num)) == 0) {
          if (zx::result<> result = ClearSlotLocked(slot); result.is_error()) {
            fdf::error("Failed to clear timed out slot[{}]: {}", slot_num, result);
          }
        }
      });
}

zx::result<uint8_t> RequestProcessor::ReserveSlot() {
  std::lock_guard<std::mutex> lock(slot_lock_);
  if (slot_allocation_blocked_) {
    // Checked before ReclaimTimedOutSlotsLocked(), which reads the doorbell register: the
    // controller is about to be, or already is, powered down.
    return zx::error(ZX_ERR_BAD_STATE);
  }
  ReclaimTimedOutSlotsLocked();

  std::optional<uint8_t> admin_slot_num = GetAdminCommandSlotNumber();
  uint32_t excluded_mask = admin_slot_num.has_value() ? (1u << *admin_slot_num) : 0;

  uint32_t free_slots_mask = ~allocated_slots_mask_ & slots_.GetSlotMask() & ~excluded_mask;
  if (free_slots_mask != 0) {
    const uint8_t slot_num = static_cast<uint8_t>(std::countr_zero(free_slots_mask));
    allocated_slots_mask_ |= (1u << slot_num);
    slots_.GetSlot(slot_num).Reset(SlotState::kReserved);
    // An allocated slot holds the link active until ClearSlotLocked() releases it (slots cannot
    // carry a ScopedLinkActive guard: RequestSlot lives in request_list.h and has no Ufs
    // reference), so the pairing is explicit here and in ClearSlotLocked().
    controller_.AcquireLinkActive();
    return zx::ok(slot_num);
  }
  fdf::debug("Failed to reserve a request slot");
  return zx::error(ZX_ERR_NO_RESOURCES);
}

zx::result<> RequestProcessor::ClearSlotLocked(RequestSlot &request_slot, bool unpin) {
  if (request_slot.pmt.is_valid()) {
    zx_status_t status = ReleasePmt(request_slot.pmt, unpin);
    if (status != ZX_OK) {
      fdf::error("Failed to unpin IO buffer: {}", zx_status_get_string(status));
      request_slot.result = status;
      return zx::error(status);
    }
  }

  uint8_t slot_num = slots_.GetSlotNum(request_slot);
  request_slot.Reset(SlotState::kFree);
  const bool was_allocated = (allocated_slots_mask_ & (1u << slot_num)) != 0;
  allocated_slots_mask_ &= ~(1u << slot_num);
  if (was_allocated) {
    // BlockSlotAllocation() waits for allocated_slots_mask_ to drain, so only an actual release
    // can make progress for it; clearing an already-free slot would be a spurious wakeup.
    if (slot_allocation_blocked_) {
      slot_released_.notify_all();
    }
    // Pairs with the AcquireLinkActive() in ReserveSlot() / ReserveAdminSlot(). Keyed on the
    // allocated bit (not on the call) so that clearing a slot twice cannot under-count.
    controller_.ReleaseLinkActive();
  }
  return zx::ok();
}

zx::result<> RequestProcessor::ClearSlot(RequestSlot &request_slot) {
  std::lock_guard<std::mutex> lock(slot_lock_);
  return ClearSlotLocked(request_slot);
}

zx::result<> RequestProcessor::BlockSlotAllocation(zx::duration drain_timeout) {
  std::lock_guard<std::mutex> lock(slot_lock_);
  slot_allocation_blocked_ = true;

  std::optional<uint8_t> admin_slot_num = GetAdminCommandSlotNumber();
  const uint32_t admin_mask = admin_slot_num.has_value() ? (1u << *admin_slot_num) : 0;
  const auto deadline =
      std::chrono::steady_clock::now() + std::chrono::nanoseconds(drain_timeout.to_nsecs());
  const auto pending = [&]() TA_REQ(slot_lock_) {
    return (allocated_slots_mask_ & ~admin_mask) != 0;
  };
  // Slots whose request timed out are only reclaimed from ReserveSlot(), which the gate above now
  // short-circuits, so do it here: once (while the controller is still enabled) before waiting,
  // and again before giving up, so that a request that timed out earlier does not hold the drain
  // for the full timeout. The in-flight slots are released on the worker dispatchers as their
  // requests complete, so the wait drops slot_lock_ while blocked (condition_variable_any
  // unlocks/relocks the mutex itself); the condition is re-checked under the lock after every
  // wakeup.
  ReclaimTimedOutSlotsLocked();
  while (pending()) {
    if (slot_released_.wait_until(slot_lock_, deadline) == std::cv_status::timeout) {
      ReclaimTimedOutSlotsLocked();
      if (pending()) {
        fdf::error("Timed out waiting for {} in-flight request(s) to complete",
                   std::popcount(allocated_slots_mask_ & ~admin_mask));
        return zx::error(ZX_ERR_TIMED_OUT);
      }
    }
  }
  return zx::ok();
}

void RequestProcessor::SetSlotAllocationBlocked(bool blocked) {
  std::scoped_lock lock(slot_lock_);
  slot_allocation_blocked_ = blocked;
}

void RequestProcessor::UnblockSlotAllocation() { SetSlotAllocationBlocked(false); }

bool RequestProcessor::HasAllocatedSlots() const {
  std::scoped_lock lock(slot_lock_);
  return allocated_slots_mask_ != 0;
}

void RequestProcessor::AbortInflightRequests(zx_status_t abort_status, bool unpin) {
  std::vector<fit::callback<void(zx_status_t)>> callbacks;
  {
    std::scoped_lock lock(slot_lock_);
    uint32_t mask = allocated_slots_mask_;
    while (mask != 0) {
      const uint8_t slot_num = static_cast<uint8_t>(std::countr_zero(mask));
      mask &= mask - 1;
      RequestSlot &slot = slots_.GetSlot(slot_num);
      if (slot.completion_cb) {
        callbacks.push_back(std::move(slot.completion_cb));
        if (zx::result<> res = ClearSlotLocked(slot, unpin); res.is_error()) {
          fdf::error("Failed to clear aborted slot[{}]: {}", slot_num, res);
        }
      } else if (slot.state == SlotState::kScheduled) {
        [[maybe_unused]] zx_status_t status = ReleasePmt(slot.pmt, unpin);
        slot.result = abort_status;
        sync_completion_signal(&slot.complete);
      } else {
        if (zx::result<> res = ClearSlotLocked(slot, unpin); res.is_error()) {
          fdf::error("Failed to clear aborted slot[{}]: {}", slot_num, res);
        }
      }
    }
  }
  for (auto &cb : callbacks) {
    if (cb) {
      cb(abort_status);
    }
  }
}

void RequestProcessor::RingRequestDoorbellLocked(uint8_t slot_num) {
  RequestSlot &request_slot = slots_.GetSlot(slot_num);
  sync_completion_t *complete = &request_slot.complete;
  sync_completion_reset(complete);
  request_slot.deadline = zx_deadline_after(GetTimeout().get());
  ZX_DEBUG_ASSERT(request_slot.state == SlotState::kReserved);
  request_slot.state = SlotState::kScheduled;
  SetDoorBellRegister(slot_num);

  // TODO(https://fxbug.dev/42075643): Set the UtrInterruptAggregationControlReg.
}

void RequestProcessor::RingRequestDoorbell(uint8_t slot_num) {
  std::lock_guard<std::mutex> lock(slot_lock_);
  RingRequestDoorbellLocked(slot_num);
}

}  // namespace ufs

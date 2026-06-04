// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "lib/power-management/port-power-level-controller.h"

#include <lib/power-management/energy-model.h>
#include <lib/power-management/power-state.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
// TODO(https://fxbug.dev/415033686): Stop using `syscalls-next.h` on host.
#define FUCHSIA_UNSUPPORTED_ALLOW_SYSCALLS_NEXT_ON_HOST
#include <zircon/syscalls-next.h>
#undef FUCHSIA_UNSUPPORTED_ALLOW_SYSCALLS_NEXT_ON_HOST
#include <zircon/syscalls/port.h>
#include <zircon/types.h>

#include <fbl/alloc_checker.h>
#include <object/port_dispatcher.h>

#include "kernel/spinlock.h"

namespace power_management {

zx::result<fbl::RefPtr<PortPowerLevelController>> PortPowerLevelController::Create(
    fbl::RefPtr<PortDispatcher> dispatcher) {
  fbl::AllocChecker ac;
  fbl::RefPtr<PacketQueue> queue = fbl::MakeRefCountedChecked<PacketQueue>(&ac, dispatcher);
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  fbl::RefPtr<PortPowerLevelController> controller =
      fbl::MakeRefCountedChecked<PortPowerLevelController>(&ac, PrivateConstructorValue,
                                                           std::move(dispatcher), std::move(queue));
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  return zx::ok(std::move(controller));
}

PortPowerLevelController::~PortPowerLevelController() { packet_queue_->CancelAll(); }

void PortPowerLevelController::PacketQueue::Queue(const zx_port_packet_t& packet) {
  PortPacket* current_packet = nullptr;
  {
    Guard<SpinLock, IrqSave> guard(&packet_lock_);
    current_packet = &packets_[current_ % 2];
    current_packet->packet = packet;
    if (packet_queued_) {
      packet_pending_ = true;
      return;
    }

    current_++;
    packet_queued_ = true;
    packet_pending_ = false;

    // We successfully queued a packet, keep PacketQueue alive.
    in_flight_ref_ = fbl::RefPtr<PacketQueue>(this);
  }
  // We must release packet_lock_ before calling Queue() to avoid lock inversion
  // with the PortDispatcher mutex. This is safe because packet_queued_ is true,
  // ensuring any concurrent updates are stashed in the other packet.
  //
  // It is not possible to hit ZX_ERR_SHOULD_WAIT since none of these packets
  // are allocated with the port's default allocator.
  const zx_status_t status = port_->Queue(current_packet);
  ZX_ASSERT(status != ZX_ERR_SHOULD_WAIT);
  if (status != ZX_OK) {
    fbl::RefPtr<PacketQueue> release_ref;
    {
      Guard<SpinLock, IrqSave> guard(&packet_lock_);
      packet_queued_ = false;
      packet_pending_ = false;
      release_ref = std::move(in_flight_ref_);
    }
  }
}

void PortPowerLevelController::PacketQueue::Free(PortPacket* packet) {
  PortPacket* current_packet = nullptr;
  bool queue_new = false;
  fbl::RefPtr<PacketQueue> release_ref;

  {
    Guard<SpinLock, IrqSave> guard(&packet_lock_);
    ZX_DEBUG_ASSERT(packet_queued_);
    ZX_DEBUG_ASSERT(packet == &packets_[(current_ + 1) % 2]);

    if (!packet_pending_) {
      packet_queued_ = false;
      release_ref = std::move(in_flight_ref_);
    } else {
      packet_pending_ = false;
      current_packet = &packets_[current_ % 2];
      current_++;
      queue_new = true;
    }
  }

  if (queue_new) {
    // Releasing packet_lock_ before calling Queue() is required to avoid lock
    // inversion with the PortDispatcher mutex.
    //
    // It is not possible to hit ZX_ERR_SHOULD_WAIT since none of these packets
    // are allocated with the port's default allocator.
    const zx_status_t status = port_->Queue(current_packet);
    ZX_ASSERT(status != ZX_ERR_SHOULD_WAIT);
    if (status != ZX_OK) {
      Guard<SpinLock, IrqSave> guard(&packet_lock_);
      packet_queued_ = false;
      packet_pending_ = false;
      release_ref = std::move(in_flight_ref_);
    }
  }
}

void PortPowerLevelController::PacketQueue::CancelAll() {
  fbl::RefPtr<PacketQueue> release_ref;
  {
    Guard<SpinLock, IrqSave> guard(&packet_lock_);
    packet_pending_ = false;

    for (PortPacket& packet : packets_) {
      // Releasing packet_lock_ while calling CancelQueued() is required to
      // avoid lock inversion with the PortDispatcher mutex.
      bool release_packet = false;
      guard.CallUnlocked([&] { release_packet = port_->CancelQueued(&packet); });

      if (release_packet && packet_queued_) {
        packet_queued_ = false;
        release_ref = std::move(in_flight_ref_);
      }
    }
  }
}

zx::result<uint32_t> PortPowerLevelController::Post(const PowerLevelUpdateRequest& pending) {
  if (pending.control != ControlInterface::kCpuDriver) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  if (port_->current_handle_count() == 0) {
    // There shouldn't be more attempts to queue.
    serving_.store(false, std::memory_order_relaxed);
    return zx::error(ZX_ERR_BAD_STATE);
  }

  zx_port_packet_t packet{
      // 'domain_id` used to register the port with a power domain.
      .key = pending.domain_id,
      .type = ZX_PKT_TYPE_PROCESSOR_POWER_LEVEL_TRANSITION_REQUEST,
      .status = ZX_OK,
      .processor_power_level_transition =
          {
              // `domain_id` in this context is subject to interpretation of `options`.
              .domain_id = pending.target_id,
              .options = pending.options,
              .control_interface = static_cast<uint64_t>(pending.control),
              .control_argument = pending.control_argument,
          },
  };

  packet_queue_->Queue(packet);

  // No CPUs need to be rescheduled to update their bookkeeping until userspace
  // sends an update about the completion of the update request.
  return zx::ok(0);
}

}  // namespace power_management

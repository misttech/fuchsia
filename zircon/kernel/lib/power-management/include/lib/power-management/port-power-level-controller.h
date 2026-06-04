// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_PORT_POWER_LEVEL_CONTROLLER_H_
#define ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_PORT_POWER_LEVEL_CONTROLLER_H_

#include <lib/zircon-internal/thread_annotations.h>
#include <lib/zx/result.h>
#include <zircon/assert.h>

#include <fbl/ref_ptr.h>
#include <kernel/spinlock.h>
#include <ktl/array.h>
#include <object/port_dispatcher.h>

#include "power-level-controller.h"

namespace power_management {

// Interface representing an entity in charge of update requests that are not handled by the kernel.
//
// In essence there will be only one type of transition handler, but we introduce the interface to
// decouple most of the code from the kernel environment.
class PortPowerLevelController final : public PowerLevelController {
  // Enables the named constructor to use MakeRefCountedChecked while protecting
  // the normal constructor from public use.
  enum PrivateConstructorTag : bool { PrivateConstructorValue };

  // Forward-declare the nested class.
  class PacketQueue;

 public:
  static zx::result<fbl::RefPtr<PortPowerLevelController>> Create(
      fbl::RefPtr<PortDispatcher> dispatcher);

  explicit PortPowerLevelController(PrivateConstructorTag, fbl::RefPtr<PortDispatcher> dispatcher,
                                    fbl::RefPtr<PacketQueue> queue)
      : PowerLevelController(ControlInterface::kCpuDriver),
        port_(std::move(dispatcher)),
        packet_queue_(std::move(queue)) {}

  ~PortPowerLevelController() final;

  // Process a pending request, which is a pending transition which could not be performed in the
  // context it originated. This method provide no guarantees on what exactly is performed. It may
  // provide defer with another entity
  zx::result<uint32_t> Post(const PowerLevelUpdateRequest& pending) final;

  // Unique id of the `ControlInterface` handler.
  uint64_t id() const final { return port_->get_koid(); }

  void ResetForTest() { packet_queue_->ResetForTest(); }

 private:
  // Stashes the latest update on the available (unqueued) packet. By becoming the packet allocator,
  // the `Free` hook will tell us when the unavailable packet becomes available. At this point, we
  // can check if there are any pending updates. In that case, we queue the next packet.
  //
  // Also this construct immediately bounds the amount of possible queued packets to one at any
  // given time and the memory used per power domain to two packets.
  class PacketQueue final : public fbl::RefCounted<PacketQueue>, public PortAllocator {
   public:
    explicit PacketQueue(fbl::RefPtr<PortDispatcher> port) : port_(std::move(port)) {}
    ~PacketQueue() final = default;

    PortPacket* Alloc() final { return nullptr; }
    void Free(PortPacket* packet) final;

    void Queue(const zx_port_packet_t& packet);

    void CancelAll();

    void ResetForTest() {
      fbl::RefPtr<PacketQueue> release_ref;
      {
        Guard<SpinLock, IrqSave> guard(&packet_lock_);
        packet_queued_ = false;
        packet_pending_ = false;
        release_ref = std::move(in_flight_ref_);
      }
    }

   private:
    DECLARE_SPINLOCK(PacketQueue) packet_lock_;

    // Current packet stashing changes.
    TA_GUARDED(&packet_lock_) size_t current_ = 0;
    TA_GUARDED(&packet_lock_) bool packet_pending_ = false;
    TA_GUARDED(&packet_lock_) bool packet_queued_ = false;

    // A reference to this queue, held while a packet is in-flight in the port.
    TA_GUARDED(&packet_lock_) fbl::RefPtr<PacketQueue> in_flight_ref_;

    TA_GUARDED(&packet_lock_)
    ktl::array<PortPacket, 2> packets_ = {
        PortPacket{nullptr, this},
        PortPacket{nullptr, this},
    };

    const fbl::RefPtr<PortDispatcher> port_;
  };

  const fbl::RefPtr<PortDispatcher> port_;
  const fbl::RefPtr<PacketQueue> packet_queue_;
};

}  // namespace power_management

#endif  // ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_PORT_POWER_LEVEL_CONTROLLER_H_

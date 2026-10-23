// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/unittest/unittest.h>

#include <ktl/byte.h>
#include <object/handle.h>
#include <object/port_dispatcher.h>

#include "tests.h"

#include <ktl/enforce.h>

namespace {

// Observes whether the port honours the PortAllocator::Free() completion protocol.
class SinglePortAllocator final : public PortAllocator {
 public:
  PortPacket* Alloc() override {
    DEBUG_ASSERT(allocated_ == 0);
    ++allocated_;
    return new (&storage_) PortPacket(nullptr, this);
  }
  void Free(PortPacket* port_packet) override {
    DEBUG_ASSERT(port_packet == reinterpret_cast<PortPacket*>(&storage_));
    ++freed_;
    port_packet->~PortPacket();
  }

  int allocated() const { return allocated_; }
  int freed() const { return freed_; }

 private:
  alignas(PortPacket) ktl::byte storage_[sizeof(PortPacket)];
  int allocated_ = 0;
  int freed_ = 0;
};

bool port_cancel_key_frees_ephemeral_packets() {
  BEGIN_TEST;

  KernelHandle<PortDispatcher> handle;
  zx_rights_t rights;
  ASSERT_EQ(ZX_OK, PortDispatcher::Create(0, &handle, &rights));
  fbl::RefPtr<PortDispatcher> port = handle.dispatcher();

  SinglePortAllocator allocator;
  constexpr uint64_t kKey = 0xF1;

  PortPacket* packet = allocator.Alloc();
  ASSERT_NONNULL(packet);
  packet->packet.key = kKey;
  packet->packet.type = ZX_PKT_TYPE_USER;
  ASSERT_EQ(ZX_OK, port->Queue(packet));

  // Cancelling by key must unlink the packet AND return it to its allocator.
  ASSERT_EQ(ZX_OK, port->CancelKey(kKey));

  EXPECT_EQ(1, allocator.allocated());
  EXPECT_EQ(1, allocator.freed());

  END_TEST;
}

// A two-slot PortAllocator that counts allocations and frees. Optionally, freeing the first
// packet re-enters the port to cancel the second one, which is exactly what PagerProxy may do
// (Free() -> ClearAsyncRequest() -> CancelQueued(&packet_)) while on_zero_handles is draining
// the queue with its lock dropped.
class TwoSlotPortAllocator final : public PortAllocator {
 public:
  PortPacket* Alloc() override {
    DEBUG_ASSERT(allocated_ < kSlots);
    // Remember the pointer placement-new hands back rather than recomputing it from the raw
    // storage later, so no cast is ever needed to get at a slot's PortPacket.
    PortPacket* port_packet = new (&storage_[allocated_]) PortPacket(nullptr, this);
    packets_[allocated_++] = port_packet;
    return port_packet;
  }
  void Free(PortPacket* port_packet) override {
    ++freed_;
    if (port_packet == packet(0) && cancel_on_free_ != nullptr) {
      // Re-enter the port from within Free(). The port must not be holding its lock here, and
      // the second packet must still be observably queued (not sitting on some private list).
      cancel_result_ = cancel_on_free_->CancelQueued(packet(1));
    }
    port_packet->~PortPacket();
  }

  PortPacket* packet(int i) {
    DEBUG_ASSERT(i < allocated_);
    return packets_[i];
  }
  void cancel_second_on_first_free(PortDispatcher* port) { cancel_on_free_ = port; }

  int allocated() const { return allocated_; }
  int freed() const { return freed_; }
  bool cancel_result() const { return cancel_result_; }

 private:
  static constexpr int kSlots = 2;
  struct alignas(PortPacket) Slot {
    ktl::byte bytes[sizeof(PortPacket)];
  };
  Slot storage_[kSlots];
  PortPacket* packets_[kSlots] = {};
  int allocated_ = 0;
  int freed_ = 0;
  PortDispatcher* cancel_on_free_ = nullptr;
  bool cancel_result_ = false;
};

bool port_on_zero_handles_frees_ephemeral_packets() {
  BEGIN_TEST;

  KernelHandle<PortDispatcher> handle;
  zx_rights_t rights;
  ASSERT_EQ(ZX_OK, PortDispatcher::Create(0, &handle, &rights));

  TwoSlotPortAllocator allocator;
  for (uint64_t key = 1; key <= 2; ++key) {
    PortPacket* packet = allocator.Alloc();
    ASSERT_NONNULL(packet);
    packet->packet.key = key;
    packet->packet.type = ZX_PKT_TYPE_USER;
    ASSERT_EQ(ZX_OK, handle.dispatcher()->Queue(packet));
  }
  EXPECT_EQ(2, allocator.allocated());
  EXPECT_EQ(0, allocator.freed());

  // Dropping the last handle runs on_zero_handles, which must return every queued ephemeral
  // packet to its (non-default) allocator.
  handle.reset();
  EXPECT_EQ(2, allocator.freed());

  END_TEST;
}

// Regression test for the on_zero_handles lock-release window: while packet A is being freed
// with the port lock dropped, a concurrent CancelQueued(B) must see B exactly as the C++
// implementation left it -- still queued on |packets_| -- so it unlinks B from the right list
// and hands it back to the caller. If B had instead been staged on a private free list without
// being canceled, CancelQueued(B) would corrupt both lists.
bool port_cancel_queued_from_free_during_on_zero_handles() {
  BEGIN_TEST;

  KernelHandle<PortDispatcher> handle;
  zx_rights_t rights;
  ASSERT_EQ(ZX_OK, PortDispatcher::Create(0, &handle, &rights));
  fbl::RefPtr<PortDispatcher> port = handle.dispatcher();

  TwoSlotPortAllocator allocator;
  PortPacket* a = allocator.Alloc();
  PortPacket* b = allocator.Alloc();
  ASSERT_NONNULL(a);
  ASSERT_NONNULL(b);
  a->packet.key = 1;
  a->packet.type = ZX_PKT_TYPE_USER;
  b->packet.key = 2;
  b->packet.type = ZX_PKT_TYPE_USER;
  ASSERT_EQ(ZX_OK, port->Queue(a));
  ASSERT_EQ(ZX_OK, port->Queue(b));
  allocator.cancel_second_on_first_free(port.get());

  // on_zero_handles pops A first and frees it with the lock dropped; Free(A) cancels B.
  handle.reset();

  // B was still queued when Free(A) ran, so CancelQueued(B) succeeded and B is now owned by the
  // test again rather than freed by the port.
  EXPECT_TRUE(allocator.cancel_result());
  EXPECT_EQ(1, allocator.freed());
  EXPECT_FALSE(b->InContainer());
  EXPECT_FALSE(b->is_canceled());

  // Clean up B ourselves; the port must have nothing further to do with it.
  EXPECT_FALSE(port->CancelQueued(b));
  allocator.Free(b);
  EXPECT_EQ(2, allocator.freed());

  END_TEST;
}

}  // namespace

UNITTEST_START_TESTCASE(port_tests)
UNITTEST("cancel_key frees ephemeral packets", port_cancel_key_frees_ephemeral_packets)
UNITTEST("on_zero_handles frees ephemeral packets", port_on_zero_handles_frees_ephemeral_packets)
UNITTEST("CancelQueued from Free during on_zero_handles",
         port_cancel_queued_from_free_during_on_zero_handles)
UNITTEST_END_TESTCASE(port_tests, "port", "PortDispatcher tests")

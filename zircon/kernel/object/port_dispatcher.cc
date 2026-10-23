// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/port_dispatcher.h"

#include <assert.h>
#include <lib/boot-options/boot-options.h>
#include <lib/counters.h>
#include <lib/object_cache.h>
#include <platform.h>
#include <pow2.h>
#include <zircon/compiler.h>
#include <zircon/errors.h>
#include <zircon/rights.h>
#include <zircon/syscalls/port.h>
#include <zircon/types.h>

#include <new>

#include <fbl/alloc_checker.h>
#include <fbl/arena.h>
#include <kernel/ffi.h>
#include <kernel/semaphore.h>
#include <ktl/utility.h>
#include <lk/init.h>
#include <object/process_dispatcher.h>
#include <object/thread_dispatcher.h>

// All port sub-packets must be exactly 32 bytes
static_assert(sizeof(zx_packet_user_t) == 32, "incorrect size for zx_packet_signal_t");
static_assert(sizeof(zx_packet_signal_t) == 32, "incorrect size for zx_packet_signal_t");
static_assert(sizeof(zx_packet_guest_bell_t) == 32, "incorrect size for zx_packet_guest_bell_t");
static_assert(sizeof(zx_packet_guest_mem_t) == 32, "incorrect size for zx_packet_guest_mem_t");
static_assert(sizeof(zx_packet_guest_io_t) == 32, "incorrect size for zx_packet_guest_io_t");
static_assert(sizeof(zx_packet_guest_vcpu_t) == 32, "incorrect size for zx_packet_guest_vcpu_t");
static_assert(sizeof(zx_packet_interrupt_t) == 32, "incorrect size for zx_packet_interrupt_t");
static_assert(sizeof(zx_packet_page_request_t) == 32,
              "incorrect size for zx_packet_page_request_t");

static_assert(sizeof(PortDispatcher) == kPortDispatcherStateSize + kPortDispatcherStateOffset,
              "PortDispatcher size mismatch");
static_assert(alignof(PortDispatcher) == kPortDispatcherStateAlign,
              "PortDispatcher align mismatch");

static_assert(sizeof(PortPacket) == kPortPacketSize, "PortPacket size mismatch");
static_assert(alignof(PortPacket) == kPortPacketAlign, "PortPacket align mismatch");

// Rust reads and writes every PortPacket field directly, so pin each field offset in addition to
// the overall size. PortPacket is not standard-layout (it derives from DoublyLinkedListable), so
// offsetof needs -Winvalid-offsetof suppressed.
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Winvalid-offsetof"
static_assert(__builtin_offsetof(PortPacket, packet) == kPortPacketPacketOffset,
              "PortPacket::packet offset mismatch");
static_assert(__builtin_offsetof(PortPacket, handle) == kPortPacketHandleOffset,
              "PortPacket::handle offset mismatch");
static_assert(__builtin_offsetof(PortPacket, observer) == kPortPacketObserverOffset,
              "PortPacket::observer offset mismatch");
static_assert(__builtin_offsetof(PortPacket, allocator) == kPortPacketAllocatorOffset,
              "PortPacket::allocator offset mismatch");
#pragma clang diagnostic pop
// The list node is the DoublyLinkedListable base subobject: it must precede |packet| and is
// exactly as large as the gap before it, so it occupies [kPortPacketNodeOffset, packet).
static_assert(kPortPacketNodeOffset == 0, "PortPacket list node must be the first subobject");
static_assert(sizeof(fbl::DoublyLinkedListable<PortPacket*>) ==
                  kPortPacketPacketOffset - kPortPacketNodeOffset,
              "PortPacket list node size mismatch");
// Rust treats PortPacket::observer as a bare PortObserver*.
static_assert(sizeof(object_cache::UniquePtr<const PortObserver>) == sizeof(void*),
              "object_cache::UniquePtr must be pointer-sized");
static_assert(sizeof(PortAllocator*) == sizeof(void*), "PortAllocator* must be pointer-sized");

static_assert(sizeof(PortObserver) == kPortObserverSize, "PortObserver size mismatch");
static_assert(alignof(PortObserver) == kPortObserverAlign, "PortObserver align mismatch");

KCOUNTER(port_ephemeral_packet_live, "port.ephemeral_packet.live")
KCOUNTER(port_ephemeral_packet_allocated, "port.ephemeral_packet.allocated")
KCOUNTER(port_ephemeral_packet_freed, "port.ephemeral_packet.freed")
KCOUNTER(port_observer_live, "port.observer.live")
KCOUNTER(port_observer_allocated, "port.observer.allocated")
KCOUNTER(port_observer_freed, "port.observer.freed")

// Implements the PortAllocator interface and trivially forwards to the
// ObjectCache allocator for PortPackets defined below.
struct PortPacketCacheAllocator final : PortAllocator {
  ~PortPacketCacheAllocator() override = default;
  PortPacket* Alloc() override;
  void Free(PortPacket* port_packet) override;
};

namespace {

// Per-cpu cache allocator for PortPackets.
object_cache::ObjectCache<PortPacket, object_cache::Option::PerCpu> packet_allocator;

// Per-cpu cache allocator for PortObservers.
object_cache::ObjectCache<PortObserver, object_cache::Option::PerCpu> observer_allocator;

// A trivial instance of the default PortAllocator for comparisons and to supply
// the vtable used outside of this compilation unit.
PortPacketCacheAllocator default_port_allocator;

}  // namespace.

PortPacket* PortPacketCacheAllocator::Alloc() {
  zx::result result = packet_allocator.Allocate(nullptr, this);
  if (result.is_error()) {
    printf("WARNING: Could not allocate new port packet: %d\n", result.error_value());
    return nullptr;
  }
  kcounter_add(port_ephemeral_packet_live, 1);
  kcounter_add(port_ephemeral_packet_allocated, 1);
  return result.value().release();
}

void PortPacketCacheAllocator::Free(PortPacket* port_packet) {
  kcounter_add(port_ephemeral_packet_live, -1);
  kcounter_add(port_ephemeral_packet_freed, 1);
  object_cache::UniquePtr<PortPacket> destroyer{port_packet};
}

PortPacket::PortPacket(const void* handle, PortAllocator* allocator)
    : packet{}, handle(handle), observer(nullptr), allocator(allocator) {
  // Note that packet is initialized to zeros.
}

PortObserver::PortObserver(uint32_t options, const Handle* handle, fbl::RefPtr<PortDispatcher> port,
                           uint64_t key, zx_signals_t signals)
    : SignalObserver(options),
      packet_(handle, nullptr),
      port_(ktl::move(port)),
      dispatcher_(handle->dispatcher().get()) {
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Winvalid-offsetof"
  static_assert(
      __builtin_offsetof(PortObserver, observer_list_node_state_) == kPortObserverListNodeOffset,
      "PortObserver list node offset mismatch");
#pragma clang diagnostic pop

  DEBUG_ASSERT(handle != nullptr);
  DEBUG_ASSERT(dispatcher_ != nullptr);

  auto& packet = packet_.packet;
  packet.status = ZX_OK;
  packet.key = key;
  packet.type = ZX_PKT_TYPE_SIGNAL_ONE;
  packet.signal.trigger = signals;

  kcounter_add(port_observer_live, 1);
  kcounter_add(port_observer_allocated, 1);
}

PortObserver::~PortObserver() {
  kcounter_add(port_observer_live, -1);
  kcounter_add(port_observer_freed, 1);
}

void PortObserver::OnMatch(zx_signals_t signals, OwnedWaitQueue* queue_to_own) {
  if (options() & ZX_WAIT_ASYNC_TIMESTAMP) {
    // Getting the current time can be somewhat expensive.
    packet_.packet.signal.timestamp = current_mono_time();
  } else if (options() & ZX_WAIT_ASYNC_BOOT_TIMESTAMP) {
    packet_.packet.signal.timestamp = current_boot_time();
  }

  // Queuing the packet will transfer ownership of the observer's reference to the PortDispatcher
  // to the observers_ container guarded by get_lock(). This container including its reference(s) to
  // the PortDispatcher could be destroyed by another thread. QueueAndRemoveObserver requires that
  // the PortDispatcher be alive for the entire call so we have to retain a reference on this thread
  // until it returns.
  fbl::RefPtr<PortDispatcher> port_ref(port_);

  const zx_status_t status =
      port_ref->QueueAndRemoveObserver(&packet_, signals, this, queue_to_own);
  // The |QueueAndRemoveObserver| call may have deleted |this|, so it is not safe to access any
  // members now.

  // The packet is not allocated in the packet arena and does not count against the per-port limit
  // so |QueueAndRemoveObserver| cannot fail due to the packet count. However, the last handle to
  // the port may have been closed so it can still fail with ZX_ERR_BAD_HANDLE.  Just ignore
  // ZX_ERR_BAD_HANDLE because there is nothing to be done.
  DEBUG_ASSERT_MSG(status == ZX_OK || status == ZX_ERR_BAD_HANDLE || status == ZX_ERR_CANCELED ||
                       status == ZX_ERR_SHOULD_WAIT,
                   "status %d\n", status);
}

void PortObserver::OnCancel(zx_signals_t signals) {
  // |MaybeReap| may unlink this observer and release the port lock while a concurrent
  // on_zero_handles finishes and drops the last handle's reference; destroying |this| would then
  // drop |port_| (the final reference) while the Rust side still holds |&PortDispatcher|. Retain
  // a reference on this thread until the call returns, as |OnMatch| does above.
  fbl::RefPtr<PortDispatcher> port_ref(port_);

  port_ref->MaybeReap(this, &packet_);
  // The |MaybeReap| call may have deleted |this|, so it is not safe to access any members now.
}

bool PortObserver::MatchesKey(const void* port, uint64_t key) {
  return key == packet_.key() && port == port_.get();
}

/////////////////////////////////////////////////////////////////////////////////////////

PortDispatcher::PortDispatcher(uint32_t options) : Dispatcher(0u) {
  DISPATCHER_VERIFY_OFFSET(PortDispatcher, kPortDispatcherStateOffset);
  rust_port_dispatcher_state_init(&opaque_storage_, this, options);
}

IMPLEMENT_DISPATCHER_RUST_STATE(PortDispatcher, rust_port_dispatcher_state_get_lock,
                                rust_port_dispatcher_state_destroy)

zx_status_t PortDispatcher::Create(uint32_t options, KernelHandle<PortDispatcher>* handle,
                                   zx_rights_t* rights) {
  if (options && options != ZX_PORT_BIND_TO_INTERRUPT) {
    return ZX_ERR_INVALID_ARGS;
  }
  fbl::AllocChecker ac;
  KernelHandle new_handle(fbl::AdoptRef(new (&ac) PortDispatcher(options)));
  if (!ac.check())
    return ZX_ERR_NO_MEMORY;

  *rights = default_rights();
  *handle = ktl::move(new_handle);
  return ZX_OK;
}

void PortDispatcher::InitializeCacheAllocators(uint32_t /*level*/) {
  const size_t observer_reserve_pages = BootOptions::Get()->port_observer_reserve_pages;

  zx::result observer_result =
      object_cache::ObjectCache<PortObserver, object_cache::Option::PerCpu>::Create(
          observer_reserve_pages);

  ASSERT(observer_result.is_ok());
  observer_allocator = ktl::move(*observer_result);

  // Reserve 1 page per CPU for servicing ephemeral PortPackets, unless
  // overridden on the command line.
  const size_t packet_reserve_pages = BootOptions::Get()->port_packet_reserve_pages;

  zx::result packet_result =
      object_cache::ObjectCache<PortPacket, object_cache::Option::PerCpu>::Create(
          packet_reserve_pages);

  ASSERT(packet_result.is_ok());
  packet_allocator = ktl::move(*packet_result);
}

// Initialize the cache after the percpu data structures are initialized.
LK_INIT_HOOK(port_observer_cache_init, PortDispatcher::InitializeCacheAllocators,
             LK_INIT_LEVEL_KERNEL)

extern "C" {

zx_status_t cpp_port_dispatcher_create(uint32_t options,
                                       ffi::Uninitialized<KernelHandle<PortDispatcher>>* handle_out,
                                       ffi::Uninitialized<zx_rights_t>* rights_out) {
  KernelHandle<PortDispatcher> handle;
  zx_rights_t rights;
  zx_status_t status = PortDispatcher::Create(options, &handle, &rights);
  if (status == ZX_OK) {
    handle_out->Initialize(ktl::move(handle));
    rights_out->Initialize(rights);
  }
  return status;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE PortPacket* cpp_port_packet_alloc() { return default_port_allocator.Alloc(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_port_packet_free(PortPacket* packet) {
  default_port_allocator.Free(packet);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE PortAllocator* cpp_port_default_allocator() { return &default_port_allocator; }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_port_allocator_free(PortAllocator* allocator, PortPacket* packet) {
  allocator->Free(packet);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE int64_t cpp_port_ephemeral_packet_freed_count() {
  return port_ephemeral_packet_freed.SumAcrossAllCpus();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE int64_t cpp_port_observer_freed_count() {
  return port_observer_freed.SumAcrossAllCpus();
}

PortObserver* cpp_port_observer_alloc(uint32_t options, const Handle* handle, PortDispatcher* port,
                                      uint64_t key, zx_signals_t signals) {
  zx::result result =
      observer_allocator.Allocate(options, handle, fbl::RefPtr<PortDispatcher>(port), key, signals);
  if (result.is_error()) {
    return nullptr;
  }
  return result.value().release();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_port_observer_free(PortObserver* observer) {
  object_cache::UniquePtr<PortObserver> destroyer(observer);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_port_observer_cancel(PortObserver* observer) { observer->Cancel(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_port_observer_add_dispatcher_ref_locked(PortObserver* observer) {
  observer->AddDispatcherRefLocked();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE Dispatcher* cpp_port_observer_unlink_dispatcher_locked(PortObserver* observer) {
  return observer->UnlinkDispatcherLocked();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_port_observer_matches_key(PortObserver* observer,
                                                     const PortDispatcher* port, uint64_t key) {
  return static_cast<SignalObserver*>(observer)->MatchesKey(port, key);
}

// Deregisters |observer| from |dispatcher|. The PortObserver -> SignalObserver upcast happens
// here, on the C++ side, so Rust never has to assume where the base subobject lives.
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_port_observer_remove_from_dispatcher(PortObserver* observer,
                                                                Dispatcher* dispatcher) {
  return dispatcher->RemoveObserver(observer);
}

}  // extern "C"

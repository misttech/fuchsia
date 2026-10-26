// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PORT_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PORT_DISPATCHER_H_

#include <lib/object-constants.h>
#include <lib/object_cache.h>
#include <sys/types.h>
#include <zircon/rights.h>
#include <zircon/syscalls/port.h>
#include <zircon/types.h>

#include <fbl/intrusive_double_list.h>
#include <kernel/ffi.h>
#include <kernel/mutex.h>
#include <object/dispatcher.h>
#include <object/handle.h>
#include <object/opaque_storage.h>
#include <object/signal_observer.h>

// PortObserver lifetime and locking notes: the canonical description of the
// PortDispatcher / PortObserver / Dispatcher pointer graph, cancelation
// ordering, and lock hierarchy lives in the module documentation at the top of
// port_dispatcher.rs.

class PortDispatcher;
class PortObserver;
struct PortPacket;

struct PortAllocator {
  virtual ~PortAllocator() = default;

  virtual PortPacket* Alloc() = 0;
  virtual void Free(PortPacket* port_packet) = 0;
};

// kPortPacketTypeCanceled (the sentinel written by Cancel()) is defined in
// <lib/object-constants.h> so that the Rust PortDispatcher shares the same value.
struct PortPacket final : public fbl::DoublyLinkedListable<PortPacket*> {
  zx_port_packet_t packet;
  const void* const handle;
  object_cache::UniquePtr<const PortObserver> observer;
  PortAllocator* const allocator;

  PortPacket(const void* handle, PortAllocator* allocator);
  PortPacket(const PortPacket&) = delete;
  void operator=(PortPacket) = delete;

  uint64_t key() const { return packet.key; }
  void Cancel() { packet.type = kPortPacketTypeCanceled; }
  bool is_canceled() const { return packet.type == kPortPacketTypeCanceled; }
};

// Observers are weakly contained in Dispatchers.
class PortObserver final : public SignalObserver {
 public:
  using ListNodeState = fbl::DoublyLinkedListNodeState<PortObserver*>;

  PortObserver(uint32_t options, const Handle* handle, fbl::RefPtr<PortDispatcher> port,
               uint64_t key, zx_signals_t signals);

  ~PortObserver() final;

  // May only be called while holding PortDispatcher lock.
  Dispatcher* UnlinkDispatcherLocked() {
    DEBUG_ASSERT(DispatcherLockIsHeld());
    Dispatcher* dispatcher = dispatcher_;
    dispatcher_ = nullptr;
    return dispatcher;
  }

  // Manually increment the reference count of our dispatcher.
  // May only be called while holding PortDispatcher lock.
  void AddDispatcherRefLocked() {
    DEBUG_ASSERT(DispatcherLockIsHeld());
    dispatcher_->AddRef();
  }

  // May only be called while holding PortDispatcher lock.
  void Cancel() {
    DEBUG_ASSERT(DispatcherLockIsHeld());
    packet_.Cancel();
  }

 private:
  PortObserver(const PortObserver&) = delete;
  PortObserver& operator=(const PortObserver&) = delete;

  // |SignalObserver| implementation.
  void OnMatch(zx_signals_t signals, OwnedWaitQueue* queue_to_own) final;
  void OnCancel(zx_signals_t signals) final;
  bool MatchesKey(const void* port, uint64_t key) final;

  // Defined below after PortDispatcher.
  inline bool DispatcherLockIsHeld() const;

  PortPacket packet_;

  fbl::RefPtr<PortDispatcher> const port_;

  // Guarded by port_->get_lock();
  ListNodeState observer_list_node_state_;

  // Guarded by port_->get_lock();
  // This is a raw pointer as the PortObserver does not have an ownership stake in the dispatcher.
  Dispatcher* dispatcher_;
};

extern "C" {

void rust_port_dispatcher_state_init(void* state, const void* disp, uint32_t options);
void rust_port_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_port_dispatcher_state_get_lock(const void* state);
void rust_port_dispatcher_on_zero_handles(const PortDispatcher* disp);
zx_status_t rust_port_dispatcher_queue(const PortDispatcher* disp, PortPacket* port_packet);
zx_status_t rust_port_dispatcher_queue_and_remove_observer(const PortDispatcher* disp,
                                                           PortPacket* port_packet,
                                                           zx_signals_t observed,
                                                           PortObserver* observer,
                                                           OwnedWaitQueue* queue_to_own);
void rust_port_dispatcher_maybe_reap(const PortDispatcher* disp, PortObserver* observer,
                                     PortPacket* port_packet);
zx_status_t rust_port_dispatcher_make_observer(const PortDispatcher* disp, uint32_t options,
                                               const Handle* handle, uint64_t key,
                                               zx_signals_t signals, PortObserver** out_observer);
bool rust_port_dispatcher_cancel_queued_packet(const PortDispatcher* disp, PortPacket* port_packet);
zx_status_t rust_port_dispatcher_cancel_key(const PortDispatcher* disp, uint64_t key);

zx_status_t cpp_port_dispatcher_create(uint32_t options,
                                       ffi::Uninitialized<KernelHandle<PortDispatcher>>* handle_out,
                                       ffi::Uninitialized<zx_rights_t>* rights_out);

PortPacket* cpp_port_packet_alloc();
void cpp_port_packet_free(PortPacket* packet);
PortAllocator* cpp_port_default_allocator();
void cpp_port_allocator_free(PortAllocator* allocator, PortPacket* packet);
int64_t cpp_port_ephemeral_packet_freed_count();
int64_t cpp_port_observer_freed_count();

PortObserver* cpp_port_observer_alloc(uint32_t options, const Handle* handle, PortDispatcher* port,
                                      uint64_t key, zx_signals_t signals);
void cpp_port_observer_free(PortObserver* observer);
void cpp_port_observer_cancel(PortObserver* observer);
void cpp_port_observer_add_dispatcher_ref_locked(PortObserver* observer);
Dispatcher* cpp_port_observer_unlink_dispatcher_locked(PortObserver* observer);
bool cpp_port_observer_matches_key(PortObserver* observer, const PortDispatcher* port,
                                   uint64_t key);
bool cpp_port_observer_remove_from_dispatcher(PortObserver* observer, Dispatcher* dispatcher);

}  // extern "C"

// The PortDispatcher implements the port kernel object which is the cornerstone
// for waiting on object changes in Zircon. The PortDispatcher handles 3 usage
// cases:
//  1- Object state change notification: zx_object_wait_async()
//  2- Manual queuing: zx_port_queue()
//  3- Interrupt change notification: zx_interrupt_bind()
//
// The state and queue logic live in Rust (port_dispatcher.rs).
class PortDispatcher final : public Dispatcher {
 public:
  static constexpr zx_rights_t default_rights() { return ZX_DEFAULT_PORT_RIGHTS; }
  static zx_status_t Create(uint32_t options, KernelHandle<PortDispatcher>* handle,
                            zx_rights_t* rights);

  ~PortDispatcher() final;
  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_PORT; }

  void on_zero_handles() final { rust_port_dispatcher_on_zero_handles(this); }

  bool is_waitable() const final { return default_rights() & ZX_RIGHT_WAIT; }
  zx_koid_t get_related_koid() const final { return ZX_KOID_INVALID; }

  zx_status_t user_signal_self(uint32_t clear_mask, uint32_t set_mask) final {
    return UserSignalSelfSolo(this, clear_mask, set_mask, 0);
  }
  zx_status_t user_signal_peer(uint32_t clear_mask, uint32_t set_mask) final {
    return ZX_ERR_NOT_SUPPORTED;
  }

  // Queues a packet on the port.
  zx_status_t Queue(PortPacket* port_packet) {
    return rust_port_dispatcher_queue(this, port_packet);
  }

  // Queues a packet based on a signal observation and deregisters the observer from the port.
  // If |observer| is non-null, also removes it from the observer list.
  zx_status_t QueueAndRemoveObserver(PortPacket* port_packet, zx_signals_t observed,
                                     PortObserver* observer = nullptr,
                                     OwnedWaitQueue* queue_to_own = nullptr) {
    return rust_port_dispatcher_queue_and_remove_observer(this, port_packet, observed, observer,
                                                          queue_to_own);
  }

  // This method determines the observer's fate. Upon return, one of the following will have
  // occurred:
  //
  // 1. The observer is destroyed.
  //
  // 2. The observer is linked to an alreadyed queued packet and will be destroyed when the packet
  // is destroyed (Queued or CancelQueued).
  //
  // 3. The observer is left for on_zero_handles to destroyed.
  void MaybeReap(PortObserver* observer, PortPacket* port_packet) {
    rust_port_dispatcher_maybe_reap(this, observer, port_packet);
  }

  // Creates an observer registered with this port.
  //
  // Requires that a read lock is held on the handle table containing |handle|.
  zx::result<SignalObserver*> MakeObserver(uint32_t options, const Handle* handle, uint64_t key,
                                           zx_signals_t signals) {
    PortObserver* observer = nullptr;
    zx_status_t status =
        rust_port_dispatcher_make_observer(this, options, handle, key, signals, &observer);
    if (status != ZX_OK) {
      return zx::error(status);
    }
    // Upcast on the C++ side so the SignalObserver base subobject is located by the compiler
    // rather than by assuming it sits at offset 0.
    return zx::ok(static_cast<SignalObserver*>(observer));
  }

  // Removes |port_packet| from this port's queue. Returns false if the packet was
  // not in this queue. It is undefined to call this with a packet queued in another port.
  bool CancelQueued(PortPacket* port_packet) {
    return rust_port_dispatcher_cancel_queued_packet(this, port_packet);
  }

  // Cancel all pending waits and packets registered with |key|.
  // Returns ZX_OK if any waits or packets were canceled and ZX_ERR_NOT_FOUND if
  // no matching waits or packets were found.
  zx_status_t CancelKey(uint64_t key) { return rust_port_dispatcher_cancel_key(this, key); }

  // Init hook that sets up the cache allocators used by this dispatcher.
  static void InitializeCacheAllocators(uint32_t level);

  // Returns true if the dispatcher lock |get_lock()| is currently held.
  bool DispatcherLockIsHeld() const { return get_lock()->lock().IsHeld(); }

 protected:
  Lock<CriticalMutex>* get_lock() const final;

 private:
  friend zx_status_t cpp_port_dispatcher_create(
      uint32_t options, ffi::Uninitialized<KernelHandle<PortDispatcher>>* handle_out,
      ffi::Uninitialized<zx_rights_t>* rights_out);
  explicit PortDispatcher(uint32_t options);

  OpaqueStorage<kPortDispatcherStateSize, kPortDispatcherStateAlign> opaque_storage_;
};

inline bool PortObserver::DispatcherLockIsHeld() const { return port_->DispatcherLockIsHeld(); }

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PORT_DISPATCHER_H_

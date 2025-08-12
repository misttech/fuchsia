// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_FENCE_H_
#define SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_FENCE_H_

#include <lib/async/cpp/wait.h>
#include <lib/async/dispatcher.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/zx/event.h>
#include <zircon/syscalls/port.h>
#include <zircon/types.h>

#include <fbl/intrusive_double_list.h>
#include <fbl/intrusive_single_list.h>
#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>

#include "src/graphics/display/drivers/coordinator/id-map.h"
#include "src/graphics/display/lib/api-types/cpp/event-id.h"

namespace display_coordinator {

class FenceReference;
class Fence;

// Interface between a Fence and the FenceCollection that owns it.
class FenceOwner {
 public:
  FenceOwner() = default;

  // FenceOwner pointers must remain stable.
  FenceOwner(const FenceOwner&) = delete;
  FenceOwner(FenceOwner&&) = delete;
  FenceOwner& operator=(const FenceOwner&) = default;
  FenceOwner& operator=(FenceOwner&&) = delete;

  virtual void OnFenceSignaled(FenceReference* fence_reference) = 0;

  // TODO(https://fxbug.dev/394422104): implementors must call `Fence::OnRefDead()`,
  // but they shouldn't have to.
  virtual void OnRefForFenceDead(Fence* fence) = 0;

 protected:
  // FenceOwner is not intended to be an owning pointer type.
  virtual ~FenceOwner() = default;
};

// Manages an event imported by a Coordinator client.
//
// The class currently uses Vulkan terminology, rather than Fuchsia event
// terminology.
//
// A single Fence can have multiple FenceReference objects, which allows an
// event to be treated as a semaphore independently of it being
// imported/released (i.e. can be released while still in use).
//
// Instances are not thread-safe and must be accessed on a single synchronized
// dispatcher.
class Fence : public fbl::RefCounted<Fence>,
              public IdMappable<fbl::RefPtr<Fence>, display::EventId>,
              public fbl::SinglyLinkedListable<fbl::RefPtr<Fence>> {
 public:
  // Fence state changes will be processed on `dispatcher`.
  //
  // `owner` must not be null and must outlive the newly created instance.
  //
  // `dispatcher` must not be null and must outlive the newly created instance.
  // The instance must be accessed exclusively on the dispatcher.
  //
  // `id` and `event` must be valid.
  Fence(FenceOwner* owner, fdf::UnownedSynchronizedDispatcher dispatcher, display::EventId id,
        zx::event event);

  ~Fence();

  Fence(const Fence& other) = delete;
  Fence(Fence&& other) = delete;
  Fence& operator=(const Fence& other) = delete;
  Fence& operator=(Fence&& other) = delete;

  // Creates a new FenceReference when an event is imported.
  bool CreateRef();
  // Clears a FenceReference when an event is released. Note that references to the cleared
  // FenceReference might still exist within the driver.
  void ClearRef();
  // Decrements the reference count and returns true if the last ref died.
  // TODO(https://fxbug.dev/394422104): Currently, the implicit contract is that this must be called
  // by the implementor of `FenceCallback::OnRefForFenceDead()`. Instead, this should be made
  // private so it can only be called by `FenceReference`, which is already a friend.
  bool OnRefDead();

  // Gets the fence reference for the current import. An individual fence reference cannot
  // be used for multiple things simultaneously.
  fbl::RefPtr<FenceReference> GetReference();

  // The raw event underlying this fence. Only used for validation.
  zx_handle_t event() const { return event_.get(); }

 private:
  void Signal() const;
  zx::result<> OnRefArmed(fbl::RefPtr<FenceReference> fence_reference);
  void OnRefDisarmed(FenceReference* fence_reference);

  // The fence reference corresponding to the current event import.
  fbl::RefPtr<FenceReference> cur_ref_;

  // A queue of fence references which are being waited upon. When the event is
  // signaled, the signal will be cleared and the first fence ref will be marked ready.
  fbl::DoublyLinkedList<fbl::RefPtr<FenceReference>> armed_refs_;

  void OnReady(async_dispatcher_t* dispatcher, async::WaitBase* self, zx_status_t status,
               const zx_packet_signal_t* signal);
  async::WaitMethod<Fence, &Fence::OnReady> ready_wait_{this};

  FenceOwner& owner_;
  const fdf::UnownedSynchronizedDispatcher dispatcher_;

  const zx::event event_;
  int ref_count_ = 0;
  zx_koid_t koid_ = 0;

  friend FenceReference;
};

// Each FenceReference represents a pending / active wait or signaling of the
// Fence it refers to, regardless of the Fence it refers to being imported
// or released by the Client.
//
// Instances are not thread-safe and must be accessed on a single synchronized
// dispatcher.
class FenceReference : public fbl::RefCounted<FenceReference>,
                       public fbl::DoublyLinkedListable<fbl::RefPtr<FenceReference>> {
 public:
  // `fence` must not be null.
  //
  // `dispatcher` must not be null and must outlive the newly created instance.
  // The instance must be accessed exclusively on the dispatcher.
  explicit FenceReference(fbl::RefPtr<Fence> fence, fdf::UnownedSynchronizedDispatcher dispatcher);

  FenceReference(const FenceReference& other) = delete;
  FenceReference(FenceReference&& other) = delete;
  FenceReference& operator=(const FenceReference& other) = delete;
  FenceReference& operator=(FenceReference&& other) = delete;

  ~FenceReference();

  void Signal() const;

  // The first of these two calls must be to `StartReadyWait()` and the next must be to
  // `ResetReadyWait()`. Subsequent calls must continue to alternate in the same way.
  zx::result<> StartReadyWait();
  void ResetReadyWait();

 private:
  const fbl::RefPtr<Fence> fence_;
  const fdf::UnownedSynchronizedDispatcher dispatcher_;
};

// Interface between a FenceCollection and the fencer.
class FenceCollectionListener {
 public:
  FenceCollectionListener() = default;

  // FenceCollectionListener pointers must remain stable.
  FenceCollectionListener(const FenceCollectionListener&) = delete;
  FenceCollectionListener(FenceCollectionListener&&) = delete;
  FenceCollectionListener& operator=(const FenceCollectionListener&) = default;
  FenceCollectionListener& operator=(FenceCollectionListener&&) = delete;

  virtual void OnFenceSignaled(FenceReference* fence_reference) = 0;

 protected:
  // FenceCollectionListener is not intended to be an owning pointer type.
  virtual ~FenceCollectionListener() = default;
};

// Manages the events (Fences) imported by a Coordinator client.
//
// Instances are not thread-safe and must be accessed on a single synchronized
// dispatcher.
class FenceCollection : public FenceOwner {
 public:
  // Creates an empty collection.
  //
  // `listener` methods are called on `dispatcher`. `listener` must not be null
  // and must outlive the newly created instance.
  //
  // `dispatcher` must not be null and must outlive the newly created instance.
  // The instance must be accessed exclusively on the dispatcher.
  FenceCollection(FenceCollectionListener* listener, fdf::UnownedSynchronizedDispatcher dispatcher);

  FenceCollection(const FenceCollection&) = delete;
  FenceCollection(FenceCollection&&) = delete;
  FenceCollection& operator=(const FenceCollection&) = delete;
  FenceCollection& operator=(FenceCollection&&) = delete;

  virtual ~FenceCollection() = default;

  // Explicit destruction step. Use this to control when fences are destroyed.
  void Clear();

  // Imports `event` so that it can subsequently be referenced by passing `id` to `GetFence()`.
  // `id` must not already be registered by a previous call to `ImportEvent()`, unless it was
  // subsequently unregistered by calling `ReleaseEvent()`.
  zx::result<> ImportEvent(zx::event event, display::EventId id);

  // Unregisters a fence that was previously registered by `ImportEvent()`.
  void ReleaseEvent(display::EventId id);

  // Gets reference to existing fence by its ID, or nullptr if no fence is found.
  fbl::RefPtr<FenceReference> GetFence(display::EventId id);

  // `FenceOwner`:
  void OnFenceSignaled(FenceReference* fence) override;
  void OnRefForFenceDead(Fence* fence) override;

 private:
  Fence::Map fences_;
  FenceCollectionListener& listener_;
  const fdf::UnownedSynchronizedDispatcher dispatcher_;
};

}  // namespace display_coordinator

#endif  // SRC_GRAPHICS_DISPLAY_DRIVERS_COORDINATOR_FENCE_H_

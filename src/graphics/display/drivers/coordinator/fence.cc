// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/coordinator/fence.h"

#include <lib/async/cpp/wait.h>
#include <lib/async/dispatcher.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/trace/event.h>
#include <lib/zx/event.h>
#include <lib/zx/result.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/status.h>
#include <zircon/types.h>

#include <utility>

#include <fbl/alloc_checker.h>
#include <fbl/intrusive_single_list.h>
#include <fbl/ref_ptr.h>

#include "src/graphics/display/lib/api-types/cpp/event-id.h"

namespace display_coordinator {

bool Fence::CreateRef() {
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());

  fbl::AllocChecker alloc_checker;
  cur_ref_ = fbl::AdoptRef(new (&alloc_checker)
                               FenceReference(fbl::RefPtr<Fence>(this), dispatcher_->borrow()));
  if (!alloc_checker.check()) {
    return false;
  }

  ++ref_count_;
  return true;
}

void Fence::ClearRef() {
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());
  cur_ref_ = nullptr;
}

fbl::RefPtr<FenceReference> Fence::GetReference() {
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());
  return cur_ref_;
}

void Fence::Signal() const {
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());
  event_.signal(0, ZX_EVENT_SIGNALED);
}

bool Fence::OnRefDead() {
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());
  return --ref_count_ == 0;
}

zx::result<> Fence::OnRefArmed(fbl::RefPtr<FenceReference> fence_reference) {
  ZX_DEBUG_ASSERT(fence_reference != nullptr);
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());
  ZX_DEBUG_ASSERT(!fence_reference->InContainer());

  if (armed_refs_.is_empty()) {
    ready_wait_.set_object(event_.get());
    ready_wait_.set_trigger(ZX_EVENT_SIGNALED);

    zx_status_t status = ready_wait_.Begin(dispatcher_->async_dispatcher());
    if (status != ZX_OK) {
      return zx::error(status);
    }
  }

  armed_refs_.push_back(std::move(fence_reference));
  return zx::ok();
}

void Fence::OnRefDisarmed(FenceReference* fence_reference) {
  ZX_DEBUG_ASSERT(fence_reference != nullptr);
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());
  // Ideally we would also check that it is in `armed_refs_`, not some other list.
  ZX_DEBUG_ASSERT(fence_reference->InContainer());

  armed_refs_.erase(*fence_reference);
  if (armed_refs_.is_empty()) {
    ready_wait_.Cancel();
  }
}

void Fence::OnReady(async_dispatcher_t* dispatcher, async::WaitBase* self, zx_status_t status,
                    const zx_packet_signal_t* signal) {
  ZX_DEBUG_ASSERT_MSG(status == ZX_OK, "Fence::OnReady failed: %s", zx_status_get_string(status));
  ZX_DEBUG_ASSERT(dispatcher != nullptr);
  ZX_DEBUG_ASSERT((signal->observed & ZX_EVENT_SIGNALED) != 0);
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());
  ZX_DEBUG_ASSERT(dispatcher == dispatcher_->async_dispatcher());
  TRACE_DURATION("gfx", "Display::Fence::OnReady");
  TRACE_FLOW_END("gfx", "event_signal", koid_);

  event_.signal(ZX_EVENT_SIGNALED, 0);

  fbl::RefPtr<FenceReference> fence_reference = armed_refs_.pop_front();
  owner_.OnFenceSignaled(fence_reference.get());

  if (!armed_refs_.is_empty()) {
    ready_wait_.Begin(dispatcher_->async_dispatcher());
  }
}

Fence::Fence(FenceOwner* owner, fdf::UnownedSynchronizedDispatcher dispatcher,
             display::EventId fence_id, zx::event event)
    : IdMappable(fence_id),
      owner_(*owner),
      dispatcher_(std::move(dispatcher)),
      event_(std::move(event)) {
  ZX_DEBUG_ASSERT(owner != nullptr);
  ZX_DEBUG_ASSERT(dispatcher_->get() != nullptr);
  ZX_DEBUG_ASSERT(fence_id != display::kInvalidEventId);
  ZX_DEBUG_ASSERT(event_.is_valid());
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());

  zx_info_handle_basic_t info;
  zx_status_t status = event_.get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr);
  ZX_DEBUG_ASSERT(status == ZX_OK);
  koid_ = info.koid;
}

Fence::~Fence() {
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());
  ZX_DEBUG_ASSERT(armed_refs_.is_empty());
  ZX_DEBUG_ASSERT(ref_count_ == 0);
}

zx::result<> FenceReference::StartReadyWait() {
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());
  return fence_->OnRefArmed(fbl::RefPtr<FenceReference>(this));
}

void FenceReference::ResetReadyWait() {
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());
  fence_->OnRefDisarmed(this);
}

void FenceReference::Signal() const { fence_->Signal(); }

FenceReference::FenceReference(fbl::RefPtr<Fence> fence,
                               fdf::UnownedSynchronizedDispatcher dispatcher)
    : fence_(std::move(fence)), dispatcher_(std::move(dispatcher)) {
  ZX_DEBUG_ASSERT(fence_ != nullptr);
  ZX_DEBUG_ASSERT(dispatcher_->get() != nullptr);
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());
}

FenceReference::~FenceReference() {
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());
  fence_->owner_.OnRefForFenceDead(fence_.get());
}

FenceCollection::FenceCollection(FenceCollectionListener* listener,
                                 fdf::UnownedSynchronizedDispatcher dispatcher)
    : listener_(*listener), dispatcher_(std::move(dispatcher)) {
  ZX_DEBUG_ASSERT(listener != nullptr);
  ZX_DEBUG_ASSERT(dispatcher_->get() != nullptr);
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());
}

void FenceCollection::Clear() {
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());

  // Use a temporary list to avoid reentrancy complications when resetting.
  fbl::SinglyLinkedList<fbl::RefPtr<Fence>> fences;
  while (!fences_.is_empty()) {
    fences.push_front(fences_.erase(fences_.begin()));
  }

  while (!fences.is_empty()) {
    fences.pop_front()->ClearRef();
  }
}

zx::result<> FenceCollection::ImportEvent(zx::event event, display::EventId id) {
  ZX_DEBUG_ASSERT(event.is_valid());
  ZX_DEBUG_ASSERT(id != display::kInvalidEventId);
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());

  auto fences_it = fences_.find(id);
  if (fences_it.IsValid()) {
    fdf::error("Refused to import an event with existing ID: {}", id.value());
    return zx::error(ZX_ERR_ALREADY_EXISTS);
  }

  fbl::AllocChecker alloc_checker;
  fbl::RefPtr<Fence> fence =
      fbl::AdoptRef(new (&alloc_checker) Fence(this, dispatcher_->borrow(), id, std::move(event)));
  if (!alloc_checker.check()) {
    fdf::error("Failed to allocate Fence for event ID: {}", id.value());
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  if (!fence->CreateRef()) {
    fdf::error("Failed to allocate FenceReference for event ID: {}", id.value());
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  bool successfully_inserted = fences_.insert_or_find(std::move(fence));
  ZX_DEBUG_ASSERT(successfully_inserted);
  return zx::ok();
}

void FenceCollection::ReleaseEvent(display::EventId id) {
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());

  // Hold a reference to prevent double locking if this destroys the fence.
  fbl::RefPtr<FenceReference> fence_reference = GetFence(id);
  if (fence_reference == nullptr) {
    return;
  }

  // TODO(https://fxbug.dev/394422104): this is an overly-complicated roundabout. It would be
  // simpler/clearer to simply remove the fence from the map here, and allow any outstanding
  // `FenceReference`s to keep the fence alive. Instead, the logic relies on `ClearRef()`
  // releasing a ref so that when the last ref is (immediately or eventually) released, then
  // `FenceCallback::OnRefForFenceDead()` (in production, implemented by `FenceCollection`) will
  // check if it was the last ref, and if so erase the fence from `fences_`.
  //
  // Unwinding this might not be quite as simple as I made it sound; the `CreateRef()/ClearRef()`
  // machinery will need to be revisited. If we simply erase the fence from `fences_`, there will
  // be a circular reference between the fence (via `Fence::cur_ref_`) and the fence ref (via
  // `FenceReference::fence_`).
  //
  // This raises the question of whether we even need to distinguish `Fence` and `FenceReference`.
  // There is some fancy stuff that allows multiple refs to arm themselves and be signaled in
  // order (once per signal of the underlying Zircon event), but AFAICT this is never used in
  // practice because there is exactly one `FenceReference`: the one stashed in `Fence::cur_ref_`.
  // But I digress; these breadcrumbs will hopefully help whoever comes next.
  fences_.find(id)->ClearRef();
}

fbl::RefPtr<FenceReference> FenceCollection::GetFence(display::EventId id) {
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());

  if (id == display::kInvalidEventId) {
    return nullptr;
  }
  auto fences_it = fences_.find(id);
  return fences_it.IsValid() ? fences_it->GetReference() : nullptr;
}

void FenceCollection::OnFenceSignaled(FenceReference* fence_reference) {
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());
  ZX_DEBUG_ASSERT(fence_reference != nullptr);

  listener_.OnFenceSignaled(fence_reference);
}

void FenceCollection::OnRefForFenceDead(Fence* fence) {
  ZX_DEBUG_ASSERT(fdf::Dispatcher::GetCurrent()->async_dispatcher() ==
                  dispatcher_->async_dispatcher());
  ZX_DEBUG_ASSERT(fence != nullptr);

  if (fence->OnRefDead()) {
    fences_.erase(fence->id());
  }
}

}  // namespace display_coordinator

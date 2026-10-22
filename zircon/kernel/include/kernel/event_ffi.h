// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_EVENT_FFI_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_EVENT_FFI_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

class Deadline;
class Event;
class OwnedWaitQueue;
enum class Interruptible : bool;

__BEGIN_CDECLS

// Rust implementations of the Event methods (kernel/event.rs). The C++ methods of the same name
// are thin forwarders to these and carry the lock annotations; none may be called with a
// ChainLockTransaction active.
zx_status_t rust_event_wait_worker(const Event* event, const Deadline* deadline,
                                   Interruptible interruptible, uint32_t signal_mask);
// |queue_to_own| may be null; otherwise its lock must not be held and it must not be downstream
// of the event's waiters' PI chains.
void rust_event_signal(const Event* event, zx_status_t wait_result, OwnedWaitQueue* queue_to_own);
zx_status_t rust_event_unsignal(const Event* event);
// ~Event: asserts the magic and resets the fields; the embedded WaitQueue is destroyed by C++.
void rust_event_destroy(Event* event);

__END_CDECLS

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_EVENT_FFI_H_

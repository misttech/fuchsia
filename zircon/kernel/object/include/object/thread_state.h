// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_THREAD_STATE_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_THREAD_STATE_H_

// TRANSITIONAL. The thread state machine now lives in //zircon/kernel/object/thread_state.rs,
// which is the source of truth for both the enums below and every legal transition between
// them. What remains here is the two enumerations, so that the C++ methods of ThreadDispatcher
// that have not been ported yet keep a real enum type to switch over exhaustively. Nothing here
// stores or transitions state. This file is deleted once the last of those methods is ported;
// do not add anything to it.

// The state of a thread has two axes. One is its arc from birth to
// death, expressed via its Lifecycle value. The other its current
// exception handler state, expressed via its Exception value.

// The state machine exists to encapsulate all legal state transitions, so
// generally other assertions about the state of a thread prior to
// transitioning are not necessary.

// Only RUNNING, SUSPENDED, or DYING threads may participate in
// exception handling.

class ThreadState {
 public:
  // The only legal transition that isn't from top-to-bottom occurs
  // when a thread is resumed after being suspended.
  enum class Lifecycle {
    // The ThreadDispatcher has been allocated, but not yet
    // associated to a Thread or an aspace.
    INITIAL,

    // The ThreadDispatcher is now associated to its underlying
    // Thread and the containing process's address space, and is
    // waiting to be run.
    INITIALIZED,

    // The thread is running.
    RUNNING,

    // The thread is currently suspended.
    // Note that suspension is orthogonal to being "in an exception".
    // A thread may be both suspended and in an exception, and the thread
    // does not "resume" execution until it is resumed from both the
    // suspension and the exception.
    SUSPENDED,

    // The thread is going to die. It may still be interacting
    // with exception handling state.
    DYING,

    // The thread is being dissociated from all of its state, and
    // no more interaction with userspace (including exception
    // handlers) is possible.
    DEAD,
  };

  // IDLE threads become UNPROCESSED. UNPROCESSED threads are told
  // to either RESUME or TRY_NEXT in a loop until they are killed
  // (no more exception handlers) or resumed, in which case they
  // become IDLE again.
  enum class Exception {
    // There's no pending exception.
    IDLE,

    // The thread is waiting for the pending exception to be
    // processed.
    UNPROCESSED,

    // The exception has been processed, and the next exception
    // handler should be queried.
    TRY_NEXT,

    // The exception has been processed, and the thread should
    // resume.
    RESUME,
  };
};

const char* ThreadLifecycleToString(ThreadState::Lifecycle lifecycle);

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_THREAD_STATE_H_

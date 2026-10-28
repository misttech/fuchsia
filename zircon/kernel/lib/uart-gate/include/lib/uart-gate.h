// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_UART_GATE_INCLUDE_LIB_UART_GATE_H_
#define ZIRCON_KERNEL_LIB_UART_GATE_INCLUDE_LIB_UART_GATE_H_

#include <assert.h>
#include <stdint.h>
#include <zircon/types.h>

#include <kernel/deadline.h>
#include <kernel/event.h>
#include <ktl/atomic.h>

namespace internal {

// Equivalent to `value.compare_exchange_weak(expected, desired,
// ktl::memory_order_seq_cst)`: returns true if |value| was |expected| and was
// replaced with |desired|.  Otherwise, returns false and sets |expected| to the
// value observed.  May fail spuriously (with |expected| unchanged).
//
// This exists because, when optimizing for arm64 without LSE atomics or
// outline atomics, LLVM expands compare_exchange_weak to an LL/SC loop in IR,
// so register allocation may place other instructions, e.g. stack spills,
// between the load-exclusive and the store-exclusive.  The Arm architecture
// only guarantees forward progress when there are no explicit memory accesses
// in between.  On Nelson, a spill there made the store-exclusive in the UART
// IRQ handler fail indefinitely, wedging a CPU in the handler (see
// https://fxbug.dev/575797059).  So on arm64, this is a single exclusive
// attempt written in assembly, with only a comparison between the LDAXR and
// the STLXR.  LDAXR/STLXR is the standard mapping of a seq_cst
// read-modify-write.  On failure, the LDAXR is a load-acquire which, like the
// LDAR used for a seq_cst load, is not reordered with a preceding
// store-release.  (Other architectures are unaffected: x86 uses a single
// instruction, and LLVM expands RISC-V LR/SC loops after register allocation.)
//
// TODO(575797059): Remove when a more general fix is accepted.
inline bool CompareExchangeWeakSeqCst(ktl::atomic<uint32_t>& value, uint32_t& expected,
                                      uint32_t desired) {
#if defined(__aarch64__)
  uint32_t observed;
  // The store-exclusive's status (0 if it succeeded), or 1 on a mismatch.
  uint32_t status;
  __asm__ volatile(
      "ldaxr %w[observed], [%[value]]\n"
      "cmp %w[observed], %w[expected]\n"
      "b.ne 1f\n"
      "stlxr %w[status], %w[desired], [%[value]]\n"
      "b 2f\n"
      "1:\n"
      "clrex\n"
      "mov %w[status], #1\n"
      "2:\n"
      : [observed] "=&r"(observed), [status] "=&r"(status)
      : [value] "r"(&value), [expected] "r"(expected), [desired] "r"(desired)
      : "cc", "memory");
  if (status == 0) {
    return true;
  }
  // On a spurious failure, |observed| is equal to |expected|.
  expected = observed;
  return false;
#else   // !defined(__aarch64__)
  return value.compare_exchange_weak(expected, desired, ktl::memory_order_seq_cst);
#endif  // defined(__aarch64__)
}

}  // namespace internal

// UartGate arbitrates access to the UART hardware so that usermode may suspend
// (and later resume) all kernel access to the hardware via `zx_debug_suspend`
// and `zx_debug_resume`, e.g. so that it may power down the UART.
//
// Every code path which touches the UART hardware must first successfully
// "enter" the gate, and then "exit" it when it is finished.  The gate's state
// is a single atomic word (a phase plus a count of active users), so entering
// (and `ExitWithoutWaking`) may be used from any context, including interrupt
// handlers, while holding spinlocks, and during a panic.  `Exit` may also wake
// the suspending thread, so it may only be used from contexts in which it is
// safe to signal an Event.
//
// The gate has three phases:
//
//  - kRunning: Normal operation.  Anyone may enter.
//  - kDraining: A suspend operation is in progress.  New operations are
//    refused, but operations which are required to allow in-flight operations
//    to complete (e.g. the UART IRQ handler) may still enter.  The suspending
//    thread waits (see `WaitForNoActiveUsers`) for the active count to drop to
//    zero, and then moves the gate to kSuspended.  Alternatively, the
//    suspending thread may abandon the suspend operation (e.g. because it was
//    interrupted), moving the gate back to kRunning.
//  - kSuspended: No one may enter.  Only the thread performing a suspend or
//    resume operation (serialized by UartSuspendResumeLock) may touch the
//    hardware.
class UartGate {
 public:
  enum class Phase : uint32_t {
    kRunning = 0,
    kDraining = 1,
    kSuspended = 2,
  };

  // Enter the gate for a normal operation.  Fails unless the gate is running.
  bool TryEnter() {
    return TryEnterWhile([](Phase p) { return p == Phase::kRunning; });
  }

  // Enter the gate for an operation which may be needed to allow in-flight
  // operations to complete.  Fails only if the gate is suspended.
  bool TryEnterUnlessSuspended() {
    return TryEnterWhile([](Phase p) { return p != Phase::kSuspended; });
  }

  // Exit the gate, waking the thread blocked in `WaitForNoActiveUsers` (if
  // any) if the caller was the last active user while the gate was draining.
  //
  // Must only be called from contexts in which it is safe to signal an Event:
  // thread context, or interrupt context (hard IRQ or timer callback), with no
  // ChainLockTransaction active and without holding any spinlock which may be
  // acquired while holding a chainlock.
  void Exit() {
    if (ExitInternal()) {
      drained_.Signal();
    }
  }

  // Exit the gate without waking the thread blocked in `WaitForNoActiveUsers`.
  // May be called from any context.
  //
  // If the caller is the last active user to exit while a thread is blocked in
  // `WaitForNoActiveUsers`, that thread will not notice until its next poll
  // (see `kDrainPollInterval`).
  void ExitWithoutWaking() { static_cast<void>(ExitInternal()); }

  // The maximum amount of time `WaitForNoActiveUsers` waits between checks for
  // active users.  Bounds the latency added when the last active user exits via
  // `ExitWithoutWaking`.
  static constexpr zx_duration_mono_t kDrainPollInterval = ZX_MSEC(10);

  // Waits, interruptibly, until there are no active users.  Must only be called
  // by the suspend/resume owner while the gate is draining.
  //
  // Operations which may enter while draining (see `TryEnterUnlessSuspended`)
  // may do so at any time, so there may be active users again by the time this
  // returns.
  //
  // Returns ZX_OK, or ZX_ERR_INTERNAL_INTR_KILLED or ZX_ERR_INTERNAL_INTR_RETRY
  // if the calling thread was killed or suspended while waiting.
  zx_status_t WaitForNoActiveUsers() {
    DEBUG_ASSERT(phase() == Phase::kDraining);
    // Normally, the last active user to exit wakes us via `Exit`.  However,
    // some users exit via `ExitWithoutWaking`, so bound each wait and check
    // again.  A stale signal (e.g. from an abandoned suspend operation, or from
    // a user which entered and exited while draining) merely causes us to check
    // again.
    while (HasActiveUsers()) {
      const zx_status_t status = drained_.Wait(Deadline::after_mono(kDrainPollInterval));
      if (status != ZX_OK && status != ZX_ERR_TIMED_OUT) {
        return status;
      }
    }
    return ZX_OK;
  }

  Phase phase() const { return PhaseOf(state_.load(ktl::memory_order_acquire)); }

  // Returns true if there are any operations currently inside the gate.
  bool HasActiveUsers() const { return (state_.load(ktl::memory_order_acquire) & kCountMask) != 0; }

  // Move from kRunning to kDraining.  Must only be called by the
  // suspend/resume owner while the gate is running.
  //
  // This synchronizes with every successful entry which precedes it, so
  // anything which happened before such an entry also happens before the
  // caller's subsequent operations.
  void BeginDrain() {
    [[maybe_unused]] const uint32_t prev =
        state_.fetch_add(MakeState(Phase::kDraining, 0), ktl::memory_order_acq_rel);
    DEBUG_ASSERT(PhaseOf(prev) == Phase::kRunning);
  }

  // Move from kDraining back to kRunning, abandoning a suspend operation.  Must
  // only be called by the suspend/resume owner while the gate is draining.
  // Operations which are still inside the gate are unaffected.
  //
  // Sequentially consistent; see `TryEnterWhile`.
  void CancelDrain() {
    [[maybe_unused]] const uint32_t prev =
        state_.fetch_sub(MakeState(Phase::kDraining, 0), ktl::memory_order_seq_cst);
    DEBUG_ASSERT(PhaseOf(prev) == Phase::kDraining);
  }

  // Attempt to move from kDraining to kSuspended.  Succeeds only if there are
  // no active users.
  bool TryFinishDrain() {
    uint32_t expected = MakeState(Phase::kDraining, 0);
    return state_.compare_exchange_strong(expected, MakeState(Phase::kSuspended, 0),
                                          ktl::memory_order_acquire, ktl::memory_order_relaxed);
  }

  // Move from kSuspended to kRunning.  Must only be called by the
  // suspend/resume owner while the gate is suspended.
  //
  // Sequentially consistent; see `TryEnterWhile`.
  void Resume() {
    DEBUG_ASSERT(state_.load(ktl::memory_order_relaxed) == MakeState(Phase::kSuspended, 0));
    state_.store(MakeState(Phase::kRunning, 0), ktl::memory_order_seq_cst);
  }

 private:
  static constexpr uint32_t kPhaseShift = 30;
  static constexpr uint32_t kCountMask = (1u << kPhaseShift) - 1;

  static constexpr uint32_t MakeState(Phase phase, uint32_t count) {
    return (static_cast<uint32_t>(phase) << kPhaseShift) | count;
  }
  static constexpr Phase PhaseOf(uint32_t state) {
    return static_cast<Phase>(state >> kPhaseShift);
  }

  template <typename Predicate>
  bool TryEnterWhile(Predicate allowed) {
    // Entry, and the operations which reopen the gate (`Resume` and
    // `CancelDrain`), are sequentially consistent.  This allows callers to
    // reliably hand off work using the pattern
    //
    //   caller:  flag.store(true, seq_cst); if (TryEnter()) { consume flag }
    //   owner:   Resume(); if (TryEnter()) { consume flag }
    //
    // (where consuming the flag is a seq_cst exchange), guaranteeing that at
    // least one of them consumes it.  With weaker orderings, the caller could
    // observe the gate closed while the owner observes the flag unset.
    //
    // A refused caller also observes everything which happened before the
    // phase change that caused it to be refused (e.g. `gUartResumedEvent` being
    // unsignaled).
    uint32_t state = state_.load(ktl::memory_order_seq_cst);
    do {
      if (!allowed(PhaseOf(state))) {
        return false;
      }
      DEBUG_ASSERT((state & kCountMask) != kCountMask);
    } while (!internal::CompareExchangeWeakSeqCst(state_, state, state + 1));
    return true;
  }

  // Returns true if the caller was the last active user while the gate was
  // draining.
  bool ExitInternal() {
    const uint32_t prev = state_.fetch_sub(1, ktl::memory_order_release);
    DEBUG_ASSERT((prev & kCountMask) != 0);
    return prev == MakeState(Phase::kDraining, 1);
  }

  // Allows tests to observe |drained_|.
  friend struct UartGateTestAccess;

  ktl::atomic<uint32_t> state_{MakeState(Phase::kRunning, 0)};

  // Signaled by `Exit` when the last active user exits while draining.  This
  // is an AutounsignalEvent so that both signaling (Signal) and consuming a
  // signal (Wait) are serialized by the event's lock.
  AutounsignalEvent drained_;
};

#endif  // ZIRCON_KERNEL_LIB_UART_GATE_INCLUDE_LIB_UART_GATE_H_

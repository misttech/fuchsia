// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_SUSPEND_WAKEUP_TIMER_INCLUDE_LIB_SUSPEND_WAKEUP_TIMER_H_
#define ZIRCON_KERNEL_LIB_SUSPEND_WAKEUP_TIMER_INCLUDE_LIB_SUSPEND_WAKEUP_TIMER_H_

#include <assert.h>
#include <lib/fit/function.h>
#include <stddef.h>
#include <zircon/time.h>

#include <fbl/alloc_checker.h>
#include <fbl/macros.h>
#include <kernel/ffi.h>
#include <ktl/unique_ptr.h>
#include <ktl/utility.h>

#if SCHEDULER_LOCK_SPIN_TRACING_ENABLED
inline constexpr size_t kSuspendWakeupTimerSpinlockSize = 24;
#elifdef WITH_LOCK_DEP
inline constexpr size_t kSuspendWakeupTimerSpinlockSize = 16;
#else
inline constexpr size_t kSuspendWakeupTimerSpinlockSize = 8;
#endif
// The Rust backend selector is zero-sized unless the ARMv7 MMIO timer backend exists.
#ifdef __aarch64__
inline constexpr size_t kSuspendWakeupTimerBackendSize = 8;
#else
inline constexpr size_t kSuspendWakeupTimerBackendSize = 0;
#endif
inline constexpr size_t kSuspendWakeupTimerStorageSize =
    kSuspendWakeupTimerSpinlockSize + kSuspendWakeupTimerBackendSize + 128;
inline constexpr size_t kSuspendWakeupTimerStorageAlign = 8;

// # SuspendWakeupTimer
//
// ## Summary
//
// A class which helps to abstract the specific timer HW used to wake the system
// from a suspended state.  Currently meant to be used only by the
// IdlePowerThread.
//
// ## Creation
// Users of the SuspendWakeupTimer start by creating a timer instance using the
// static Create factory function.  During creation, a Callback instance
// must be passed.
//
// The Callback is a fit::inline_function with enough storage for exactly one
// pointer, allowing it to easily target either a static or instanced method of
// a class.  It takes two parameters, now and resume_at, both timestamps on
// the boot timeline which inform the callback owner of when the interrupt
// fired, as well as what the originally requested resume time was.
//
// The lifecycle of the SuspendWakeupTimer is controlled by a unique_ptr
// returned from the Create function.  Create must not be called earlier
// than LK_INIT_LEVEL_PLATFORM to allow platform specific wakeup timer
// hardware to be detected and initialized.  Callbacks will always take place on
// the BOOT_CPU and with interrupts off.
//
// ## Usage
//
// Usage of the SuspendWakeupTimer involves three methods called in accordance
// with a protocol which must be followed to ensure correct behavior.  The
// methods are:
//
// + SetResumeDeadline
// + CancelTimer
// + EnsureStarted
//
// At the start of a suspend operation, if the operation has a resume deadline,
// SetResumeDeadline must be called to configure the desired deadline before
// instructing the BOOT_CPU's IdlePowerThread to transition to the suspended
// state.  Once a resume deadline has been configured, it is illegal to call
// SetResumeDeadline again without first explicitly canceling the timer via
// CancelTimer.
//
// After the resume deadline has been configured, and as the suspend coordinator
// thread unwinds from the suspend operation for any reason (resume timer fired,
// some other wake source fired, failure to enter suspend due to an unrelated
// error), CancelTimer *must* be called in order to reset the timer, and
// guarantee that there are no timer callbacks in flight.  CancelTimer is
// idempotent.  Unlike SetResumeDeadline, it may be called any number of times
// in a row.  Once CancelTimer has been called, it is guaranteed that there are
// no longer any callbacks in flight, however it does *not* guarantee that the
// user won the race to cancel a timer which had been set up, but may not have
// fired yet.
//
// Finally, in the BOOT_CPU's IdlePowerThread itself, when the system is
// supposed to be in the suspend state, it *must* call EnsureStarted to make
// certain that the hardware is properly set up to deliver an interrupt which
// will wake the system at an appropriate time.  This call *must* be made from
// the BOOT_CPU and interrupts *must* be disabled when this happens.  Similar to
// CancelTimer, EnsureStarted is idempotent and may be called multiple times
// without a problem.  If there is a configured deadline and the timer is not
// yet started, it will be.
//
// So, the summary of the usage protocol is as follows.
//
// 1) At the start of a suspend operation call SetResumeDeadline to configure
//    the time to wake up and resume the system.
// 2) In the BOOT_CPU's IdlePowerThread itself, just before entering a suspended
//    state and with interrupts still disabled, call EnsureStarted to make sure
//    that the timer is running if it needs to be.
// 3) After control is returned to the coordinator thread, always call
//    CancelTimer as we unwind.
//
// ## Restrictions
//
// SuspendWakeupTimer callbacks will take place at hard IRQ time.  Operations
// which may need to block (such as holding a Mutex) are not allowed.  That
// said, no spinlocks are held during the callback itself.  It is safe for the
// callback to call methods on the SuspendWakeupTimer instance itself, however
// it is anticipated that there is little to no reason to do so.  Keep in mind,
// however, that it is illegal to call SetResumeDeadline a second time without
// having canceled the timer first.  If a callback _wanted_ to re-program its
// deadline during the operation itself, `CancelTimer` must still be called
// first to ensure that the timer is ready to be set up again.
//
class SuspendWakeupTimer {
 public:
  using Callback =
      fit::inline_function<void(zx_instant_boot_t now, zx_instant_boot_t resume_at), sizeof(void*)>;

  ~SuspendWakeupTimer();

  DISALLOW_COPY_ASSIGN_AND_MOVE(SuspendWakeupTimer);

  static ktl::unique_ptr<SuspendWakeupTimer> Create(Callback callback) {
    fbl::AllocChecker ac;
    ktl::unique_ptr<SuspendWakeupTimer> ret(new (&ac) SuspendWakeupTimer(ktl::move(callback)));
    ASSERT(ac.check());
    return ret;
  }

  // Sets the time at which the SuspendWakeupTimer should call the user's
  // callback in order to wake the system from suspend.  Note that once this has
  // been called and the timer has an assigned deadline, it is illegal to call
  // this method again without first calling CancelTimer.
  void SetResumeDeadline(zx_instant_boot_t resume_at);

  // Called by the BOOT_CPU's IdlePowerThread every time it is just about to
  // enter a suspended state to ensure that its SuspendWakeupTimer is started
  // and will wake it up at the appropriate time.
  void EnsureStarted();

  // Called to cancel any pending timer and its callback.  Must be called at
  // least once after every call to SetResumeDeadline before a second call to
  // SetResumeDeadline may be made.  After returning from CancelTimer, the user
  // is guaranteed that there the registered callback is no longer in flight.
  // It has either completed, or was successfully canceled.
  void CancelTimer();

 private:
  explicit SuspendWakeupTimer(Callback callback);

  OpaqueStorage<kSuspendWakeupTimerStorageSize, kSuspendWakeupTimerStorageAlign> storage_{};
};

extern "C" {

void rust_suspend_wakeup_timer_init(void* storage, SuspendWakeupTimer::Callback* callback);
void rust_suspend_wakeup_timer_destroy(void* storage);
void rust_suspend_wakeup_timer_set_resume_deadline(void* storage, zx_instant_boot_t resume_at);
void rust_suspend_wakeup_timer_ensure_started(void* storage);
void rust_suspend_wakeup_timer_cancel_timer(void* storage);

}  // extern "C"

inline SuspendWakeupTimer::SuspendWakeupTimer(Callback callback) {
  rust_suspend_wakeup_timer_init(&storage_, &callback);
}

inline SuspendWakeupTimer::~SuspendWakeupTimer() { rust_suspend_wakeup_timer_destroy(&storage_); }

inline void SuspendWakeupTimer::SetResumeDeadline(zx_instant_boot_t resume_at) {
  rust_suspend_wakeup_timer_set_resume_deadline(&storage_, resume_at);
}

inline void SuspendWakeupTimer::EnsureStarted() {
  rust_suspend_wakeup_timer_ensure_started(&storage_);
}

inline void SuspendWakeupTimer::CancelTimer() { rust_suspend_wakeup_timer_cancel_timer(&storage_); }

#endif  // ZIRCON_KERNEL_LIB_SUSPEND_WAKEUP_TIMER_INCLUDE_LIB_SUSPEND_WAKEUP_TIMER_H_

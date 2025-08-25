// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_SUSPEND_WAKEUP_TIMER_INCLUDE_LIB_SUSPEND_WAKEUP_TIMER_H_
#define ZIRCON_KERNEL_LIB_SUSPEND_WAKEUP_TIMER_INCLUDE_LIB_SUSPEND_WAKEUP_TIMER_H_

#include <lib/relaxed_atomic.h>
#include <stdint.h>
#include <zircon/time.h>

#include <kernel/spinlock.h>
#include <ktl/unique_ptr.h>
#include <ktl/utility.h>

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
// fired yet.  (TODO(johngro): Make certain that this statement holds true when
// using the generic system timer in addition to the ARMv7 timers.)
//
// Finally, in the BOOT_CPU's IdlePowerThread itself, when the system is
// supposed to be in the suspend state, it *must* call EnsureStarted to make
// certain that the hardware is properly set up to deliver an interrupt which
// will wake the system at an appropriate time.  This call *must* be made from
// the BOOT_CPU and interrupts *must* be disabled when this happens.  Similar to
// CancelTimer, EnsureStarted is idempotent and may multiple times without a
// problem.  If there is a configured deadline and the timer is not yet started,
// it will be.
//
// So, the summary of the usage protocol is as follows.
//
// On the thread coordinating the suspend operation:
// 1) At the start of a suspend operation, configure the timer using a call to
//    SetResumeDeadline.
// 2) Tell the BOOT_CPU IdlePowerThread to enter the suspended state.
// 3) After control is returned, for any reason, call CancelTimer to set up for
//    the next suspend cycle.
//
// And, when interrupts are disabled, just before the BOOT_CPU's IdlePowerThread
// is about to suspend itself call EnsureStarted.
//
// ## Restrictions
//
// During execution of a user's registered callback, it is important that no
// methods of the callback's SuspendWakeupTimer be called.  Doing so risks
// deadlock.
//
class SuspendWakeupTimer {
 public:
  using Callback =
      fit::inline_function<void(zx_instant_boot_t now, zx_instant_boot_t resume_at), sizeof(void*)>;
  virtual ~SuspendWakeupTimer() = default;

  static ktl::unique_ptr<SuspendWakeupTimer> Create(Callback callback);

  // Sets the time at which the SuspendWakeupTimer should call the user's
  // callback in order to wake the system from suspend.  Note that once this has
  // been called and the timer has an assigned deadline, it is illegal to call
  // this method again without first calling CancelTimer.
  void SetResumeDeadline(zx_instant_boot_t resume_at) {
    Guard<SpinLock, IrqSave> timer_guard{&lock_};
    DEBUG_ASSERT(!started_);
    DEBUG_ASSERT(resume_at_ == ZX_TIME_INFINITE);
    resume_at_ = resume_at;
  }

  // Called by the BOOT_CPU's IdlePowerThread every time it is just about to
  // enter a suspended state to ensure that its SuspendWakeupTimer is started
  // and will wake it up at the appropriate time.
  virtual void EnsureStarted() = 0;

  // Called to cancel any pending timer and its callback.  Must be called at
  // least once after every call to SetResumeDeadline before a second call to
  // SetResumeDeadline may be made.  After returning from CancelTimer, the user
  // is guaranteed that there the registered callback is no longer in flight.
  // It has either completed, or was successfully canceled.
  virtual void CancelTimer() = 0;

 protected:
  SuspendWakeupTimer(Callback callback) : callback_(ktl::move(callback)) {}

  SuspendWakeupTimer(const SuspendWakeupTimer&) = delete;
  SuspendWakeupTimer& operator=(const SuspendWakeupTimer&) = delete;
  SuspendWakeupTimer(SuspendWakeupTimer&&) = delete;
  SuspendWakeupTimer& operator=(SuspendWakeupTimer&&) = delete;

  void ResetLocked() TA_REQ(lock_) {
    started_ = false;
    resume_at_ = ZX_TIME_INFINITE;
  }

  void DoCallback(zx_instant_boot_t now) {
    // It is important that we not hold any locks when calling our callback. Timer
    // implementations may be holding internal locks during their callback that
    // they also need to hold when we set them up.  We hold our `lock_` while
    // setting up our timer (which might hold an internal timer lock), so if we
    // attempt to hold it here, we might expose ourselves to an A/B deadlock.
    //
    // Why is it safe for us to read `resume_at_` then?  Isn't a formal data race?
    // In this case, it is OK because of the specifics of our API.  We define 3
    // operations:
    //
    // + SetResumeDeadline: holds the lock but will ASSERT if started_ is true.
    //   So, resume_at_ only changes if there is not a callback in flight.
    // + EnsureStarted: holds the lock and calls into our timer, but only if we are
    //   not already started (meaning there is no HW timer in flight).  Also, it
    //   never changes the value of resume_at_ (only examines it).
    // + Cancel: Holds the lock and resets both started_ and resume_at_, but only
    //   after canceling the underlying timer, which will guarantee that there is
    //   no callback in flight.
    //
    callback_(now, resume_at_);
  }

  DECLARE_SPINLOCK(SuspendWakeupTimer) lock_;
  bool started_ TA_GUARDED(lock_){false};
  RelaxedAtomic<zx_instant_boot_t> resume_at_{ZX_TIME_INFINITE};
  const Callback callback_;
};

#endif  // ZIRCON_KERNEL_LIB_SUSPEND_WAKEUP_TIMER_INCLUDE_LIB_SUSPEND_WAKEUP_TIMER_H_

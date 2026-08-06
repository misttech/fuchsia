// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_CAPTIVE_THREAD_INCLUDE_LIB_CAPTIVE_THREAD_CAPTIVE_THREAD_H_
#define SRC_LIB_CAPTIVE_THREAD_INCLUDE_LIB_CAPTIVE_THREAD_CAPTIVE_THREAD_H_

#include <lib/fit/function.h>
#include <lib/zx/channel.h>
#include <lib/zx/exception.h>
#include <lib/zx/result.h>
#include <lib/zx/thread.h>
#include <lib/zx/time.h>
#include <zircon/syscalls/debug.h>
#include <zircon/syscalls/exception.h>

#include <atomic>
#include <concepts>
#include <memory>
#include <ostream>
#include <thread>
#include <tuple>

namespace captive_thread {

// kTrapException is what __builtin_trap() produces.
constexpr zx_excp_type_t kTrapException =
#ifdef __aarch64__
    ZX_EXCP_SW_BREAKPOINT
#elif defined(__x86_64__) || defined(__riscv)
    ZX_EXCP_UNDEFINED_INSTRUCTION
#endif
    ;

constexpr uint64_t kTrapInstructionSize =
#if defined(__x86_64__) || defined(__riscv_c)
    2
#elif defined(__aarch64__) || defined(__riscv)
    4
#endif
    ;

// CaptiveThread is a large and immovable object.  To move one around, create
// it with std::make_unique<CaptiveThread>(...) and use it via std::unique_ptr.
//
// CaptiveThread is constructed just like a std::thread to launch a thread.
// Its ForceJoin() method forces the thread to exit before doing
// std::thread::join.  The thread is always joined this way on CaptiveThread
// destruction.
//
// Other methods provide for catching exceptions the thread hits or for
// suspending it asynchronously; and for easily accessing its register state
// while it's stopped for an exception or suspension.
class CaptiveThread {
 public:
  using Routine = fit::callback<void()>;

  CaptiveThread() noexcept = delete;
  CaptiveThread(const CaptiveThread&) = delete;
  CaptiveThread(CaptiveThread&& other) noexcept = delete;

  // Create a thread that runs the given function (can be move-only).
  explicit CaptiveThread(Routine);

  // The given function can take any kind of args that can be captured as
  // perfect forwards.
  template <typename F, typename... Args>
    requires(std::invocable<F, Args...> &&  // Anything not already coercible.
             !std::constructible_from<Routine, F, Args...>)
  explicit CaptiveThread(F f, Args&&... args)
      : CaptiveThread(Routine([f = std::move(f), ... args = std::forward<Args>(args)] mutable {
          std::move(f)(std::forward<Args>(args)...);
        })) {}

  // After destruction, the thread is guaranteed to be exited and joined.
  ~CaptiveThread() { ForceJoin(); }

  // If the thread is not already exiting, then force it to exit.  Then join
  // with it as in std::thread::join.  Other methods are not necessarily valid
  // after ForceJoin(), but it is always safe to call ForceJoin() again or to
  // call ForceJoin() after BlockUntilSuccess().
  void ForceJoin();

  // True if ForceJoin() or BlockUntilSuccess() has already been called.
  bool Joined() const { return !thread_handle_.is_valid(); }

  // Borrow the thread's kernel handle.  This handle is valid for the life of
  // the CaptiveThread object, even after the actual thread dies.
  zx::unowned_thread thread_handle() const { return thread_handle_.borrow(); }

  // Wait for the thread to get an exception or exit.  If this succeeds, then
  // either InException() is true, or the thread has exited.  If the thread is
  // suspended, this will wait until it resumes and hits an exception or exits.
  //
  // On success, the result value is just the `this` pointer.  This return
  // value is accepted by the <lib/captive-thread/testing/matchers.h> gmock
  // matchers for `EXPECT_THAT(thread.WaitForException(), ...);` use in tests.
  zx::result<CaptiveThread*> WaitForException(zx::time deadline = zx::time::infinite()) {
    return Wait(deadline, false);
  }

  // Request a thread suspension.  If it's already stopped in any fashion, this
  // returns immediate success.
  zx::result<> Suspend();

  // Wait for the thread to be stopped in any fashion.  If Suspend() hasn't
  // been called, then this is similar to WaitForException().
  zx::result<CaptiveThread*> WaitForStop(zx::time deadline = zx::time::infinite()) {
    return Wait(deadline, true);
  }

  // This presumes the thread will finish running the function and waits until
  // it has done so.  If the thread gets an exception, it will not be caught.
  // Must not be called when IsStopped().
  void BlockUntilSuccess();

  // Report if the thread is currently stopped in exception or suspension, or
  // has a suspension in progress.  If Suspend() has been called, then
  // InSuspend() and IsStopped() are true even if WaitForStop() is still needed
  // to actually synchronize and be able access registers, etc.
  bool InException() const { return exception_.is_valid(); }
  bool InSuspend() const { return suspend_.is_valid(); }
  bool IsStopped() const { return InException() || InSuspend(); }

  // Return the report for the exception, or std::nullopt if !InException().
  std::optional<zx_exception_report_t> ExceptionReport() const { return exception_report_; }

  // Resume and resolve the exception so no other handler will see it.  Must be
  // called when InException() is true.
  void ResolveException();

  // Resume from being stopped.  Must be called when IsStopped() is true.  When
  // InException(), this results in cascading to the next exception handler
  // (system crash service, etc.).
  void Resume();

  friend void PrintTo(const CaptiveThread&, std::ostream* os);

 private:
  void ResumeInternal();
  zx::result<CaptiveThread*> Wait(zx::time deadline, bool suspend_ok);

  std::atomic_int state_;
  zx::thread thread_handle_;
  zx::channel channel_;
  zx::exception exception_;
  zx::suspend_token suspend_;
  zx_thread_state_general_regs_t exit_regs_;
  std::optional<zx_exception_report_t> exception_report_;

  // Note this member is declared last so others are initialized first.
  std::thread thread_;
};
static_assert(!std::default_initializable<CaptiveThread>);
static_assert(!std::movable<CaptiveThread>);
static_assert(!std::copyable<CaptiveThread>);

constexpr uint64_t FaultAddress(const zx_exception_report_t& report) {
  const auto& arch_context = report.context.arch.u;
#ifdef __aarch64__
  return arch_context.arm_64.far;
#elifdef __riscv
  return arch_context.riscv_64.tval;
#elifdef __x86_64__
  return arch_context.x86_64.cr2;
#endif
}

}  // namespace captive_thread

#endif  // SRC_LIB_CAPTIVE_THREAD_INCLUDE_LIB_CAPTIVE_THREAD_CAPTIVE_THREAD_H_

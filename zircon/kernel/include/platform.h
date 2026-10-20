// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_PLATFORM_H_
#define ZIRCON_KERNEL_INCLUDE_PLATFORM_H_

#include <lib/zbi-format/reboot.h>
#include <lib/zx/result.h>
#include <sys/types.h>
#include <zircon/boot/crash-reason.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <dev/power.h>
#include <kernel/cpu.h>

#define BOOT_CPU_ID 0

typedef enum {
  HALT_ACTION_HALT = 0,           // Spin forever.
  HALT_ACTION_REBOOT,             // Reset the CPU.
  HALT_ACTION_REBOOT_BOOTLOADER,  // Reboot into the bootloader.
  HALT_ACTION_REBOOT_RECOVERY,    // Reboot into the recovery partition.
  HALT_ACTION_SHUTDOWN,           // Shutdown and power off.
} platform_halt_action;

// Holds platform-specific, per-cpu state used to suspend/resume a CPU.
struct PlatformCpuResumeState {
#if defined(__aarch64__)
  uint64_t cntkctl_el1{};
#endif
};

/* super early platform initialization, before almost everything */
void platform_early_init();

/* Perform any set up required before virtual memory is enabled, or the heap is set up. */
void platform_prevm_init();

/* later init, after the kernel has come up */
void platform_init();

/* platform_halt halts the system and performs the |suggested_action|.
 *
 * This function is used in both the graceful shutdown and panic paths so it
 * does not perform more complex actions like switching to the primary CPU,
 * unloading the run queue of secondary CPUs, stopping secondary CPUs, etc.
 *
 * There is no returning from this function.
 */
extern "C" void rust_platform_halt(platform_halt_action suggested_action,
                                   zircon_crash_reason_t reason) __NO_RETURN;

__NO_RETURN inline void platform_halt(platform_halt_action suggested_action,
                                      zircon_crash_reason_t reason) {
  rust_platform_halt(suggested_action, reason);
}

/* The platform specific actions to be taken in a halt situation.  This is a
 * weak symbol meant to be overloaded by platform specific implementations and
 * called from the common |platform_halt| implementation.  Do not call this
 * function directly, call |platform_halt| instead.
 *
 * There is no returning from this function.
 */
void platform_specific_halt(platform_halt_action suggested_action, zircon_crash_reason_t reason,
                            bool halt_on_panic) __NO_RETURN;
extern "C" void cpp_platform_specific_halt(platform_halt_action suggested_action,
                                           zircon_crash_reason_t reason,
                                           bool halt_on_panic) __NO_RETURN;

/* optionally stop the current cpu in a way the platform finds appropriate */
void platform_halt_cpu();
extern "C" void cpp_platform_halt_cpu();

// Returns true iff this platform supports |platform_suspend_cpu|.
bool platform_supports_suspend_cpu();

// Specifies whether this suspend operation is limited to the calling CPU or may
// power down the encapsulating power domain.
enum class PlatformAllowDomainPowerDown : bool { No, Yes };

// Suspend the calling CPU.
//
// On success, the CPU will enter an implementation-defined suspend state.
//
// If |allow_domain| is No, then the operation will only target the calling CPU.
// However, if Yes is specified, then, depending on the implementation, the
// power domain encapsulating the calling CPU may also be suspended and powered
// down.
//
// Prior to calling this function, it's critical to set up some kind of
// interrupt that will wake the CPU and resume execution.  Upon successful
// suspend and resume, this call returns ZX_OK.
//
// Prior to calling, interrupts must be disabled, preemption must be disabled,
// and the caller must be pinned to the calling CPU.
//
// The calling thread must be a kernel-only thread.  Because this routine saves
// only the minimum required register state, it is an error to call this
// function on a thread that has a "user half" (ThreadDispatcher).
//
// Possible error results include (but are not limited to),
//
// ZX_ERR_NOT_SUPPORTED if unsupported on this platform.
//
// ZX_ERR_ACCESS_DENIED or ZX_ERR_INVALID_ARGS if the suspend power state is
// incompatible with the current power state of other CPUs.  This is a somewhat
// normal error and must be handled by callers.
//
// TODO(https://fxbug.dev/417556024): Consider adding a platform-specific or
// custom result type to differentiates between success-but-did-not-power-down
// and powered-down.
//
// TODO(https://fxbug.dev/417558115): Consider checking the state of the
// secondary CPUs in order to determine which power state value to use so that
// we can eliminate the need to return ZX_ERR_ACCESS_DENIED or
// ZX_ERR_INVALID_ARGS in "normal" situations.
//
// TODO(https://fxbug.dev/417558115): Currently, the caller is responsible for
// determining if pausing/resuming the monotonic clock is necessary and then
// actually doing it.  More thought needs to be given to where that logic should
// live.  Right now, it's done in |IdlePowerThread::UpdateMonotonicClock|,
// however, it might be better to move some of that logic down into the platform
// layer (perhaps within |platform_suspend_cpu|).
zx_status_t platform_suspend_cpu(PlatformAllowDomainPowerDown allow_domain);

// Returns true iff this platform supports |platform_suspend_system|.
bool platform_supports_suspend_system();

// Suspend the entire system from the boot CPU after all secondary CPUs have
// been powered off.
//
// Prior to calling this function, it's critical to set up some kind of
// interrupt that will wake the boot CPU and resume execution.
//
// Prior to calling, all secondary CPUs must be powered off, interrupts must be
// disabled, preemption must be disabled, and the caller must be running on the
// boot CPU.
//
// The calling thread must be a kernel-only thread.  Because this routine saves
// only the minimum required register state, it is an error to call this
// function on a thread that has a "user half" (ThreadDispatcher).
//
// Returns ZX_OK on success.  Note that this does not necessarily mean that the
// system actually suspended (e.g. it may return immediately if an interrupt is
// pending).
//
// Returns ZX_ERR_NOT_SUPPORTED if unsupported on this platform.
//
// Returns ZX_ERR_BAD_STATE, without suspending, if any secondary CPU is online
// or the platform cannot confirm that every secondary CPU has powered off.
//
// May return other errors if the platform rejects the request.
zx_status_t platform_suspend_system();

// Returns true if this system has a debug serial port that is enabled
bool platform_serial_enabled();

// Suspend all kernel access to the debug UART hardware, on behalf of
// `zx_debug_suspend`.
//
// Upon successful return, the kernel will not touch the UART hardware (directly
// or indirectly) until `platform_serial_resume` has been called.  In particular:
//
//  - Blocking writes (`platform_dputs_thread`) write nothing and return
//    ZX_ERR_SHOULD_WAIT.  Callers may wait for the UART to be resumed using
//    `platform_serial_wait_for_resume`.
//  - Non-blocking writes (`platform_dputs_irq`, `platform_pputc`) are dropped.
//  - Polling reads (`platform_pgetc`) report no data.
//  - The UART interrupt is masked.
//
// Must be called from thread context.  Blocks, interruptibly, until any
// in-progress UART operations have completed and any output they queued in the
// hardware has been transmitted.  Transmission is detected via the driver's
// TxDrained method; drivers which do not implement it are assumed to have
// drained immediately, so with those drivers output still in the hardware FIFO
// may be lost.  This is normally brief, but may take indefinitely long if the
// hardware is unable to make progress (e.g. TX is stalled by flow control).  If
// the wait is interrupted, the suspend operation is abandoned and the UART is
// left running.  No spinlocks are held for long, and no lock is held while
// waiting, so concurrent callers are never blocked for long.
//
// While the suspend operation is in progress, `platform_serial_is_suspended`
// returns true, writers are refused (as though the UART were suspended), and
// concurrent calls to `platform_serial_suspend` and `platform_serial_resume`
// fail with ZX_ERR_BAD_STATE.
//
// Returns:
//  - ZX_OK on success.
//  - ZX_ERR_NOT_SUPPORTED if there is no debug UART, or if the debuglog is
//    being bypassed.
//  - ZX_ERR_BAD_STATE if the UART is already suspended, another suspend
//    operation is in progress, or the system is halting (see
//    `platform_serial_discard_writes_while_suspended`).
//  - ZX_ERR_INTERNAL_INTR_KILLED or ZX_ERR_INTERNAL_INTR_RETRY if the calling
//    thread was killed or suspended while waiting.  The UART is not suspended.
zx_status_t platform_serial_suspend();

// Resume kernel access to the debug UART hardware after a previous successful
// call to `platform_serial_suspend`.  The caller is responsible for ensuring the
// hardware is powered and clocked before calling.
//
// Must be called from thread context.  Does not block for long.
//
// Returns:
//  - ZX_OK on success.
//  - ZX_ERR_NOT_SUPPORTED if there is no debug UART, or if the debuglog is
//    being bypassed.
//  - ZX_ERR_BAD_STATE if the UART is not suspended (including while a suspend
//    operation is still in progress).
zx_status_t platform_serial_resume();

// Defined in <kernel/wait.h>.
enum class Interruptible : bool;

// Block until the debug UART is not suspended, or until blocking writes are
// being discarded (see `platform_serial_discard_writes_while_suspended`).
//
// Note that by the time this returns, the UART may have been suspended again,
// so callers should be prepared to retry.
//
// If |interruptible| is Interruptible::Yes, the wait may be interrupted by the
// calling thread being killed (returning ZX_ERR_INTERNAL_INTR_KILLED) or
// suspended (returning ZX_ERR_INTERNAL_INTR_RETRY).  Otherwise, returns ZX_OK.
//
// Must be called from thread context.
zx_status_t platform_serial_wait_for_resume(Interruptible interruptible);

// Cause blocking writes which cannot proceed because the debug UART is
// suspended to be discarded instead of blocking, and release any writers which
// are currently waiting.  This is permanent, and is intended to be called as
// the system halts (when the debuglog is shut down) so that a suspended UART
// cannot prevent the system from halting.  Has no effect on writes made while
// the UART is not suspended.  Once called, `platform_serial_suspend` fails with
// ZX_ERR_BAD_STATE.
//
// Must be called from thread context.  Does not block for long.  Safe to call
// concurrently with `platform_serial_suspend` and `platform_serial_resume`.
void platform_serial_discard_writes_while_suspended();

// Returns true if kernel access to the debug UART has been suspended (or a
// suspend operation is in progress) via `platform_serial_suspend`.
//
// Safe to call from any context, including during a panic.
bool platform_serial_is_suspended();
extern "C" bool cpp_platform_serial_is_suspended();

// Returns the boot time at which the debug UART was most recently suspended via
// `platform_serial_suspend`.  The value is only meaningful when
// `platform_serial_is_suspended` returns true.
//
// Safe to call from any context, including during a panic.
zx_instant_boot_t platform_serial_suspended_at();

// platform_panic_start informs the system that a panic message is about
// to be printed and that platform_halt will be called shortly.  The
// platform should stop other CPUs if requested and do whatever is necessary
// to safely ensure that the panic message will be visible to the user.
enum class PanicStartHaltOtherCpus { No = 0, Yes };
void platform_panic_start(PanicStartHaltOtherCpus option = PanicStartHaltOtherCpus::Yes);
extern "C" void cpp_platform_panic_start();

// Start the given cpu in a way the platform finds appropriate.
// 'context' contains an opaque value passed through to the new cpu for the
// bootstrap code that the cpu starts on. Most likely the context is a pointer
// to a data structure or the stack for the cpu to set up.
// NOTE: ARM specific at the moment with the inclusion of the notion of a mpid.
extern "C" zx_status_t platform_start_cpu(cpu_num_t cpu_id, uint64_t mpid, uint64_t context);

// Get the state of a CPU.
zx::result<power_cpu_state> platform_get_cpu_state(cpu_num_t cpu_id);

#endif  // ZIRCON_KERNEL_INCLUDE_PLATFORM_H_

// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <inttypes.h>
#include <lib/boot-options/boot-options.h>
#include <lib/cbuf.h>
#include <lib/console.h>
#include <lib/debuglog.h>
#include <lib/page/size.h>
#include <lib/root_resource_filter.h>
#include <lib/uart-gate.h>
#include <lib/uart/all.h>
#include <lib/uart/null.h>
#include <lib/uart/qemu.h>
#include <lib/uart/uart.h>
#include <lib/zbi-format/driver-config.h>
#include <lib/zircon-internal/macros.h>
#include <lib/zircon-internal/thread_annotations.h>
#include <lib/zx/time.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <zircon/errors.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <cassert>
#include <type_traits>

#include <arch/arch_interrupt.h>
#include <arch/interrupt.h>
#include <dev/init.h>
#include <dev/interrupt.h>
#include <kernel/deadline.h>
#include <kernel/event.h>
#include <kernel/lockdep.h>
#include <kernel/mutex.h>
#include <kernel/spinlock.h>
#include <kernel/thread.h>
#include <kernel/timer.h>
#include <ktl/algorithm.h>
#include <ktl/atomic.h>
#include <ktl/optional.h>
#include <ktl/type_traits.h>
#include <ktl/variant.h>
#include <lockdep/guard.h>
#include <platform/debug.h>
#include <platform/uart.h>

#include "platform.h"

namespace {

// No locks will be acquired.
struct NullLockPolicy {};

bool is_tx_irq_enabled = false;

template <typename LockPolicy, typename Guard>
using GuardSelector =
    std::conditional_t<std::is_same_v<LockPolicy, NullLockPolicy>, NullGuard, Guard>;

// Implements SyncPolicy as defined in <lib/uart/sync.h>
struct UartSyncPolicy {
  // While tempting, do *not* use a templated `using` statement here.  In some
  // places in code, we will need to deduce (via argument passing) what
  // `MemberOf` is.  Attempting to do this when we declare lock as a using alias
  // is not going to work in this situation.  The deduction is going to execute
  // against the underlying type, not the `using` statement.  So the final type
  // (in this case, a LockDep instance) would need to be a template with exactly
  // one argument which is identical to MemberOf.  In the case of LockDep locks,
  // this is *not* the case.
  //
  // Declaring this as a single-argument templated struct with no members which
  // inherits from the underlying LockDep type works around this issue.  Guards
  // and other things will auto-upcast to the proper underlying LockDep type,
  // and the only constructor needed/supported is the default constructor, so we
  // don't need to write a forwarding constructor to the underlying type.
  template <typename MemberOf>
  struct Lock : public DECLARE_SPINLOCK_WITH_TYPE(MemberOf, MonitoredSpinLock) {};

  template <typename LockPolicy>
  using Guard = GuardSelector<LockPolicy, Guard<MonitoredSpinLock, LockPolicy>>;

  class Waiter {
   public:
    enum class Blocking {
      // `Wait` is allowed to block callers, e.g. wait on an event.
      kYes,
      // `Wait` is not allowed to block callers, should spin instead.
      kNo,
    };

    template <typename Guard, typename T>
    void Wait(Guard& guard, T&& enable_tx_interrupt, Blocking blocking) TA_REQ(guard) {
      if (blocking == Blocking::kYes && is_tx_irq_enabled) {
        enable_tx_interrupt();
        guard.CallUnlocked([this]() { tx_fifo_not_full_.Wait(); });
      } else {
        // Drop the spinlock while spinning.
        guard.CallUnlocked([]() { arch::Yield(); });
      }
    }

    void Wake() { tx_fifo_not_full_.Signal(); }

   private:
    AutounsignalEvent tx_fifo_not_full_{true};
  };

  using DefaultLockPolicy = NullLockPolicy;

  template <typename LockType>
  static void AssertHeld(LockType& lock) TA_ASSERT(lock) TA_ASSERT(lock.lock()) {
    lock.lock().AssertHeld();
  }
};

// Guards all kernel access to the UART hardware so it can be suspended/resumed.
UartGate gUartGate;

class KernelIrqProvider {
 public:
  KernelIrqProvider() = default;
  ~KernelIrqProvider() = default;

  // No copy, no move.
  KernelIrqProvider(const KernelIrqProvider&) = delete;
  KernelIrqProvider& operator=(const KernelIrqProvider&) = delete;
  KernelIrqProvider(KernelIrqProvider&&) = delete;
  KernelIrqProvider& operator=(KernelIrqProvider&&) = delete;

  void Init(int irq_num) { irq_num_ = irq_num; }
  void SetInterruptsEnabled(bool enabled) {
    if (enabled) {
      // While kernel access to the UART is suspended, the IRQ must remain
      // masked; the IRQ handler refuses to touch the hardware, so a pending
      // (level-triggered) IRQ would otherwise fire repeatedly.  This matters
      // when `platform_serial_resume` calls the driver's `WakeupFromSuspend`
      // hook (which may attempt to unmask the IRQ) before it has reopened the
      // gate.  `platform_serial_resume` unmasks the IRQ itself once the gate has
      // been reopened.
      if (gUartGate.phase() == UartGate::Phase::kSuspended) {
        return;
      }
      unmask_interrupt(irq_num_);
    } else {
      mask_interrupt(irq_num_);
    }
  }

 private:
  int irq_num_{0};
};

uart::all::KernelDriver<PlatformUartIoProvider, UartSyncPolicy, KernelIrqProvider> gUart;

// Serializes the gate phase transitions (and associated hardware operations)
// made by platform_serial_suspend and platform_serial_resume.  This lock is
// only ever held briefly: it is never held while waiting for in-flight
// operations to drain, so that callers blocked acquiring it are never blocked
// indefinitely (Mutex acquisition is not interruptible).
DECLARE_SINGLETON_MUTEX(UartSuspendResumeLock);

// Signaled whenever the UART is not suspended (or when writes are being
// discarded, see below).  Threads performing blocking writes wait on this (see
// `platform_serial_wait_for_resume`) when they are unable to enter the gate.
Event gUartResumedEvent{true};

// When set, blocking writes which cannot proceed because the UART is suspended
// are discarded rather than blocking.  Set (permanently) when the debuglog is
// shut down as part of halting the system, so that a suspended UART cannot
// prevent the system from halting.  Once set, `platform_serial_suspend` refuses
// to start a new suspend operation.
//
// Only written while holding UartSuspendResumeLock, which serializes it with
// the suspending thread's check (and unsignaling of `gUartResumedEvent`).  Once
// set, `gUartResumedEvent` remains signaled.  May be read without the lock.
ktl::atomic<bool> gUartDiscardWhileSuspended{false};

// The boot time at which the UART was last suspended.  Reported in the crashlog
// if we panic while the UART is suspended.
ktl::atomic<zx_instant_boot_t> gUartSuspendedAt{0};

// Set when a reader drains the RX queue from full while the UART is suspended
// (and therefore could not re-enable the RX interrupt).  Consumed by whoever
// next manages to enter the gate (the reader itself, or the operation which
// reopens the gate: resume, or an abandoned suspend).
//
// All accesses are sequentially consistent, as are entering and reopening the
// gate (see UartGate::TryEnterWhile), so that if a reader is refused entry, the
// operation which later reopens the gate is guaranteed to observe the flag.
ktl::atomic<bool> gUartRxEnablePending{false};

// The UART's IRQ, if the UART is being driven by interrupts (as opposed to
// being polled).  Written once during UartDriverHandoffLate.
ktl::optional<uint32_t> gUartIrq;

// Re-enable the RX interrupt if a reader requested it while the UART was
// suspended.  The caller must have entered the gate.
void MaybeEnableRxInterrupt() {
  if (gUartRxEnablePending.exchange(false, ktl::memory_order_seq_cst)) {
    gUart.Visit([](auto& driver) { driver.template EnableRxInterrupt<IrqSave>(); });
  }
}

// Called by the suspend/resume owner just after reopening the gate (see
// gUartRxEnablePending).  Re-enables the RX interrupt on behalf of any reader
// which was refused entry while the gate was closed.
void EnableRxInterruptIfPendingAfterReopen() {
  if (gUartGate.TryEnter()) {
    MaybeEnableRxInterrupt();
    gUartGate.Exit();
  }
}

// Returns true if all output previously handed to the UART has been
// transmitted.  Does not wait.  Used by platform_serial_suspend while the gate
// is draining.
bool UartTxDrained() {
  bool drained = true;
  if (gUartGate.TryEnterUnlessSuspended()) {
    gUart.Visit([&drained](auto& driver) { drained = driver.template TxDrained<IrqSave>(); });
    // We're called by the suspending thread itself, so there's no one to wake.
    gUartGate.ExitWithoutWaking();
  }
  return drained;
}

// Enter the gate for non-blocking I/O which may be performed during a panic
// (output, and polling for panic shell input).
//
// Normally, such I/O is refused once a suspend operation has begun.  However,
// once the system has started to panic, it may also enter while a suspend
// operation is draining: the hardware is still powered (usermode has not yet
// been told that the suspend succeeded), and the suspending thread will never
// complete anyway, so observing us as an active user does not matter.
bool TryEnterForPanicIo() {
  return dlog_panic_started() ? gUartGate.TryEnterUnlessSuspended() : gUartGate.TryEnter();
}

// Initialized by UartInitLate, provides buffered output of the uart,
// which helps readers catch up.
// Also provides synchronization mechanisms for character availability.
Cbuf rx_queue;

// Size of the rx queue. The bigger the buffer, the bigger the window for
// the reader to catch up. Useful when the incoming data is bursty.
constexpr size_t kRxQueueSize = 1024;

// When Polling is enabled, this will fire the polling callback for draining UART's RX Queue.
Timer gUartPollTimer;

constexpr zx_duration_mono_t kPollingPeriod = ZX_MSEC(10);
constexpr TimerSlack kPollingSlack = {ZX_MSEC(10), TIMER_SLACK_CENTER};

// Callback used by |gUartPollTimer| when deadline is met. See |Timer| interface for more
// information.
template <bool DrainUart>
void UartPoll(Timer* uart_timer, zx_instant_mono_t now, void* arg) {
  uart_timer->Set(Deadline(zx_time_add_duration(now, kPollingPeriod), kPollingSlack),
                  &UartPoll<true>, nullptr);
  if constexpr (DrainUart) {
    // If the UART is suspended, skip this poll, but leave the timer running so
    // that we resume polling once the UART has been resumed.
    if (!gUartGate.TryEnter()) {
      return;
    }
    gUart.Visit([&](auto& driver) {
      // Drain until there is nothing else in the RX Queue of the device.
      while (auto c = driver.Read()) {
        rx_queue.WriteChar(*c);
      }
    });
    // Timer callbacks may signal events.
    gUartGate.Exit();
  }
}

struct IrqConfig {
  interrupt_trigger_mode trigger;
  interrupt_polarity polarity;
};

ktl::optional<IrqConfig> GetIrqConfigFromFlags(uint32_t uart_flags) {
  if (uart_flags == 0) {
    return ktl::nullopt;
  }

  if ((uart_flags & (ZBI_KERNEL_DRIVER_IRQ_FLAGS_LEVEL_TRIGGERED |
                     ZBI_KERNEL_DRIVER_IRQ_FLAGS_EDGE_TRIGGERED)) == 0) {
    return ktl::nullopt;
  }

  if ((uart_flags & (ZBI_KERNEL_DRIVER_IRQ_FLAGS_POLARITY_HIGH |
                     ZBI_KERNEL_DRIVER_IRQ_FLAGS_POLARITY_LOW)) == 0) {
    return ktl::nullopt;
  }

  // In order to configure the IRQ all information, trigger and polarity must be provided.
  // Otherwise the step must be omitted and let defaults take over.
  IrqConfig config = {};
  config.trigger = uart_flags & ZBI_KERNEL_DRIVER_IRQ_FLAGS_LEVEL_TRIGGERED
                       ? interrupt_trigger_mode::LEVEL
                       : interrupt_trigger_mode::EDGE;
  config.polarity = uart_flags & ZBI_KERNEL_DRIVER_IRQ_FLAGS_POLARITY_HIGH
                        ? interrupt_polarity::HIGH
                        : interrupt_polarity::LOW;
  return config;
}

}  // namespace

bool platform_serial_enabled(void) { return !!gUart; }

void platform_serial_prepare_for_suspend() {
  if (!platform_serial_enabled()) {
    return;
  }

  if (BootOptions::Get()->allow_debug_uart_suspend) {
    // If usermode has suspended the UART, the hardware has already been
    // prepared for suspend (and may be powered off).  Do not touch it.
    if (!gUartGate.TryEnterUnlessSuspended()) {
      return;
    }
    gUart.Visit([&]<typename DriverType>(DriverType& driver) { driver.PrepareForSuspend(); });
    // Called by the idle/power thread as it suspends the CPU, which must not
    // pend a preemption (see IdlePowerThread::Run), so do not signal.  The
    // bounded wait in UartGate::WaitForNoActiveUsers covers any suspending
    // thread.
    gUartGate.ExitWithoutWaking();
  }
}

void platform_serial_wakeup_from_suspend() {
  if (!platform_serial_enabled()) {
    return;
  }

  if (BootOptions::Get()->allow_debug_uart_suspend) {
    // If usermode has suspended the UART, leave the hardware alone.  It will be
    // woken up when usermode calls zx_debug_resume.
    if (!gUartGate.TryEnterUnlessSuspended()) {
      return;
    }
    gUart.Visit([&]<typename DriverType>(DriverType& driver) { driver.WakeupFromSuspend(); });
    // Called by the idle/power thread as it resumes the CPU, which must not
    // pend a preemption (see IdlePowerThread::Run), so do not signal.  The
    // bounded wait in UartGate::WaitForNoActiveUsers covers any suspending
    // thread.
    gUartGate.ExitWithoutWaking();
  }
}

namespace {

// Waits, interruptibly, until all output previously handed to the UART has been
// transmitted.  The gate must be draining with no writers still inside, so that
// no new output can be queued.
//
// UARTs generally do not raise an interrupt when the transmitter becomes
// completely empty (e.g. LSR.TEMT on the 8250), so we must poll.  Since no new
// output can be queued, this normally takes no longer than it takes to transmit
// a FIFO's worth of data.  We sleep between polls rather than spinning with the
// driver's lock held so that we never hold a spinlock (or disable interrupts)
// for long.
zx_status_t WaitForTxDrained() {
  constexpr zx_duration_mono_t kMinPollInterval = ZX_USEC(100);
  constexpr zx_duration_mono_t kMaxPollInterval = ZX_MSEC(10);
  zx_duration_mono_t poll_interval = kMinPollInterval;
  while (!UartTxDrained()) {
    if (zx_status_t status = Thread::Current::SleepInterruptible(
            zx_time_add_duration(current_mono_time(), poll_interval));
        status != ZX_OK) {
      return status;
    }
    poll_interval = ktl::min(poll_interval * 2, kMaxPollInterval);
  }
  return ZX_OK;
}

}  // namespace

zx_status_t platform_serial_suspend() {
  if (!platform_serial_enabled() || dlog_bypass()) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  {
    Guard<Mutex> guard{UartSuspendResumeLock::Get()};
    if (gUartGate.phase() != UartGate::Phase::kRunning) {
      return ZX_ERR_BAD_STATE;
    }

    // If the system is halting, refuse to suspend, since writers must never
    // block once writes are being discarded (see `gUartDiscardWhileSuspended`).
    // The flag is only set while holding UartSuspendResumeLock, so it cannot be
    // set until we have finished starting the suspend operation.
    if (gUartDiscardWhileSuspended.load(ktl::memory_order_relaxed)) {
      return ZX_ERR_BAD_STATE;
    }

    // Unsignal the event before we start refusing entry to the gate, so that
    // any blocking writer which is refused will block until we resume (or
    // cancel).
    gUartResumedEvent.Unsignal();
    // Record the suspend time before the gate starts reporting that the UART is
    // suspended, so that a crashlog never reports a stale time.
    gUartSuspendedAt.store(current_boot_time(), ktl::memory_order_relaxed);
    gUartGate.BeginDrain();
  }

  // Wait for any in-flight operations to finish, and then for any output they
  // queued in the hardware to be transmitted.  Note that we leave the UART IRQ
  // unmasked while we wait; writers which are blocked waiting for space in the
  // TX FIFO need the IRQ handler in order to make progress.
  //
  // Some operations exit the gate via `UartGate::ExitWithoutWaking` because they
  // run in contexts in which signaling an Event is unsafe; the bounded wait in
  // `UartGate::WaitForNoActiveUsers` covers them.
  //
  // In-flight operations and queued output normally drain quickly, but if the
  // hardware is unable to make progress (e.g. TX is stalled by flow control)
  // they may never drain.  So, wait interruptibly and without holding
  // UartSuspendResumeLock.  If we are interrupted, abandon the suspend
  // operation and leave the UART running.  While we wait, concurrent
  // suspend/resume calls fail with ZX_ERR_BAD_STATE.
  for (;;) {
    // Once there are no active users, no new output can be queued: writers are
    // refused entry while draining, and the operations which may still enter
    // (e.g. the IRQ handler) do not write.  So it is safe to wait for the TX
    // output to drain, and then to finish the drain.
    zx_status_t status = gUartGate.WaitForNoActiveUsers();
    if (status == ZX_OK) {
      status = WaitForTxDrained();
    }
    if (status != ZX_OK) {
      // We were killed or suspended.  Abandon the suspend operation and
      // release any writers which were refused entry while we were draining.
      Guard<Mutex> guard{UartSuspendResumeLock::Get()};
      gUartGate.CancelDrain();
      EnableRxInterruptIfPendingAfterReopen();
      gUartResumedEvent.Signal();
      return status;
    }

    // Hold the lock while finishing the drain and preparing the hardware so
    // that a concurrent resume (which requires the gate to be suspended) cannot
    // touch the hardware until we are done.
    Guard<Mutex> guard{UartSuspendResumeLock::Get()};
    bool finished;
    {
      // Mask the IRQ as soon as the gate is suspended.  Do this with interrupts
      // disabled on this CPU so that a pending level-triggered IRQ (which the
      // handler will now refuse to service) cannot prevent us from masking it.
      InterruptDisableGuard irqd;
      finished = gUartGate.TryFinishDrain();
      if (finished && gUartIrq.has_value()) {
        mask_interrupt(*gUartIrq);
      }
    }
    if (finished) {
      // At this point we are the only one who may touch the hardware.  Give the
      // driver a chance to quiesce the hardware before it loses power and/or
      // clocks.  Driver suspend hooks are required to complete quickly (see
      // uart::DriverBase::PrepareForSuspend).
      gUart.Visit([](auto& driver) { driver.template PrepareForSuspend<IrqSave>(); });
      return ZX_OK;
    }
    // Something (e.g. the IRQ handler) entered the gate in the meantime.  It
    // will wake us when it exits.  Try again.
  }
}

zx_status_t platform_serial_resume() {
  if (!platform_serial_enabled() || dlog_bypass()) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  Guard<Mutex> guard{UartSuspendResumeLock::Get()};
  if (gUartGate.phase() != UartGate::Phase::kSuspended) {
    return ZX_ERR_BAD_STATE;
  }

  // Restore the hardware while the gate is still closed.  Note that the driver
  // may attempt to unmask the UART IRQ as part of this.  `KernelIrqProvider`
  // suppresses that while the gate is suspended so that the IRQ cannot fire
  // (on any CPU) while the IRQ handler would still refuse to service it.
  gUart.Visit([](auto& driver) { driver.template WakeupFromSuspend<IrqSave>(); });

  {
    // Reopen the gate and then unmask the IRQ.  Keep interrupts disabled on
    // this CPU so that the IRQ stays masked for as short a time as possible
    // after the gate has been reopened (writers may need it to make progress).
    InterruptDisableGuard irqd;
    gUartGate.Resume();
    if (gUartIrq.has_value()) {
      unmask_interrupt(*gUartIrq);
    }
  }

  // If a reader drained the RX queue while we were suspended, re-enable the RX
  // interrupt on its behalf.
  EnableRxInterruptIfPendingAfterReopen();

  // Finally, release any blocked writers.
  gUartResumedEvent.Signal();

  return ZX_OK;
}

void platform_serial_discard_writes_while_suspended() {
  // UartSuspendResumeLock is only ever held briefly (never while waiting for a
  // suspend operation to drain), so acquiring it here cannot block the halt
  // path for long.
  Guard<Mutex> guard{UartSuspendResumeLock::Get()};
  gUartDiscardWhileSuspended.store(true, ktl::memory_order_relaxed);
  // Wake any blocked writers so that they observe the flag.
  gUartResumedEvent.Signal();
}

zx_status_t platform_serial_wait_for_resume(Interruptible interruptible) {
  if (interruptible == Interruptible::No) {
    return gUartResumedEvent.Wait();
  }
  return gUartResumedEvent.Wait(Deadline::infinite());
}

bool platform_serial_is_suspended() { return gUartGate.phase() != UartGate::Phase::kRunning; }

zx_instant_boot_t platform_serial_suspended_at() {
  return gUartSuspendedAt.load(ktl::memory_order_relaxed);
}

void UartDriverHandoffEarly(const uart::all::Driver& serial) {
  gUart = serial;
  if constexpr (DPRINTF_ENABLED_FOR_LEVEL(INFO)) {
    if (gUart) {
      ktl::array<char, 128> buffer = {};
      StringFile file{buffer};
      fprintf(&file, "UART: Selected driver kernel.serial=");
      gUart.Unparse(&file);
      dprintf(INFO, "%.*s\n", static_cast<int>(file.as_string_view().size()),
              file.as_string_view().data());
    }
  }
}

void UartDriverHandoffPostVm(const uart::all::Driver& serial) {}

void UartDriverHandoffLate(const uart::all::Driver& serial) {
  // This buffer is needed even when serial is disabled, to prevent uninitialized
  // access to it.
  rx_queue.Initialize(kRxQueueSize, malloc(kRxQueueSize));

  if (!platform_serial_enabled()) {
    return;
  }

  // Check for interrupt support or explicitly polling uart and reserve the IO regions, preventing
  // userspace from claiming them.
  ktl::optional<uint32_t> uart_irq;
  bool polling_mode = false;
  gUart.Visit([&]<typename DriverType>(DriverType& driver) {
    using uart_type = typename DriverType::uart_type;
    using cfg_type = typename uart_type::config_type;
    if constexpr (uart::MmioDriver<uart_type>) {
      uart_irq = PlatformUartGetIrqNumber(driver.config().irq);
      uart::MmioRange aligned_mmio_range = driver.mmio_range().AlignedTo(kPageSize);
      root_resource_filter_add_deny_region(aligned_mmio_range.address, aligned_mmio_range.size,
                                           ZX_RSRC_KIND_MMIO);
    } else if constexpr (uart::PioDriver<uart_type>) {
      uart_irq = PlatformUartGetIrqNumber(driver.config().irq);
      uart::IoPortRange ioports = driver.ioports();
      root_resource_filter_add_deny_region(ioports.base, ioports.count, ZX_RSRC_KIND_IOPORT);
    } else {  // Only |uart::null::Driver| is expected to have a different configuration type.
      constexpr auto kIsNullDriver = ktl::is_same_v<uart_type, uart::null::Driver>;
      ZX_ASSERT_MSG(kIsNullDriver, "Unexpected UART Configuration.");
      // No IRQ Handler for null driver.
      return;
    }

    // Check for polling mode.
    if (!uart_irq || BootOptions::Get()->debug_uart_poll) {
      // Start the polling without performing any drain.
      UartPoll</*DrainUart=*/false>(&gUartPollTimer, current_mono_time(), nullptr);
      dprintf(INFO, "UART: POLLING mode enabled.\n");
      polling_mode = true;
      return;
    }

    static constexpr auto rx_irq_handler = [](auto& rx_interrupt) {
      // This check needs to be performed under a lock, such that we prevent operation
      // interleaving that would leave us in a blocked state.
      //
      // E.g.
      // Assume a simple MT scenario with one reader R and one writer R:
      //
      // * W: Observes the buffer is full.
      // * R: Reads a character. The buffer is now empty.
      // * R: Unmasks RX.
      // * W: Masks RX.
      //
      //  At this point, we have an empty buffer and RX interrupts are masked -
      //  we're stuck! Thus, to avoid this, we acquire the spinlock before
      //  checking if the buffer is full, and release after (conditionally)
      //  masking RX interrupts. By pairing this with the acquisition of the
      //  same lock around unmasking RX interrupts, we prevent the writer above
      //  from being interrupted by a read-and-unmask.
      char c;
      {
        Guard<MonitoredSpinLock, NoIrqSave> lock(&rx_interrupt.lock(), SOURCE_TAG);
        if (rx_queue.Full()) {
          // disables RX interrupts.
          rx_interrupt.DisableInterrupt();
          return;
        }
        c = static_cast<char>(rx_interrupt.ReadChar());
      }
      rx_queue.WriteChar(c);
    };

    static constexpr auto tx_irq_handler = [](auto& tx_interrupt) {
      // Mask the TX interrupt before signalling any blocked thread as there may
      // be a race between masking TX here below and unmasking by the blocked
      // thread.
      {
        Guard<MonitoredSpinLock, NoIrqSave> lock(&tx_interrupt.lock(), SOURCE_TAG);
        tx_interrupt.DisableInterrupt();
      }

      // Do not signal the event while holding the sync capability, this could lead
      // to invalid lock dependencies.
      tx_interrupt.Notify();
    };

    if constexpr (ktl::is_same_v<cfg_type, zbi_dcfg_simple_t>) {
      // Configure the interrupt if available.
      auto irq_config = GetIrqConfigFromFlags(driver.config().flags);
      if (irq_config) {
        zx_status_t irq_config_result =
            configure_interrupt(*uart_irq, irq_config->trigger, irq_config->polarity);
        DEBUG_ASSERT(irq_config_result == ZX_OK);
      }
    }

    // Register IRQ Handler.
    auto irq_handler_helper = [](void* cookie) {
      // Once the UART has been suspended, we must not touch the hardware.  The
      // suspend operation masks the IRQ shortly after suspending the gate, but
      // we may still observe a spurious (or already pending) IRQ in the
      // meantime.
      //
      // Note that the handler is still permitted to run while a suspend
      // operation is draining in-flight operations, as writers blocked waiting
      // on TX FIFO space depend on it.
      if (!gUartGate.TryEnterUnlessSuspended()) {
        return;
      }
      auto* drv = static_cast<DriverType*>(cookie);
      drv->Interrupt(tx_irq_handler, rx_irq_handler);
      // IRQ handlers may signal events (see `tx_irq_handler`).
      gUartGate.Exit();
    };
    zx_status_t irq_register_result =
        register_permanent_int_handler(*uart_irq, interrupt_handler_t{&driver, irq_handler_helper});
    DEBUG_ASSERT(irq_register_result == ZX_OK);

    // Init UART Interrupts.
    driver.InitInterrupt(*uart_irq);
    gUartIrq = uart_irq;
  });

  if (!polling_mode) {
    dprintf(INFO, "UART: IRQ driven RX: enabled\n");

    is_tx_irq_enabled = !dlog_bypass();
    dprintf(INFO, "UART: IRQ driven TX: %s\n", is_tx_irq_enabled ? "enabled" : "disabled");
  }
}

zx_status_t platform_dputs_thread(const char* str, size_t len) {
  if (!platform_serial_enabled()) {
    return ZX_OK;
  }

  // If the UART has been suspended, tell the caller to wait for it to be
  // resumed (or, if the system is halting, discard the output).
  if (!gUartGate.TryEnter()) {
    return gUartDiscardWhileSuspended.load(ktl::memory_order_relaxed) ? ZX_OK : ZX_ERR_SHOULD_WAIT;
  }

  gUart.Visit([str, len](auto& driver) {
    driver.template Write<IrqSave>({str, len}, UartSyncPolicy::Waiter::Blocking::kYes);
  });

  gUartGate.Exit();
  return ZX_OK;
}

void platform_dputs_irq(const char* str, size_t len) {
  if (!platform_serial_enabled()) {
    return;
  }

  // We cannot block here.  If the UART has been suspended, drop the output.
  if (!TryEnterForPanicIo()) {
    return;
  }

  gUart.Visit([str, len](auto& driver) {
    driver.template Write<IrqSave>({str, len}, UartSyncPolicy::Waiter::Blocking::kNo);
  });

  // We may be called with a ChainLockTransaction active, or while holding
  // spinlocks, so we must not signal.
  gUartGate.ExitWithoutWaking();
}

int platform_dgetc(char* c, bool wait) {
  if (!platform_serial_enabled()) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  auto read = rx_queue.ReadCharWithContext(wait);

  // 1 => Character read.
  if (read.is_ok()) {
    // This is safe because:
    //   * The RX IRQ handler is holding the UART lock while the queue is being inspected (Full) and
    //     the RX IRQ is being disabled.
    //   * The Read path, which is the only path which can transition the queue from full to not
    //     full, is not holding the the UART lock while inspecting, but the operations is deferred
    //     and acquires the lock before enabling interrupts.
    //
    // As a consequence, the IRQ RX Interrupt cannot be enabled by this path, until the RX IRQ
    // Handler, has disabled it and released the lock. This means there is no possible interleaving,
    // where both paths observe full queue, and we enable the RX IRQ followed by the IRQ RX Handler
    // disabling them.
    if (read->transitioned_from_full) {
      // Record that the RX interrupt needs to be re-enabled.  If the UART is
      // suspended right now, we cannot touch the hardware, so the resume
      // operation will take care of it for us.  Otherwise, take care of it now.
      gUartRxEnablePending.store(true, ktl::memory_order_seq_cst);
      if (gUartGate.TryEnter()) {
        MaybeEnableRxInterrupt();
        gUartGate.Exit();
      }
    }
    *c = read->c;
    return 1;
  }

  // 0 => No character yet.
  if (read.status_value() == ZX_ERR_SHOULD_WAIT) {
    return 0;
  }

  // < 0 => Error.
  return read.status_value();
}

int platform_pgetc(char* c) {
  if (!platform_serial_enabled()) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  // If the UART is suspended, report that there is no data available.
  if (!TryEnterForPanicIo()) {
    return -1;
  }

  ktl::optional<char> read;
  gUart.Visit([&](auto& driver) { read = driver.Read(); });

  // Only used during a panic, so we must not signal.
  gUartGate.ExitWithoutWaking();

  if (read) {
    *c = *read;
    return 0;
  }

  return -1;
}

void platform_pputc(char c) {
  if (!platform_serial_enabled()) {
    return;
  }

  // We cannot block here.  If the UART has been suspended, drop the output.
  if (!TryEnterForPanicIo()) {
    return;
  }

  gUart.Visit([c](auto& driver) { driver.Write({&c, 1}, UartSyncPolicy::Waiter::Blocking::kNo); });

  // Only used during a panic, so we must not signal.
  gUartGate.ExitWithoutWaking();
}

namespace {

// Parses a duration of the form <NUM><UNIT>, where UNIT is one of s, ms, us, or
// ns (e.g. "500ms").  Returns ktl::nullopt if |str| is not a valid duration.
ktl::optional<zx_duration_mono_t> ParseDuration(const char* str) {
  char* end = nullptr;
  const unsigned long value = strtoul(str, &end, 10);
  if (end == str) {
    return ktl::nullopt;
  }
  const int64_t num = static_cast<int64_t>(value);
  if (!strcmp(end, "s")) {
    return ZX_SEC(num);
  }
  if (!strcmp(end, "ms")) {
    return ZX_MSEC(num);
  }
  if (!strcmp(end, "us")) {
    return ZX_USEC(num);
  }
  if (!strcmp(end, "ns")) {
    return ZX_NSEC(num);
  }
  return ktl::nullopt;
}

int cmd_serial(int argc, const cmd_args* argv, uint32_t flags) {
  auto usage = [&argv]() -> int {
    printf("usage:\n");
    printf(
        "%s suspend <NUM><s|ms|us|ns>  : suspend the debug UART, then resume it after the "
        "given duration\n",
        argv[0].str);
    return ZX_ERR_INTERNAL;
  };

  if (argc < 2) {
    return usage();
  }

  if (!strcmp(argv[1].str, "suspend")) {
    if (argc != 3) {
      return usage();
    }
    const ktl::optional<zx_duration_mono_t> duration = ParseDuration(argv[2].str);
    if (!duration) {
      printf("Invalid duration: %s\n", argv[2].str);
      return usage();
    }

    printf("Suspending the debug UART for %" PRId64 " ns\n", *duration);
    zx_status_t status = platform_serial_suspend();
    if (status != ZX_OK) {
      printf("platform_serial_suspend failed: %d\n", status);
      return status;
    }
    Thread::Current::SleepRelative(*duration);
    status = platform_serial_resume();
    if (status != ZX_OK) {
      printf("platform_serial_resume failed: %d\n", status);
      return status;
    }
    printf("Resumed the debug UART\n");
    return ZX_OK;
  }

  printf("unknown command\n");
  return usage();
}

}  // namespace

STATIC_COMMAND_START
STATIC_COMMAND_MASKED("serial", "debug serial port commands", &cmd_serial, CMD_AVAIL_NORMAL)
STATIC_COMMAND_END(serial)

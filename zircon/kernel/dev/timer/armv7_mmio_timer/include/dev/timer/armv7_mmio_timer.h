// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_TIMER_ARMV7_MMIO_TIMER_INCLUDE_DEV_TIMER_ARMV7_MMIO_TIMER_H_
#define ZIRCON_KERNEL_DEV_TIMER_ARMV7_MMIO_TIMER_INCLUDE_DEV_TIMER_ARMV7_MMIO_TIMER_H_

#include <lib/affine/ratio.h>
#include <lib/zbi-format/driver-config.h>
#include <lib/zx/result.h>
#include <stdint.h>

#include <dev/interrupt.h>
#include <kernel/cpu.h>
#include <ktl/type_traits.h>
#include <ktl/utility.h>

// # Armv7MmioTimer
//
// Armv7MmioTimer and its associated inner class, Armv7MmioTimer::Timer provide
// an interface to the memory mapped timer used described in the v7 ARM ARM.
//
// Armv7MmioTimer represents a block of timer registers, while
// Armv7MmioTimer::Timer represents the specific PCT and VCT based timers within
// that block, when available.
//
// ## Usage.
//
// ### Finding a timer.
//
// After initialization, which takes place just before LK_INIT_LEVEL_PLATFORM,
// users may find a timer to use with the Armv7MmioTimer::Get method.  There are
// a maximum of Armv7MmioTimer::kMaxTimers timer blocks in the system.  When a
// given block index is not detected or is otherwise unusable, the
// Armv7MmioTimer::Get method will return nullptr.
//
// Each block may contain a PCT based timer, a VCT based timer, or both.  They
// may be accessed via the pct_timer() and vct_timer() methods of a timer block
// instance, and are usable if their `supported()` members return true.
//
// ### Configuring a handler.
//
// In order to receive notification that a timer has fired, users must register
// an Armv7MmioTimer::IrqHandler with the timer via the Timer::SetHandler
// method.  This is a pointer-sized inline callback wrapper which takes no
// arguments and returns a Armv7MmioTimer::IrqHandlerResult.
//
// Only one handler can be registered at once, and attempts to register a second
// handler will return an error.  In order to change an existing handler for a
// Timer instance, users must first call ResetHandler.
//
// ### Using the timer.
//
// #### Programming.
//
// After configuring a handler, users may read the timer's current clock value
// via the `ticks()` method.  Additionally, the Timer's tick rate may be read
// from its parent timer block's `ticks_to_nsec()` method.  An absolute deadline
// may be programmed via the SetTimer method, which takes a deadline expressed
// in the timer's specific ticks timeline.  Alternatively, a relative timer may
// be set using the SetRelativeTimer helper method, which takes a deadline
// expressed in nanosecond units.
//
// #### Canceling.
//
// A programmed timer may be canceled at any time by calling the CancelTimer
// method.  While canceling a running timer is a race, the Timer instance
// guarantees that after CancelTimer has been called, any handlers will have
// been successfully canceled and will not run, or they will have already run to
// completion.  Calls to CancelTimer are idempotent.
//
// #### Handling a timer event.
//
// When a timer finally fires, a user's registered handler will be called.  This
// is done at hard IRQ time and with interrupts disabled.  Blocking is not
// allowed.  The timer always acts as a one-shot.  When the timer handler has
// been called, the timer is effectively canceled.  Users may safely reset their
// timer during their timer handler by calling either SetTimer or
// SetRelativeTimer. They are also free to cancel their timer using CancelTimer,
// however since timers are effectively already canceled at the start of the
// handler, this would only matter if they had already set the timer during the
// handler and suddenly wanted to cancel it.
//
// Users *may not* change their registered handler during their handler
// callback.  Any attempt to do so with produce an error.  That said, timer
// handlers may choose to stay registered after the handler has run by retuning
// IrqHandlerResult::RemainRegistered, or they have have their handler
// unregistered after completing by returning IrqHandlerResult::Unregister.  Care
// must be taken, however, as the destruction of the inline function will take
// place at IRQ time.  In order to safely unregister the handler at this time,
// the inline function must not hold and references to anything which would
// require blocking to destroy (such as a heap allocation).
//
class Armv7MmioTimer {
 public:
  class Timer;  // fwd decl
  enum class Type : int32_t { PCT = 0, VCT = 1 };
  enum class IrqHandlerResult : int32_t { RemainRegistered = 0, Unregister = 1 };

  class IrqHandler {
   public:
    IrqHandler() = default;
    IrqHandler(decltype(nullptr)) {}

    template <typename Callable,
              typename = ktl::enable_if_t<!ktl::is_same_v<ktl::decay_t<Callable>, IrqHandler> &&
                                          sizeof(ktl::decay_t<Callable>) <= sizeof(void*) &&
                                          alignof(ktl::decay_t<Callable>) <= alignof(void*) &&
                                          ktl::is_trivially_copyable_v<ktl::decay_t<Callable>> &&
                                          ktl::is_trivially_destructible_v<ktl::decay_t<Callable>>>>
    IrqHandler(Callable&& callable) {
      using Decayed = ktl::decay_t<Callable>;
      Decayed fn(ktl::forward<Callable>(callable));
      __builtin_memcpy(&cookie_, &fn, sizeof(Decayed));
      call_ = [](void* cookie) -> int32_t {
        alignas(Decayed) unsigned char storage[sizeof(Decayed)];
        __builtin_memcpy(storage, &cookie, sizeof(Decayed));
        return static_cast<int32_t>((*reinterpret_cast<Decayed*>(storage))());
      };
    }

    IrqHandler(const IrqHandler&) = delete;
    IrqHandler& operator=(const IrqHandler&) = delete;

    IrqHandler(IrqHandler&& other) noexcept
        : cookie_(other.cookie_), call_(other.call_), destroy_(other.destroy_) {
      other.cookie_ = nullptr;
      other.call_ = nullptr;
      other.destroy_ = nullptr;
    }

    IrqHandler& operator=(IrqHandler&& other) noexcept {
      if (this != &other) {
        Reset();
        cookie_ = other.cookie_;
        call_ = other.call_;
        destroy_ = other.destroy_;
        other.cookie_ = nullptr;
        other.call_ = nullptr;
        other.destroy_ = nullptr;
      }
      return *this;
    }

    ~IrqHandler() { Reset(); }

    void Reset() {
      if (destroy_ != nullptr) {
        destroy_(cookie_);
      }
      cookie_ = nullptr;
      call_ = nullptr;
      destroy_ = nullptr;
    }

   private:
    friend class Timer;
    void* cookie_{nullptr};
    int32_t (*call_)(void*){nullptr};
    void (*destroy_)(void*){nullptr};
  };

  struct Irq {
    uint32_t num;
    uint32_t zbi_flags;
  };

  static inline constexpr size_t kMaxTimers = 8;

  static void Init(const zbi_dcfg_arm_generic_timer_mmio_driver_t& config);
  static Armv7MmioTimer* Get(size_t ndx);

  // Console diags
  static int Dump(uint8_t timer_mask);
  static int ShowStatus(uint8_t timer_mask);

  Armv7MmioTimer() = delete;
  ~Armv7MmioTimer() = delete;
  Armv7MmioTimer(const Armv7MmioTimer&) = delete;
  Armv7MmioTimer& operator=(const Armv7MmioTimer&) = delete;

  affine::Ratio ticks_to_nsec() const;

  Timer& pct_timer();
  Timer& vct_timer();

  const Timer& pct_timer() const;
  const Timer& vct_timer() const;

  class Timer {
   public:
    Timer() = delete;
    ~Timer() = delete;
    Timer(const Timer&) = delete;
    Timer& operator=(const Timer&) = delete;

    bool supported() const;
    bool enabled() const;
    bool has_handler() const;
    uint32_t irq() const;
    Type type() const;
    const char* type_name() const;
    uint64_t ticks() const;

    zx_status_t SetHandler(IrqHandler handler, cpu_mask_t mask = cpu_num_to_mask(BOOT_CPU_ID));
    zx_status_t ResetHandler() { return SetHandler(IrqHandler{}); }
    zx_status_t CancelTimer();
    zx_status_t SetTimer(uint64_t ticks_deadline);
    zx_status_t SetRelativeTimer(zx_duration_t relative_timout);
    zx::result<zx_duration_t> TimeUntilDeadline() const;
  };
};

static_assert(sizeof(affine::Ratio) == 8 && alignof(affine::Ratio) == 4);

extern "C" {
void rust_armv7_mmio_timer_init(const zbi_dcfg_arm_generic_timer_mmio_driver_t* config);
Armv7MmioTimer* rust_armv7_mmio_timer_get(size_t ndx);
int rust_armv7_mmio_timer_dump(uint8_t timer_mask);
int rust_armv7_mmio_timer_show_status(uint8_t timer_mask);
const affine::Ratio* rust_armv7_mmio_timer_ticks_to_nsec(const Armv7MmioTimer* timer);
Armv7MmioTimer::Timer* rust_armv7_mmio_timer_pct_timer(const Armv7MmioTimer* timer);
Armv7MmioTimer::Timer* rust_armv7_mmio_timer_vct_timer(const Armv7MmioTimer* timer);

bool rust_armv7_mmio_timer_timer_supported(const Armv7MmioTimer::Timer* timer);
bool rust_armv7_mmio_timer_timer_enabled(const Armv7MmioTimer::Timer* timer);
bool rust_armv7_mmio_timer_timer_has_handler(const Armv7MmioTimer::Timer* timer);
uint32_t rust_armv7_mmio_timer_timer_irq(const Armv7MmioTimer::Timer* timer);
int32_t rust_armv7_mmio_timer_timer_type(const Armv7MmioTimer::Timer* timer);
const char* rust_armv7_mmio_timer_timer_type_name(const Armv7MmioTimer::Timer* timer);
uint64_t rust_armv7_mmio_timer_timer_ticks(const Armv7MmioTimer::Timer* timer);
zx_status_t rust_armv7_mmio_timer_timer_set_handler(const Armv7MmioTimer::Timer* timer,
                                                    void* cookie, int32_t (*call)(void*),
                                                    void (*destroy)(void*), cpu_mask_t mask);
zx_status_t rust_armv7_mmio_timer_timer_cancel_timer(const Armv7MmioTimer::Timer* timer);
zx_status_t rust_armv7_mmio_timer_timer_set_timer(const Armv7MmioTimer::Timer* timer,
                                                  uint64_t ticks_deadline);
zx_status_t rust_armv7_mmio_timer_timer_set_relative_timer(const Armv7MmioTimer::Timer* timer,
                                                           zx_duration_t relative_timeout);
zx_status_t rust_armv7_mmio_timer_timer_time_until_deadline(const Armv7MmioTimer::Timer* timer,
                                                            zx_duration_t* out_duration);
}

inline void Armv7MmioTimer::Init(const zbi_dcfg_arm_generic_timer_mmio_driver_t& config) {
  rust_armv7_mmio_timer_init(&config);
}

inline Armv7MmioTimer* Armv7MmioTimer::Get(size_t ndx) { return rust_armv7_mmio_timer_get(ndx); }

inline int Armv7MmioTimer::Dump(uint8_t timer_mask) {
  return rust_armv7_mmio_timer_dump(timer_mask);
}

inline int Armv7MmioTimer::ShowStatus(uint8_t timer_mask) {
  return rust_armv7_mmio_timer_show_status(timer_mask);
}

inline affine::Ratio Armv7MmioTimer::ticks_to_nsec() const {
  return *rust_armv7_mmio_timer_ticks_to_nsec(this);
}

inline Armv7MmioTimer::Timer& Armv7MmioTimer::pct_timer() {
  return *rust_armv7_mmio_timer_pct_timer(this);
}

inline Armv7MmioTimer::Timer& Armv7MmioTimer::vct_timer() {
  return *rust_armv7_mmio_timer_vct_timer(this);
}

inline const Armv7MmioTimer::Timer& Armv7MmioTimer::pct_timer() const {
  return *rust_armv7_mmio_timer_pct_timer(this);
}

inline const Armv7MmioTimer::Timer& Armv7MmioTimer::vct_timer() const {
  return *rust_armv7_mmio_timer_vct_timer(this);
}

inline bool Armv7MmioTimer::Timer::supported() const {
  return rust_armv7_mmio_timer_timer_supported(this);
}

inline bool Armv7MmioTimer::Timer::enabled() const {
  return rust_armv7_mmio_timer_timer_enabled(this);
}

inline bool Armv7MmioTimer::Timer::has_handler() const {
  return rust_armv7_mmio_timer_timer_has_handler(this);
}

inline uint32_t Armv7MmioTimer::Timer::irq() const { return rust_armv7_mmio_timer_timer_irq(this); }

inline Armv7MmioTimer::Type Armv7MmioTimer::Timer::type() const {
  return static_cast<Type>(rust_armv7_mmio_timer_timer_type(this));
}

inline const char* Armv7MmioTimer::Timer::type_name() const {
  return rust_armv7_mmio_timer_timer_type_name(this);
}

inline uint64_t Armv7MmioTimer::Timer::ticks() const {
  return rust_armv7_mmio_timer_timer_ticks(this);
}

inline zx_status_t Armv7MmioTimer::Timer::SetHandler(IrqHandler handler, cpu_mask_t mask) {
  void* cookie = handler.cookie_;
  auto call = handler.call_;
  auto destroy = handler.destroy_;
  handler.cookie_ = nullptr;
  handler.call_ = nullptr;
  handler.destroy_ = nullptr;
  return rust_armv7_mmio_timer_timer_set_handler(this, cookie, call, destroy, mask);
}

inline zx_status_t Armv7MmioTimer::Timer::CancelTimer() {
  return rust_armv7_mmio_timer_timer_cancel_timer(this);
}

inline zx_status_t Armv7MmioTimer::Timer::SetTimer(uint64_t ticks_deadline) {
  return rust_armv7_mmio_timer_timer_set_timer(this, ticks_deadline);
}

inline zx_status_t Armv7MmioTimer::Timer::SetRelativeTimer(zx_duration_t relative_timout) {
  return rust_armv7_mmio_timer_timer_set_relative_timer(this, relative_timout);
}

inline zx::result<zx_duration_t> Armv7MmioTimer::Timer::TimeUntilDeadline() const {
  zx_duration_t out_duration = 0;
  zx_status_t status = rust_armv7_mmio_timer_timer_time_until_deadline(this, &out_duration);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(out_duration);
}

#endif  // ZIRCON_KERNEL_DEV_TIMER_ARMV7_MMIO_TIMER_INCLUDE_DEV_TIMER_ARMV7_MMIO_TIMER_H_

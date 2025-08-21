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
#include <dev/timer/armv7_mmio_timer_registers.h>
#include <hwreg/mmio.h>
#include <ktl/array.h>
#include <ktl/optional.h>
#include <ktl/unique_ptr.h>

class Armv7MmioTimer {
 public:
  class Timer;  // fwd decl
  enum class Type { PCT, VCT };

  struct Irq {
    uint32_t num;
    uint32_t zbi_flags;
  };

  static inline constexpr size_t kMaxTimers = 8;

  static void Init(const zbi_dcfg_arm_generic_timer_mmio_driver_t& config);

  static Armv7MmioTimer* Get(size_t ndx) {
    return (ndx < timers_.size()) ? timers_[ndx].get() : nullptr;
  }

  // Console diags
  static int Dump(uint8_t timer_mask);
  static int ShowStatus(uint8_t timer_mask);

  Armv7MmioTimer(uint32_t frame_ndx, vaddr_t mmio, vaddr_t el0_mmio, Irq phys_irq, Irq virt_irq)
      : frame_ndx_(frame_ndx),
        mmio_{reinterpret_cast<volatile void*>(mmio)},
        el0_mmio_{reinterpret_cast<volatile void*>(el0_mmio)},
        pct_timer_{*this, Type::PCT, phys_irq},
        vct_timer_{*this, Type::VCT, virt_irq} {}

  ~Armv7MmioTimer() = default;

  affine::Ratio ticks_to_nsec() const { return ticks_to_nsec_; }

  Timer& pct_timer() { return pct_timer_; }
  Timer& vct_timer() { return vct_timer_; }

  const Timer& pct_timer() const { return pct_timer_; }
  const Timer& vct_timer() const { return vct_timer_; }

  class Timer {
   private:
    using CT = armv7_mmio_timer_registers::CNTPVRegs::CT;
    using CVAL = armv7_mmio_timer_registers::CNTPVRegs::CVAL;
    using TVAL = armv7_mmio_timer_registers::CNTPVRegs::TVAL;
    using CTL = armv7_mmio_timer_registers::CNTPVRegs::CTL;

   public:
    Timer(const Armv7MmioTimer& owner, Type type, Irq irq)
        : owner_(owner), type_(type), irq_(irq) {}

    void set_supported(bool value) { supported_ = value; }
    bool supported() const { return supported_; }
    bool enabled() const { return enabled_; }

    uint32_t irq() const { return irq_.num; }
    Type type() const { return type_; }
    const char* type_name() const { return type_ == Type::PCT ? "PCT" : "VCT"; }

    uint64_t ticks() const { return CT::Get().ReadFrom(&counter_mmio_).reg_value(); }

    void Setup();

    zx_status_t SetHandler(interrupt_handler_t handler,
                           cpu_mask_t mask = cpu_num_to_mask(BOOT_CPU_ID));
    zx_status_t CancelTimer() {
      Guard<SpinLock, IrqSave> guard(&irq_lock_);
      if (!supported_) {
        return ZX_ERR_NOT_SUPPORTED;
      }
      Disable();
      return ZX_OK;
    }
    zx_status_t SetTimer(uint64_t ticks_deadline);
    zx_status_t SetRelativeTimer(zx_duration_t relative_timout);

    zx::result<zx_duration_t> TimeUntilDeadline() const;

   private:
    void IrqHandler();

    void Disable() TA_REQ(irq_lock_) {
      CTL::Get().FromValue(0).set_ENABLE(0).set_IMASK(1).WriteTo(&timer_mmio_);
      enabled_ = false;
    }

    void Enable() TA_REQ(irq_lock_) {
      CTL::Get().FromValue(0).set_ENABLE(1).set_IMASK(0).WriteTo(&timer_mmio_);
      enabled_ = true;
    }

    const Armv7MmioTimer& owner_;
    mutable hwreg::RegisterMmio counter_mmio_{nullptr};
    mutable hwreg::RegisterMmio timer_mmio_{nullptr};

    const Type type_;
    const Irq irq_;
    bool supported_{false};
    bool enabled_{false};

    DECLARE_SPINLOCK(Timer) irq_lock_;
    TA_GUARDED(irq_lock_) interrupt_handler_t user_handler_ { nullptr };
  };

 private:
  void Setup();

  static inline hwreg::RegisterMmio mmio_ctl_{nullptr};
  static inline ktl::array<ktl::unique_ptr<Armv7MmioTimer>, kMaxTimers> timers_;

  const uint32_t frame_ndx_;
  mutable hwreg::RegisterMmio mmio_{nullptr};
  mutable hwreg::RegisterMmio el0_mmio_{nullptr};

  affine::Ratio ticks_to_nsec_;
  Timer pct_timer_;
  Timer vct_timer_;
};

#endif  // ZIRCON_KERNEL_DEV_TIMER_ARMV7_MMIO_TIMER_INCLUDE_DEV_TIMER_ARMV7_MMIO_TIMER_H_

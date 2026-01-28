// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/intrin.h>
#include <lib/console.h>
#include <lib/fit/function.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <kernel/spinlock.h>
#include <lk/init.h>
#include <pdev/interrupt.h>

#include <ktl/enforce.h>

namespace {

DECLARE_SINGLETON_SPINLOCK(pdev_lock);

struct int_handler_struct {
  interrupt_handler_t handler TA_GUARDED(pdev_lock::Get()) = nullptr;
  ktl::atomic<bool> permanent = false;
};

struct int_handler_struct int_handler_table[MAX_INTERRUPTS];

struct int_handler_struct* pdev_get_int_handler(interrupt_vector_t vector) {
  DEBUG_ASSERT(vector < MAX_INTERRUPTS);
  return &int_handler_table[vector];
}

zx_status_t register_int_handler_common(interrupt_vector_t vector, interrupt_handler_t handler,
                                        bool permanent) {
  if (!is_valid_interrupt(vector, 0)) {
    return ZX_ERR_INVALID_ARGS;
  }

  Guard<SpinLock, IrqSave> guard{pdev_lock::Get()};

  auto h = pdev_get_int_handler(vector);
  if ((handler && h->handler) || h->permanent.load(ktl::memory_order_relaxed)) {
    return ZX_ERR_ALREADY_BOUND;
  }
  h->handler = ktl::move(handler);
  h->permanent.store(permanent, ktl::memory_order_relaxed);

  return ZX_OK;
}

// By default most of these are empty stubs and the particular interrupt controller must override
// all of them.
const struct pdev_interrupt_ops default_ops = {
    .mask = [](interrupt_vector_t) { return ZX_ERR_NOT_SUPPORTED; },
    .unmask = [](interrupt_vector_t) { return ZX_ERR_NOT_SUPPORTED; },
    .deactivate = [](interrupt_vector_t) { return ZX_ERR_NOT_SUPPORTED; },
    .configure = [](interrupt_vector_t, interrupt_trigger_mode,
                    interrupt_polarity) { return ZX_ERR_NOT_SUPPORTED; },
    .get_config = [](interrupt_vector_t, interrupt_trigger_mode*,
                     interrupt_polarity*) { return ZX_ERR_NOT_SUPPORTED; },
    .set_affinity = [](interrupt_vector_t, cpu_mask_t) { return ZX_ERR_NOT_SUPPORTED; },
    .is_valid = [](interrupt_vector_t, uint32_t flags) { return false; },
    .get_base_vector = []() -> interrupt_vector_t { return 0; },
    .get_max_vector = []() -> interrupt_vector_t { return 0; },
    .remap = [](interrupt_vector_t) -> interrupt_vector_t { return 0; },
    .send_ipi = [](cpu_mask_t, mp_ipi) { return ZX_ERR_NOT_SUPPORTED; },
    .init_percpu_early = []() {},
    .init_percpu = []() {},
    .handle_irq = [](iframe_t*) {},
    .shutdown = []() {},
    .shutdown_cpu = []() {},
    .suspend_cpu = []() { return ZX_ERR_NOT_SUPPORTED; },
    .resume_cpu = []() { return ZX_ERR_NOT_SUPPORTED; },
    .msi_is_supported = []() { return false; },
    .msi_supports_masking = []() { return false; },
    .msi_mask_unmask = [](const msi_block_t*, uint, bool) {},
    .msi_alloc_block = [](uint, bool, bool, msi_block_t*) { return ZX_ERR_NOT_SUPPORTED; },
    .msi_free_block = [](msi_block_t*) {},
    .msi_register_handler = [](const msi_block_t*, uint, interrupt_handler_t) {}};

const struct pdev_interrupt_ops* intr_ops = &default_ops;

}  // anonymous namespace

zx_status_t register_int_handler(interrupt_vector_t vector, interrupt_handler_t handler) {
  return register_int_handler_common(vector, ktl::move(handler), false);
}

zx_status_t register_permanent_int_handler(interrupt_vector_t vector, interrupt_handler_t handler) {
  return register_int_handler_common(vector, ktl::move(handler), true);
}

bool pdev_invoke_int_if_present(interrupt_vector_t vector) {
  auto h = pdev_get_int_handler(vector);
  // Use a relaxed load as permanent handlers are never modified once set, and they are only set in
  // startup code, and so there is nothing to race with.
  if (h->permanent.load(ktl::memory_order_relaxed)) {
    // Once permanent is set to true we know that handler and arg are immutable and so it is safe
    // to read them without holding the lock.
    [&h]() TA_NO_THREAD_SAFETY_ANALYSIS {
      DEBUG_ASSERT(h->handler);
      h->handler();
    }();
    return true;
  }
  Guard<SpinLock, IrqSave> guard{pdev_lock::Get()};

  if (h->handler) {
    h->handler();
    return true;
  }
  return false;
}

zx_status_t mask_interrupt(interrupt_vector_t vector) { return intr_ops->mask(vector); }

zx_status_t unmask_interrupt(interrupt_vector_t vector) { return intr_ops->unmask(vector); }

zx_status_t deactivate_interrupt(interrupt_vector_t vector) { return intr_ops->deactivate(vector); }

zx_status_t configure_interrupt(interrupt_vector_t vector, enum interrupt_trigger_mode tm,
                                enum interrupt_polarity pol) {
  return intr_ops->configure(vector, tm, pol);
}

zx_status_t get_interrupt_config(interrupt_vector_t vector, enum interrupt_trigger_mode* tm,
                                 enum interrupt_polarity* pol) {
  return intr_ops->get_config(vector, tm, pol);
}

zx_status_t set_interrupt_affinity(interrupt_vector_t vector, cpu_mask_t mask) {
  return intr_ops->set_affinity(vector, mask);
}

uint32_t interrupt_get_base_vector() { return intr_ops->get_base_vector(); }

uint32_t interrupt_get_max_vector() { return intr_ops->get_max_vector(); }

bool is_valid_interrupt(interrupt_vector_t vector, uint32_t flags) {
  return intr_ops->is_valid(vector, flags);
}

interrupt_vector_t remap_interrupt(interrupt_vector_t vector) { return intr_ops->remap(vector); }

zx_status_t interrupt_send_ipi(cpu_mask_t target, mp_ipi ipi) {
  return intr_ops->send_ipi(target, ipi);
}

void interrupt_init_percpu_early() { intr_ops->init_percpu_early(); }
void interrupt_init_percpu() { intr_ops->init_percpu(); }

void platform_irq(iframe_t* frame) { intr_ops->handle_irq(frame); }

void pdev_register_interrupts(const struct pdev_interrupt_ops* ops) {
  // Assert that all of the ops are fulled in with at least a default hook.
  DEBUG_ASSERT(ops->mask);
  DEBUG_ASSERT(ops->unmask);
  DEBUG_ASSERT(ops->deactivate);
  DEBUG_ASSERT(ops->configure);
  DEBUG_ASSERT(ops->get_config);
  DEBUG_ASSERT(ops->set_affinity);
  DEBUG_ASSERT(ops->is_valid);
  DEBUG_ASSERT(ops->get_base_vector);
  DEBUG_ASSERT(ops->get_max_vector);
  DEBUG_ASSERT(ops->remap);
  DEBUG_ASSERT(ops->send_ipi);
  DEBUG_ASSERT(ops->init_percpu_early);
  DEBUG_ASSERT(ops->init_percpu);
  DEBUG_ASSERT(ops->handle_irq);
  DEBUG_ASSERT(ops->shutdown);
  DEBUG_ASSERT(ops->shutdown_cpu);
  DEBUG_ASSERT(ops->suspend_cpu);
  DEBUG_ASSERT(ops->resume_cpu);
  DEBUG_ASSERT(ops->msi_is_supported);
  DEBUG_ASSERT(ops->msi_supports_masking);
  DEBUG_ASSERT(ops->msi_mask_unmask);
  DEBUG_ASSERT(ops->msi_alloc_block);
  DEBUG_ASSERT(ops->msi_free_block);
  DEBUG_ASSERT(ops->msi_register_handler);

  intr_ops = ops;
  arch::ThreadMemoryBarrier();
}

void shutdown_interrupts() { intr_ops->shutdown(); }

void shutdown_interrupts_curr_cpu() { intr_ops->shutdown_cpu(); }

zx_status_t suspend_interrupts_curr_cpu() { return intr_ops->suspend_cpu(); }

zx_status_t resume_interrupts_curr_cpu() { return intr_ops->resume_cpu(); }

bool msi_is_supported() { return intr_ops->msi_is_supported(); }

bool msi_supports_masking() { return intr_ops->msi_supports_masking(); }

void msi_mask_unmask(const msi_block_t* block, uint msi_id, bool mask) {
  intr_ops->msi_mask_unmask(block, msi_id, mask);
}

zx_status_t msi_alloc_block(uint requested_irqs, bool can_target_64bit, bool is_msix,
                            msi_block_t* out_block) {
  return intr_ops->msi_alloc_block(requested_irqs, can_target_64bit, is_msix, out_block);
}

void msi_free_block(msi_block_t* block) { intr_ops->msi_free_block(block); }

void msi_register_handler(const msi_block_t* block, uint msi_id, interrupt_handler_t handler) {
  intr_ops->msi_register_handler(block, msi_id, ktl::move(handler));
}

namespace {

void interrupt_init_percpu_early_hook(uint level) { interrupt_init_percpu_early(); }

LK_INIT_HOOK_FLAGS(interrupt_init_percpu_early, interrupt_init_percpu_early_hook,
                   LK_INIT_LEVEL_PLATFORM_EARLY, LK_INIT_FLAG_SECONDARY_CPUS)

//
// Console support
//
class IrqInfo {
 public:
#if __arm__ || __aarch64__
  // TODO(johngro): This is not the best of assumptions, but for now, we want
  // to avoid querying the status of GIC PPIs as they are all CPU dependent,
  // and we don't provide any way to control which CPU we are on when
  // querying.
  static constexpr uint32_t kFirstValidIrq = 32;
#else
  static constexpr uint32_t kFirstValidIrq = 0;
#endif
  static constexpr uint32_t kLastValidIrq = MAX_INTERRUPTS;

  static bool ValidIrqNum(uint64_t irq_num) {
    return (irq_num >= kFirstValidIrq) && (irq_num < kLastValidIrq);
  }

  static IrqInfo Get(uint32_t irq_num) {
    DEBUG_ASSERT_MSG(ValidIrqNum(irq_num), "Bad IRQ num %u\n", irq_num);
    IrqInfo ret{};

    interrupt_trigger_mode tm;
    interrupt_polarity p;
    if (intr_ops->get_config(irq_num, &tm, &p) == ZX_OK) {
      ret.trigger_mode_ = tm;
      ret.polarity_ = p;
    }

    bool pend, enb;
    if (intr_ops->get_status && (intr_ops->get_status(irq_num, pend, enb) == ZX_OK)) {
      ret.pending_ = pend;
      ret.enabled_ = enb;
    }

    Guard<SpinLock, IrqSave> guard{pdev_lock::Get()};
    ret.registered_ = static_cast<bool>(int_handler_table[irq_num].handler);
    return ret;
  }

  bool registered() const { return registered_; }
  ktl::optional<interrupt_trigger_mode> trigger_mode() const { return trigger_mode_; }
  ktl::optional<interrupt_polarity> polarity() const { return polarity_; }
  ktl::optional<bool> pending() const { return pending_; }
  ktl::optional<bool> enabled() const { return enabled_; }

  // If the actual interrupt driver cannot tell us our configuration, we
  // consider this to be an "invalid" interrupt, and don't display it (even when
  // -v is passed to "show all").
  bool is_valid() const { return trigger_mode_.has_value() && polarity_.has_value(); }

  const char* trigger_mode_str() const {
    if (!trigger_mode_.has_value()) {
      return "Unknown";
    }

    switch (trigger_mode_.value()) {
      case interrupt_trigger_mode::EDGE:
        return "Edge";
      case interrupt_trigger_mode::LEVEL:
        return "Level";
      default:
        return "Unknown";
    }
  }

  const char* polarity_str() const {
    if (!trigger_mode_.has_value() || !polarity_.has_value()) {
      return "Unknown";
    }

    switch (polarity_.value()) {
      case interrupt_polarity::HIGH:
        return trigger_mode_.value() == interrupt_trigger_mode::LEVEL ? "High" : "Rising";
      case interrupt_polarity::LOW:
        return trigger_mode_.value() == interrupt_trigger_mode::LEVEL ? "Low" : "Falling";
      default:
        return "Unknown";
    }
  }

  const char* pending_str() const { return TriStateBoolStr(pending_); }
  const char* enabled_str() const { return TriStateBoolStr(enabled_); }

 private:
  static const char* TriStateBoolStr(ktl::optional<bool> val) {
    if (val.has_value()) {
      return val.value() ? "True" : "False";
    }
    return "Unknown";
  }

  bool registered_{false};
  ktl::optional<interrupt_trigger_mode> trigger_mode_;
  ktl::optional<interrupt_polarity> polarity_;
  ktl::optional<bool> pending_;
  ktl::optional<bool> enabled_;
};

int IrqConsoleCmd(int argc, const cmd_args* argv, uint32_t flags) {
  auto usage = [](int ret = ZX_ERR_INVALID_ARGS) -> int {
    printf("Usage: irq <cmd>\n");
    printf("Valid commands are:\n");
    printf("  help : show this message\n");
    printf("  show (all | reg | pending | enabled | <N>)\n");
    printf("    Shows the status of one or more IRQs, depending on the mandatory argument.\n");
    printf("      all     : Show the status of all IRQs in the system.\n");
    printf("      reg     : Show the status of all IRQs which have a registered handler.\n");
    printf("      pending : Show the status of all IRQs which are currently pending.\n");
    printf("      enabled : Show the status of all IRQs which are currently enabled.\n");
    printf("      pendenb : Show the status of all IRQs which are currently pending OR enabled.\n");
    printf("      <N>     : Show the status IRQ #<N>.\n");
    return ret;
  };

  if (argc < 2) {
    printf("No command specified!\n");
    return usage();
  }

  if (!strcmp(argv[1].str, "help")) {
    return usage(ZX_OK);
  } else if (!strcmp(argv[1].str, "show")) {
    if (argc < 3) {
      printf("No target specified for \"show\"\n");
      return usage();
    }

    ktl::optional<uint32_t> maybe_target;
    fit::inline_function<bool(const IrqInfo&), 0> predicate = [](const IrqInfo&) { return false; };

    if (!strcmp(argv[2].str, "all")) {
      predicate = [](const IrqInfo&) { return true; };
    } else if (!strcmp(argv[2].str, "reg")) {
      predicate = [](const IrqInfo& info) { return info.registered(); };
    } else if (!strcmp(argv[2].str, "pending")) {
      predicate = [](const IrqInfo& info) { return info.pending().value_or(false); };
    } else if (!strcmp(argv[2].str, "enabled")) {
      predicate = [](const IrqInfo& info) { return info.enabled().value_or(false); };
    } else if (!strcmp(argv[2].str, "pendenb")) {
      predicate = [](const IrqInfo& info) {
        return info.pending().value_or(false) || info.enabled().value_or(false);
      };
    } else {
      if (!IrqInfo::ValidIrqNum(argv[2].u)) {
        printf("Invalid IRQ target (\"%s\") for \"show\".\n", argv[2].str);
        return usage();
      }
      maybe_target = static_cast<uint32_t>(argv[2].u);
    }

    if (maybe_target.has_value()) {
      uint32_t target = maybe_target.value();
      const IrqInfo info = IrqInfo::Get(target);
      printf("IRQ          : %u\n", target);
      printf("State        : %s\n", info.registered() ? "Registered" : "Unregistered");
      printf("Trigger Mode : %s\n", info.trigger_mode_str());
      printf("Polarity     : %s\n", info.polarity_str());
      printf("Pending      : %s\n", info.pending_str());
      printf("Enabled      : %s\n", info.enabled_str());
    } else {
      printf("  ID  | Registered | Trigger Mode | Polarity | Pending | Enabled |\n");
      printf("------+------------+--------------+----------+---------+---------+\n");
      for (uint32_t i = IrqInfo::kFirstValidIrq; i < IrqInfo::kLastValidIrq; ++i) {
        if (const IrqInfo info = IrqInfo::Get(i); info.is_valid() && predicate(info)) {
          printf(" %4u |        %3s |      %7s |  %7s | %7s | %7s |\n", i,
                 info.registered() ? "Yes" : "No", info.trigger_mode_str(), info.polarity_str(),
                 info.pending_str(), info.enabled_str());
        }
      }
    }

    return ZX_OK;
  } else {
    printf("Unrecognized command \"%s\"\n", argv[1].str);
    return usage();
  }
}

}  // namespace

STATIC_COMMAND_START
STATIC_COMMAND("irq", "IRQ related commands", IrqConsoleCmd)
STATIC_COMMAND_END(smmu)
